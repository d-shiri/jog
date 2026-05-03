use anyhow::{Context, Result, anyhow};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::app::state::{AppState, DetailItem, TriggerPrompt, View, build_detail_items};
use crate::config::KeymapConfig;
use crate::config::Config;
use crate::history::History;
use crate::provider::github::{GitHubProvider, current_branch};
use crate::provider::{Provider, Run, RunDetail, Status, Workflow};

mod views;

pub enum AppEvent {
    RepoStatuses(Vec<Run>),
    WorkflowRunsPreviewLoaded(String, Vec<Run>),
    RunsLoaded(String, Vec<Run>),
    RunDetailLoaded(RunDetail),
    RunPreviewLoaded(u64, RunDetail),
    LogsLoaded(Vec<String>),
    Status(String),
    /// Like Status but also decrements the pending counter (used by async tasks that bumped it).
    TaskError(String),
    Quit,
}

pub struct TuiOpts {
    pub initial_view: View,
    pub focus_workflow: Option<String>,
}

pub async fn run(
    provider: Arc<GitHubProvider>,
    workflows: Vec<Workflow>,
    config: Config,
    opts: TuiOpts,
) -> Result<()> {
    let branch = current_branch().unwrap_or_else(|_| "?".into());
    let repo_label = format!("{}/{}", provider.repo().owner, provider.repo().repo);
    let history = History::load_for_repo(&provider.repo().owner, &provider.repo().repo);
    let mut state = AppState::new(
        repo_label,
        branch,
        sort_with_favorites(workflows, &config),
        config.keys.clone(),
        history,
    );

    if let Some(file) = opts.focus_workflow.as_deref() {
        if let Some(idx) = state
            .workflows
            .iter()
            .position(|w| w.file_name == file || w.name.eq_ignore_ascii_case(file))
        {
            state.workflow_cursor = idx;
        }
    }
    state.view = opts.initial_view;

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut state, provider.clone(), config).await;
    restore_terminal(&mut terminal).ok();
    result
}

fn sort_with_favorites(mut wfs: Vec<Workflow>, cfg: &Config) -> Vec<Workflow> {
    let prio: HashMap<&str, usize> = cfg
        .ui
        .favorites
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    wfs.sort_by_key(|w| {
        prio.get(w.file_name.as_str())
            .copied()
            .unwrap_or(usize::MAX / 2)
    });
    wfs
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(t: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(
        t.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("leave alternate screen")?;
    t.show_cursor().ok();
    Ok(())
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    provider: Arc<GitHubProvider>,
    config: Config,
) -> Result<()> {
    let km = resolve_keymap(&config.keys)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    spawn_repo_status_fetch(provider.clone(), tx.clone());
    if let Some(w) = state.selected_workflow().cloned() {
        spawn_fetch_workflow_preview(provider.clone(), w.file_name, tx.clone(), state);
    }

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut last_poll = tokio::time::Instant::now();
    let poll_interval = Duration::from_millis(config.ui.poll_interval_ms.max(1000));
    tick.tick().await;

    loop {
        if state.needs_clear {
            terminal.clear()?;
            state.needs_clear = false;
        }
        terminal.draw(|f| views::render(f, state))?;

        tokio::select! {
            maybe_evt = events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_evt
                    && key.kind == KeyEventKind::Press
                {
                    if let Some(action) = handle_key(state, key, &provider, &tx, &km).await {
                        match action {
                            AppEvent::Quit => return Ok(()),
                            _ => {}
                        }
                    }
                }
            }
            Some(app_evt) = rx.recv() => {
                match app_evt {
                    AppEvent::Quit => return Ok(()),
                    AppEvent::RepoStatuses(runs) => {
                        let mut seen = std::collections::HashSet::new();
                        for r in runs {
                            if let Some(file) = &r.workflow_file {
                                if seen.insert(file.clone()) {
                                    if let Some(w) = state.workflows.iter_mut().find(|w| &w.file_name == file) {
                                        w.last_status = Some(r.status);
                                        w.last_run_at = Some(r.updated_at);
                                    }
                                }
                            }
                        }
                    }
                    AppEvent::WorkflowRunsPreviewLoaded(file, runs) => {
                        if state.workflow_preview_file.as_deref() == Some(file.as_str()) {
                            state.workflow_preview_runs = runs;
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RunsLoaded(file, runs) => {
                        if state.workflow_for_runs.as_deref() == Some(file.as_str()) {
                            state.runs = runs;
                            state.run_cursor = 0;
                            if let Some(r) = state.runs.first().cloned() {
                                spawn_fetch_run_preview(provider.clone(), r.id, tx.clone(), state);
                            }
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RunDetailLoaded(detail) => {
                        if let Some(file) = state.workflow_for_runs.as_deref() {
                            state.history.record(file, &detail);
                        }
                        // Sound notification: play when a run we saw as Running finishes.
                        let id = detail.run.id;
                        if detail.run.status.is_terminal() {
                            if state.watch_seen_running.remove(&id) {
                                play_sound("/usr/share/sounds/freedesktop/stereo/complete.oga");
                            }
                        } else {
                            state.watch_seen_running.insert(id);
                        }
                        state.run_detail = Some(detail);
                        state.detail_cursor = 0;
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RunPreviewLoaded(run_id, detail) => {
                        if let Some(file) = state.workflow_for_runs.as_deref() {
                            state.history.record(file, &detail);
                        }
                        if state.runs_preview_id == Some(run_id) {
                            state.runs_preview = Some(detail);
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::LogsLoaded(lines) => {
                        state.clear_log_search();
                        state.log_sections = parse_log_sections(&lines);
                        state.log_raw = lines.clone();
                        if let Some((step_name, step_number)) = state.log_pending_section.take() {
                            let n = state.log_sections.len();
                            // Try exact name match first, then substring, then fall back to
                            // step_number-1 as the section index (GitHub's log has one ##[group]
                            // per step including hidden internal sub-steps, so step_number is a
                            // reliable 1-based index into the full section list).
                            let idx = state.log_sections.iter()
                                .position(|s| s.trim() == step_name.trim())
                                .or_else(|| state.log_sections.iter()
                                    .position(|s| s.contains(step_name.as_str()) || step_name.contains(s.as_str())))
                                .or_else(|| {
                                    let by_number = (step_number - 1) as usize;
                                    if by_number < n { Some(by_number) } else { None }
                                });
                            state.log_section_idx = idx;
                            state.log_lines = idx
                                .map(|i| extract_log_section(&lines, i))
                                .unwrap_or(lines);
                        } else {
                            state.log_section_idx = None;
                            state.log_lines = lines;
                        }
                        state.log_scroll = 0;
                        state.recompute_log_rendered();
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::Status(msg) => {
                        state.set_status(msg);
                    }
                    AppEvent::TaskError(msg) => {
                        state.set_status(msg);
                        state.pending = state.pending.saturating_sub(1);
                    }
                }
            }
            _ = tick.tick() => {
                state.tick_count += 1;
                if state.status_msg.is_some() && state.tick_count.saturating_sub(state.status_msg_tick) > 30 {
                    state.status_msg = None;
                }
                let now = tokio::time::Instant::now();
                if now.duration_since(last_poll) >= poll_interval {
                    last_poll = now;
                    if state.view == View::Watch && state.pending == 0 {
                        if let Some(file) = state.workflow_for_runs.clone() {
                            spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
                        }
                        if let Some(run) = state.runs.first().cloned() {
                            spawn_fetch_run_detail(provider.clone(), run.id, tx.clone(), state);
                        }
                    }
                }
            }
        }
    }
}

async fn handle_key(
    state: &mut AppState,
    key: KeyEvent,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    km: &Keymap,
) -> Option<AppEvent> {
    // ctrl+c always quits regardless of keymap
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(AppEvent::Quit);
    }
    // In trigger-prompt edit mode, route ALL keys to the text editor.
    if state.view == View::TriggerPrompt
        && state.trigger_prompt.as_ref().map(|p| p.editing).unwrap_or(false)
    {
        handle_trigger_prompt_edit(state, key);
        return None;
    }
    // While typing into the log search prompt, route every key to the editor.
    if state.view == View::Logs && state.log_search_input.is_some() {
        handle_log_search_input(state, key);
        return None;
    }
    // Esc clears an active log query before falling through to view-back.
    if state.view == View::Logs
        && key.code == KeyCode::Esc
        && state.log_search_query.is_some()
    {
        state.log_search_query = None;
        state.log_search_matches.clear();
        state.log_search_match_idx = None;
        return None;
    }

    // Global: quit
    if key_is(&key, km.quit) {
        return Some(AppEvent::Quit);
    }

    // Global: back — Esc is always accepted as a fallback regardless of config
    if key_is(&key, km.back) || key.code == KeyCode::Esc {
        match state.view {
            View::Workflows => return Some(AppEvent::Quit),
            View::Runs => {
                state.switch_view(View::Workflows);
                state.runs.clear();
                state.workflow_for_runs = None;
                state.runs_preview = None;
                state.runs_preview_id = None;
            }
            View::RunDetail | View::Watch => {
                state.switch_view(View::Runs);
                state.run_detail = None;
            }
            View::Logs => {
                state.switch_view(View::RunDetail);
                state.log_lines.clear();
            }
            View::Diff => {
                state.switch_view(View::RunDetail);
            }
            View::TriggerPrompt => cancel_trigger_prompt(state),
        }
        return None;
    }

    // Global: open current run in browser.
    // Resolution order: loaded run_detail → selected run in list → async-fetch latest for workflow.
    if key_is(&key, km.open_browser) {
        if let Some(url) = state.run_detail.as_ref().map(|d| d.run.url.clone())
            .or_else(|| state.runs.get(state.run_cursor).map(|r| r.url.clone()))
        {
            let _ = open::that(&url);
            state.set_status(format!("opened in browser"));
        } else if let Some(w) = state.selected_workflow().cloned() {
            // Workflows view: no run loaded yet, fetch the latest one.
            let p = provider.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                if let Ok(Some(run)) = p.get_latest_run(&w.file_name).await {
                    let _ = open::that(&run.url);
                    let _ = tx2.send(AppEvent::Status(format!("opened run {}", run.id)));
                }
            });
        }
        return None;
    }

    match state.view {
        View::Workflows => {
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                move_cursor(&mut state.workflow_cursor, state.workflows.len(), 1);
                maybe_fetch_workflow_preview(state, provider, tx);
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                move_cursor(&mut state.workflow_cursor, state.workflows.len(), -1);
                maybe_fetch_workflow_preview(state, provider, tx);
            } else if key_is(&key, km.confirm) || key.code == KeyCode::Enter {
                if let Some(w) = state.selected_workflow().cloned() {
                    state.switch_view(View::Runs);
                    state.workflow_for_runs = Some(w.file_name.clone());
                    state.runs.clear();
                    spawn_fetch_runs(provider.clone(), w.file_name, tx.clone(), state);
                }
            } else if key_is(&key, km.trigger) {
                trigger_workflow_at_cursor(state, provider, tx);
            } else if key_is(&key, km.watch) {
                if let Some(w) = state.selected_workflow().cloned() {
                    state.switch_view(View::Watch);
                    state.workflow_for_runs = Some(w.file_name.clone());
                    state.runs.clear();
                    spawn_fetch_runs(provider.clone(), w.file_name, tx.clone(), state);
                }
            }
        }
        View::Runs => {
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                move_cursor(&mut state.run_cursor, state.runs.len(), 1);
                maybe_fetch_preview(state, provider, tx);
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                move_cursor(&mut state.run_cursor, state.runs.len(), -1);
                maybe_fetch_preview(state, provider, tx);
            } else if key_is(&key, km.confirm) || key.code == KeyCode::Enter {
                if let Some(r) = state.selected_run().cloned() {
                    state.switch_view(View::RunDetail);
                    spawn_fetch_run_detail(provider.clone(), r.id, tx.clone(), state);
                }
            } else if key_is(&key, km.watch) {
                state.switch_view(View::Watch);
            } else if key_is(&key, km.trigger) {
                if let Some(file) = state.workflow_for_runs.clone() {
                    if let Some(w) = state.workflows.iter().find(|w| w.file_name == file).cloned() {
                        trigger_workflow(state, &w, provider, tx);
                    }
                }
            } else if key_is(&key, km.cancel_run) {
                if let Some(r) = state.selected_run().cloned() {
                    let p = provider.clone();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        let msg = match p.cancel(r.id).await {
                            Ok(_) => format!("cancelled run {}", r.id),
                            Err(e) => format!("cancel failed: {e}"),
                        };
                        let _ = tx2.send(AppEvent::Status(msg));
                    });
                }
            } else if key_is(&key, km.rerun) {
                if let Some(r) = state.selected_run().cloned() {
                    let p = provider.clone();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        let msg = match p.rerun(r.id).await {
                            Ok(_) => format!("rerunning all jobs for {}", r.id),
                            Err(e) => format!("rerun failed: {e}"),
                        };
                        let _ = tx2.send(AppEvent::Status(msg));
                    });
                }
            } else if key_is(&key, km.rerun_failed) {
                if let Some(r) = state.selected_run().cloned() {
                    let p = provider.clone();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        let msg = match p.rerun_failed(r.id).await {
                            Ok(_) => format!("rerunning failed jobs for {}", r.id),
                            Err(e) => format!("rerun-failed failed: {e}"),
                        };
                        let _ = tx2.send(AppEvent::Status(msg));
                    });
                }
            }
        }
        View::RunDetail => {
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                let max = state.run_detail.as_ref().map(|d| build_detail_items(d).len()).unwrap_or(0);
                move_cursor(&mut state.detail_cursor, max, 1);
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                let max = state.run_detail.as_ref().map(|d| build_detail_items(d).len()).unwrap_or(0);
                move_cursor(&mut state.detail_cursor, max, -1);
            } else if key_is(&key, km.diff) {
                state.switch_view(View::Diff);
            } else if key_is(&key, km.confirm) || key.code == KeyCode::Enter || key_is(&key, km.open_logs) {
                if let Some(detail) = &state.run_detail {
                    let items = build_detail_items(detail);
                    match items.get(state.detail_cursor).copied() {
                        Some(DetailItem::Job(ji)) => {
                            if let Some(job) = detail.jobs.get(ji).cloned() {
                                state.log_pending_section = None;
                                state.switch_view(View::Logs);
                                state.log_lines = vec!["loading...".into()];
                                spawn_fetch_logs(provider.clone(), job.id, tx.clone(), state);
                            }
                        }
                        Some(DetailItem::Step { job: ji, step: si }) => {
                            if let (Some(job), Some(step)) = (
                                detail.jobs.get(ji).cloned(),
                                detail.jobs.get(ji).and_then(|j| j.steps.get(si)).cloned(),
                            ) {
                                state.log_pending_section = Some((step.name.clone(), step.number));
                                state.switch_view(View::Logs);
                                state.log_lines = vec!["loading...".into()];
                                spawn_fetch_logs(provider.clone(), job.id, tx.clone(), state);
                            }
                        }
                        None => {}
                    }
                }
            }
        }
        View::Logs => {
            // Approx max scroll: line count - viewport height. This is a lower
            // bound on the true wrapped-line count, so the user can still
            // scroll a little past the perfect bottom on long wrapped lines,
            // but no longer scrolls forever into blank space.
            let max_scroll = (state.log_rendered.len() as u16)
                .saturating_sub(state.last_logs_viewport_height.get());
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                state.log_scroll = state.log_scroll.saturating_add(1).min(max_scroll);
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                state.log_scroll = state.log_scroll.saturating_sub(1);
            } else if key_is(&key, km.page_down) || key.code == KeyCode::PageDown {
                state.log_scroll = state.log_scroll.saturating_add(20).min(max_scroll);
            } else if key_is(&key, km.page_up) || key.code == KeyCode::PageUp {
                state.log_scroll = state.log_scroll.saturating_sub(20);
            } else if key_is(&key, km.scroll_top) {
                state.log_scroll = 0;
            } else if key_is(&key, km.scroll_bottom) {
                state.log_scroll = max_scroll;
            } else if key_is(&key, km.search) {
                state.log_search_input = Some(String::new());
            } else if key_is(&key, km.next_step) {
                if state.log_search_query.is_some() {
                    jump_log_match(state, 1);
                    state.recompute_log_rendered();
                } else {
                    let n = state.log_sections.len();
                    if n > 0 {
                        let next = state.log_section_idx.map(|i| (i + 1).min(n - 1)).unwrap_or(0);
                        state.log_section_idx = Some(next);
                        state.log_lines = extract_log_section(&state.log_raw, next);
                        state.log_scroll = 0;
                        state.clear_log_search();
                        state.recompute_log_rendered();
                    }
                }
            } else if key_is(&key, km.prev_step) {
                if state.log_search_query.is_some() {
                    jump_log_match(state, -1);
                    state.recompute_log_rendered();
                } else {
                    match state.log_section_idx {
                        None => {}
                        Some(0) => {
                            state.log_section_idx = None;
                            state.log_lines = state.log_raw.clone();
                            state.log_scroll = 0;
                            state.clear_log_search();
                            state.recompute_log_rendered();
                        }
                        Some(i) => {
                            let prev = i - 1;
                            state.log_section_idx = Some(prev);
                            state.log_lines = extract_log_section(&state.log_raw, prev);
                            state.log_scroll = 0;
                            state.clear_log_search();
                            state.recompute_log_rendered();
                        }
                    }
                }
            } else if key_is(&key, km.all_steps) {
                state.log_section_idx = None;
                state.log_lines = state.log_raw.clone();
                state.log_scroll = 0;
                state.clear_log_search();
                state.recompute_log_rendered();
            }
        }
        View::TriggerPrompt => {
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                if let Some(p) = state.trigger_prompt.as_mut() {
                    let len = p.fields.len();
                    move_cursor(&mut p.cursor, len, 1);
                }
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                if let Some(p) = state.trigger_prompt.as_mut() {
                    let len = p.fields.len();
                    move_cursor(&mut p.cursor, len, -1);
                }
            } else if key_is(&key, km.tp_cycle) {
                if let Some(p) = state.trigger_prompt.as_mut() {
                    p.cycle_option();
                }
            } else if key_is(&key, km.tp_yes) {
                if let Some(p) = state.trigger_prompt.as_mut() {
                    if let Some(f) = p.current_field_mut() {
                        if f.options.as_deref().map_or(false, |o| o.iter().any(|x| x == "yes")) {
                            f.value = "yes".to_string();
                        }
                    }
                }
            } else if key_is(&key, km.tp_no) {
                if let Some(p) = state.trigger_prompt.as_mut() {
                    if let Some(f) = p.current_field_mut() {
                        if f.options.as_deref().map_or(false, |o| o.iter().any(|x| x == "no")) {
                            f.value = "no".to_string();
                        }
                    }
                }
            } else if key_is(&key, km.confirm) || key.code == KeyCode::Enter || key_is(&key, km.tp_edit) {
                if let Some(p) = state.trigger_prompt.as_mut() {
                    if p.current_field().is_some() {
                        p.editing = true;
                    }
                }
            } else if key_is(&key, km.tp_submit) {
                submit_trigger_prompt(state, provider, tx);
            }
        }
        View::Watch => {}
        View::Diff => {}
    }
    None
}

fn handle_log_search_input(state: &mut AppState, key: KeyEvent) {
    let Some(buf) = state.log_search_input.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            state.log_search_input = None;
        }
        KeyCode::Enter => {
            let q = std::mem::take(buf);
            state.log_search_input = None;
            if q.is_empty() {
                state.log_search_query = None;
                state.log_search_matches.clear();
                state.log_search_match_idx = None;
                state.recompute_log_rendered();
                return;
            }
            state.log_search_query = Some(q);
            state.recompute_log_matches();
            state.recompute_log_rendered();
            scroll_to_current_match(state);
        }
        KeyCode::Backspace => {
            buf.pop();
        }
        KeyCode::Char(c) => {
            buf.push(c);
        }
        _ => {}
    }
}

fn jump_log_match(state: &mut AppState, dir: i32) {
    if state.log_search_matches.is_empty() {
        return;
    }
    let n = state.log_search_matches.len() as i32;
    let cur = state.log_search_match_idx.unwrap_or(0) as i32;
    let next = ((cur + dir) % n + n) % n;
    state.log_search_match_idx = Some(next as usize);
    scroll_to_current_match(state);
}

fn scroll_to_current_match(state: &mut AppState) {
    let Some(idx) = state.log_search_match_idx else { return };
    let Some(&src_line) = state.log_search_matches.get(idx) else { return };
    let viewport = state.last_logs_viewport_height.get().max(1);

    // Mirror render expansion: each `##[group]`/`##[section]` source line
    // becomes 3 rendered lines (separator, header, blank). All other prefixes
    // are 1:1. Without this correction the scroll target lands above the
    // actual match by 2 rows per preceding group.
    let render_offset_through = |upto: usize| -> u32 {
        let mut total: u32 = 0;
        for line in state.log_lines.iter().take(upto) {
            let content = strip_time_prefix(line);
            if content.starts_with("##[group]") || content.starts_with("##[section]") {
                total += 3;
            } else {
                total += 1;
            }
        }
        total
    };

    let target_render = render_offset_through(src_line);
    let total_render = render_offset_through(state.log_lines.len());
    let target = (target_render as u16).saturating_sub(viewport / 3);
    let max = (total_render as u16).saturating_sub(viewport);
    state.log_scroll = target.min(max);
}

fn handle_trigger_prompt_edit(state: &mut AppState, key: KeyEvent) {
    let Some(prompt) = state.trigger_prompt.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Enter | KeyCode::Esc => {
            prompt.editing = false;
        }
        KeyCode::Backspace => {
            if let Some(f) = prompt.current_field_mut() {
                f.value.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some(f) = prompt.current_field_mut() {
                f.value.push(c);
            }
        }
        _ => {}
    }
}

fn move_cursor(cursor: &mut usize, len: usize, delta: i32) {
    if len == 0 {
        *cursor = 0;
        return;
    }
    let last = len - 1;
    let new = (*cursor as i32 + delta).clamp(0, last as i32);
    *cursor = new as usize;
}

struct Keymap {
    quit: (KeyCode, KeyModifiers),
    back: (KeyCode, KeyModifiers),
    down: (KeyCode, KeyModifiers),
    up: (KeyCode, KeyModifiers),
    confirm: (KeyCode, KeyModifiers),
    open_logs: (KeyCode, KeyModifiers),
    page_down: (KeyCode, KeyModifiers),
    page_up: (KeyCode, KeyModifiers),
    scroll_top: (KeyCode, KeyModifiers),
    scroll_bottom: (KeyCode, KeyModifiers),
    next_step: (KeyCode, KeyModifiers),
    prev_step: (KeyCode, KeyModifiers),
    all_steps: (KeyCode, KeyModifiers),
    search: (KeyCode, KeyModifiers),
    trigger: (KeyCode, KeyModifiers),
    watch: (KeyCode, KeyModifiers),
    open_browser: (KeyCode, KeyModifiers),
    cancel_run: (KeyCode, KeyModifiers),
    rerun: (KeyCode, KeyModifiers),
    rerun_failed: (KeyCode, KeyModifiers),
    diff: (KeyCode, KeyModifiers),
    tp_edit: (KeyCode, KeyModifiers),
    tp_submit: (KeyCode, KeyModifiers),
    tp_yes: (KeyCode, KeyModifiers),
    tp_no: (KeyCode, KeyModifiers),
    tp_cycle: (KeyCode, KeyModifiers),
}

fn parse_key(s: &str) -> Result<(KeyCode, KeyModifiers)> {
    let s = s.trim();
    let (mods, key_str) = if let Some((prefix, k)) = s.rsplit_once('+') {
        let mods = prefix.split('+').try_fold(KeyModifiers::NONE, |acc, m| {
            Ok(acc | match m.to_lowercase().as_str() {
                "ctrl" => KeyModifiers::CONTROL,
                "shift" => KeyModifiers::SHIFT,
                "alt" => KeyModifiers::ALT,
                other => return Err(anyhow!("unknown modifier `{other}` in key `{s}`")),
            })
        })?;
        (mods, k)
    } else {
        (KeyModifiers::NONE, s)
    };
    let code = match key_str {
        "Enter" => KeyCode::Enter,
        "Esc" | "Escape" => KeyCode::Esc,
        "Backspace" => KeyCode::Backspace,
        "Tab" => KeyCode::Tab,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "Delete" => KeyCode::Delete,
        "Space" => KeyCode::Char(' '),
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next().unwrap()),
        _ => return Err(anyhow!("unknown key `{key_str}` in config")),
    };
    Ok((code, mods))
}

fn key_is(event: &KeyEvent, (code, mods): (KeyCode, KeyModifiers)) -> bool {
    event.code == code && event.modifiers == mods
}

fn resolve_keymap(cfg: &KeymapConfig) -> Result<Keymap> {
    Ok(Keymap {
        quit:          parse_key(&cfg.quit)?,
        back:          parse_key(&cfg.back)?,
        down:          parse_key(&cfg.down)?,
        up:            parse_key(&cfg.up)?,
        confirm:       parse_key(&cfg.confirm)?,
        open_logs:     parse_key(&cfg.open_logs)?,
        page_down:     parse_key(&cfg.page_down)?,
        page_up:       parse_key(&cfg.page_up)?,
        scroll_top:    parse_key(&cfg.scroll_top)?,
        scroll_bottom: parse_key(&cfg.scroll_bottom)?,
        next_step:     parse_key(&cfg.next_step)?,
        prev_step:     parse_key(&cfg.prev_step)?,
        all_steps:     parse_key(&cfg.all_steps)?,
        search:        parse_key(&cfg.search)?,
        trigger:       parse_key(&cfg.trigger)?,
        watch:         parse_key(&cfg.watch)?,
        open_browser:  parse_key(&cfg.open_browser)?,
        cancel_run:    parse_key(&cfg.cancel_run)?,
        rerun:         parse_key(&cfg.rerun)?,
        rerun_failed:  parse_key(&cfg.rerun_failed)?,
        diff:          parse_key(&cfg.diff)?,
        tp_edit:       parse_key(&cfg.tp_edit)?,
        tp_submit:     parse_key(&cfg.tp_submit)?,
        tp_yes:        parse_key(&cfg.tp_yes)?,
        tp_no:         parse_key(&cfg.tp_no)?,
        tp_cycle:      parse_key(&cfg.tp_cycle)?,
    })
}

/// Strip the `HH:MM:SS ` prefix added by `clean_log_line`, returning the raw content.
fn strip_time_prefix(s: &str) -> &str {
    if s.len() > 9
        && s.as_bytes().get(2) == Some(&b':')
        && s.as_bytes().get(5) == Some(&b':')
        && s.as_bytes().get(8) == Some(&b' ')
        && s[..2].bytes().all(|b| b.is_ascii_digit())
        && s[3..5].bytes().all(|b| b.is_ascii_digit())
        && s[6..8].bytes().all(|b| b.is_ascii_digit())
    {
        &s[9..]
    } else {
        s
    }
}

/// Strip CSI sequences (`\x1b[…<final-byte>`) so titles/comparisons see clean text.
/// Mirrors the escape-skipping shape of `views::ansi_line_to_spans`.
fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '[' {
            let mut j = i + 2;
            while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            // Skip the final byte too (the alphabetic terminator).
            i = if j < chars.len() { j + 1 } else { j };
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn parse_log_sections(raw: &[String]) -> Vec<String> {
    let mut depth: usize = 0;
    let mut result = Vec::new();
    for line in raw {
        let content = strip_time_prefix(line);
        let is_group = content.starts_with("##[group]") || content.starts_with("##[section]");
        let is_endgroup = content.starts_with("##[endgroup]");
        if is_group {
            if depth == 0 {
                result.push(strip_ansi(
                    content.strip_prefix("##[group]")
                        .or_else(|| content.strip_prefix("##[section]"))
                        .unwrap_or("")
                ));
            }
            depth += 1;
        } else if is_endgroup {
            depth = depth.saturating_sub(1);
        }
    }
    result
}

fn extract_log_section(raw: &[String], section_idx: usize) -> Vec<String> {
    // Track depth so a nested `##[group]` inside a section doesn't get treated
    // as the next top-level boundary. Section indices count only top-level groups.
    let mut current = 0usize;
    let mut depth: usize = 0;
    let mut capturing = false;
    let mut result = Vec::new();
    for line in raw {
        let content = strip_time_prefix(line);
        let is_group = content.starts_with("##[group]") || content.starts_with("##[section]");
        let is_endgroup = content.starts_with("##[endgroup]");
        if is_group {
            if depth == 0 {
                if current == section_idx {
                    capturing = true;
                    result.push(line.clone());
                    depth += 1;
                    current += 1;
                    continue;
                } else if capturing {
                    break;
                }
                current += 1;
            } else if capturing {
                result.push(line.clone());
            }
            depth += 1;
        } else if is_endgroup {
            if capturing {
                result.push(line.clone());
                if depth <= 1 {
                    break;
                }
            }
            depth = depth.saturating_sub(1);
        } else if capturing {
            result.push(line.clone());
        }
    }
    result
}

fn spawn_repo_status_fetch(
    provider: Arc<GitHubProvider>,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        if let Ok(runs) = provider.list_repo_runs(50).await {
            let _ = tx.send(AppEvent::RepoStatuses(runs));
        }
    });
}

fn spawn_fetch_runs(
    provider: Arc<GitHubProvider>,
    file: String,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    state.pending += 1;
    tokio::spawn(async move {
        let runs = provider.list_runs(&file, 20).await.unwrap_or_default();
        let _ = tx.send(AppEvent::RunsLoaded(file, runs));
    });
}

fn maybe_fetch_workflow_preview(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if let Some(w) = state.selected_workflow().cloned() {
        if state.workflow_preview_file.as_deref() != Some(w.file_name.as_str()) {
            spawn_fetch_workflow_preview(provider.clone(), w.file_name, tx.clone(), state);
        }
    }
}

fn spawn_fetch_workflow_preview(
    provider: Arc<GitHubProvider>,
    file: String,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    state.workflow_preview_file = Some(file.clone());
    state.workflow_preview_runs.clear();
    state.pending += 1;
    tokio::spawn(async move {
        let runs = provider.list_runs(&file, 10).await.unwrap_or_default();
        let _ = tx.send(AppEvent::WorkflowRunsPreviewLoaded(file, runs));
    });
}

fn maybe_fetch_preview(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if let Some(r) = state.selected_run().cloned() {
        if state.runs_preview_id != Some(r.id) {
            spawn_fetch_run_preview(provider.clone(), r.id, tx.clone(), state);
        }
    }
}

fn spawn_fetch_run_preview(
    provider: Arc<GitHubProvider>,
    run_id: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    state.runs_preview_id = Some(run_id);
    state.runs_preview = None;
    state.pending += 1;
    tokio::spawn(async move {
        match provider.get_run(run_id).await {
            Ok(detail) => { let _ = tx.send(AppEvent::RunPreviewLoaded(run_id, detail)); }
            Err(e) => { let _ = tx.send(AppEvent::TaskError(format!("preview: {e}"))); }
        }
    });
}

fn spawn_fetch_run_detail(
    provider: Arc<GitHubProvider>,
    run_id: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    state.pending += 1;
    tokio::spawn(async move {
        match provider.get_run(run_id).await {
            Ok(detail) => {
                let _ = tx.send(AppEvent::RunDetailLoaded(detail));
            }
            Err(e) => {
                let _ = tx.send(AppEvent::TaskError(format!("get run failed: {e}")));
            }
        }
    });
}

fn spawn_fetch_logs(
    provider: Arc<GitHubProvider>,
    job_id: u64,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    state.pending += 1;
    tokio::spawn(async move {
        match provider.stream_logs(job_id).await {
            Ok(mut s) => {
                let mut lines = Vec::new();
                while let Some(chunk) = s.next().await {
                    if let Ok(c) = chunk {
                        lines.push(c.line);
                    }
                }
                let _ = tx.send(AppEvent::LogsLoaded(lines));
            }
            Err(e) => {
                let _ = tx.send(AppEvent::TaskError(format!("logs failed: {e}")));
            }
        }
    });
}

fn trigger_workflow_at_cursor(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(w) = state.selected_workflow().cloned() else {
        return;
    };
    trigger_workflow(state, &w, provider, tx);
}

fn trigger_workflow(
    state: &mut AppState,
    workflow: &Workflow,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if !workflow.triggerable {
        state.set_status(format!(
            "`{}` has no workflow_dispatch trigger",
            workflow.name
        ));
        return;
    }
    if !workflow.inputs.is_empty() {
        let return_view = state.view;
        state.trigger_prompt = Some(TriggerPrompt::from_workflow(workflow, return_view));
        state.switch_view(View::TriggerPrompt);
        return;
    }
    dispatch_trigger(
        state,
        &workflow.file_name,
        &workflow.name,
        HashMap::new(),
        provider,
        tx,
    );
}

fn dispatch_trigger(
    state: &mut AppState,
    workflow_file: &str,
    workflow_name: &str,
    inputs: HashMap<String, String>,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let file = workflow_file.to_string();
    let r = state.current_branch.clone();
    let p = provider.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let msg = match p.trigger(&file, &r, inputs).await {
            Ok(_) => format!("triggered {} on {}", file, r),
            Err(e) => format!("trigger failed: {}", e),
        };
        let _ = tx2.send(AppEvent::Status(msg));
    });
    state.set_status(format!(
        "triggering {} on {}",
        workflow_name, state.current_branch
    ));
}

fn submit_trigger_prompt(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(prompt) = state.trigger_prompt.as_ref() else {
        return;
    };
    let missing = prompt.missing_required();
    if !missing.is_empty() {
        state.set_status(format!("missing required: {}", missing.join(", ")));
        return;
    }
    let inputs = prompt.collected();
    let file = prompt.workflow_file.clone();
    let name = prompt.workflow_name.clone();
    let return_view = prompt.return_view;
    state.trigger_prompt = None;
    state.switch_view(return_view);
    dispatch_trigger(state, &file, &name, inputs, provider, tx);
}

fn cancel_trigger_prompt(state: &mut AppState) {
    let return_view = state
        .trigger_prompt
        .as_ref()
        .map(|p| p.return_view)
        .unwrap_or(View::Workflows);
    state.trigger_prompt = None;
    state.switch_view(return_view);
}

pub fn status_glyph(s: Status) -> &'static str {
    match s {
        Status::Success => "✓",
        Status::Failure => "✗",
        Status::Running => "⏵",
        Status::Queued => "•",
        Status::Cancelled => "⊘",
        Status::Skipped => "↷",
        Status::Unknown => "?",
    }
}

/// Play a sound file non-blocking. Tries `paplay` (PulseAudio/PipeWire),
/// falls back to `pw-play`, then silently gives up.
fn play_sound(path: &str) {
    let path = path.to_string();
    std::thread::spawn(move || {
        if std::process::Command::new("paplay").arg(&path).spawn().is_err() {
            let _ = std::process::Command::new("pw-play").arg(&path).spawn();
        }
    });
}

pub fn animated_glyph(s: Status, tick: u64) -> &'static str {
    if s == Status::Running {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[(tick % 10) as usize]
    } else {
        status_glyph(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\x1b[36;1mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn parse_log_sections_strips_ansi_from_titles() {
        let raw = vec![
            "08:12:34 ##[group]\x1b[36;1mRun npm test\x1b[0m".to_string(),
            "08:12:35 some content".to_string(),
            "08:12:36 ##[endgroup]".to_string(),
        ];
        assert_eq!(parse_log_sections(&raw), vec!["Run npm test".to_string()]);
    }

    #[test]
    fn parse_log_sections_skips_nested_groups() {
        let raw = vec![
            "##[group]Step 1".to_string(),
            "  ##[group]Nested".to_string(),
            "  ##[endgroup]".to_string(),
            "##[endgroup]".to_string(),
            "##[group]Step 2".to_string(),
            "##[endgroup]".to_string(),
        ];
        assert_eq!(
            parse_log_sections(&raw),
            vec!["Step 1".to_string(), "Step 2".to_string()]
        );
    }

    #[test]
    fn extract_log_section_handles_nested_groups() {
        // Two top-level sections; the first contains a nested group.
        let raw = vec![
            "##[group]Outer A".to_string(),
            "  step 1".to_string(),
            "##[group]Inner".to_string(),
            "    nested step".to_string(),
            "##[endgroup]".to_string(),
            "  step 2".to_string(),
            "##[endgroup]".to_string(),
            "##[group]Outer B".to_string(),
            "  other".to_string(),
            "##[endgroup]".to_string(),
        ];
        // section 0 should contain everything between the first outer group
        // and its matching endgroup, including the nested group.
        let s0 = extract_log_section(&raw, 0);
        assert!(s0.iter().any(|l| l.contains("Outer A")));
        assert!(s0.iter().any(|l| l.contains("Inner")));
        assert!(s0.iter().any(|l| l.contains("nested step")));
        assert!(s0.iter().any(|l| l.contains("step 2")));
        assert!(!s0.iter().any(|l| l.contains("Outer B")));

        // section 1 should be Outer B (not the nested Inner).
        let s1 = extract_log_section(&raw, 1);
        assert!(s1.iter().any(|l| l.contains("Outer B")));
        assert!(s1.iter().any(|l| l.contains("other")));
        assert!(!s1.iter().any(|l| l.contains("Outer A")));
    }
}
