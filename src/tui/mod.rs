use anyhow::{Context, Result, anyhow};
use chrono::Timelike;
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

use rayon::prelude::*;

use crate::app::state::{
    AppState, DetailItem, Finder, FinderKind, RepoCard, TriggerPrompt, View, build_detail_items,
};
use crate::config::KeymapConfig;
use crate::config::{Config, NotifyMode};
use crate::history::History;
use crate::provider::github::{GitHubProvider, RepoSpec, current_branch};
use crate::provider::{Provider, Run, RunDetail, Status, Step, Workflow};

mod views;

pub enum AppEvent {
    RepoStatuses(Vec<Run>),
    WorkflowRunsPreviewLoaded(String, Vec<Run>),
    RunsLoaded(String, Vec<Run>),
    RunDetailLoaded(RunDetail),
    RunPreviewLoaded(u64, RunDetail),
    LogsLoaded(Vec<String>),
    /// Recent runs for one row of the multi-repo dashboard.
    RepoCardLoaded(String, Vec<Run>),
    /// A dashboard row could not be fetched (typo, no access, network).
    RepoCardFailed(String, String),
    /// Working-tree state for a local checkout.
    GitStatusLoaded(String, crate::git::RepoStatus),
    /// A git command finished. `Some(msg)` reports success, `Err` the failure;
    /// either way the status is refreshed afterwards.
    GitOpDone(String, Result<String, String>),
    /// The active repo changed: new label, default branch, and workflow list.
    RepoSwitched {
        label: String,
        branch: String,
        workflows: Vec<Workflow>,
    },
    Status(String),
    /// Like Status but also decrements the pending counter (used by async tasks that bumped it).
    TaskError(String),
    Quit,
}

pub struct TuiOpts {
    pub initial_view: View,
    pub focus_workflow: Option<String>,
    /// Local checkouts found by scanning a workspace directory. Empty when
    /// running inside a single repo.
    pub workspace: Vec<std::path::PathBuf>,
    /// The directory that scan was rooted at, for display.
    pub workspace_root: Option<std::path::PathBuf>,
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

    state.log_focus_context = config.ui.log_focus_context;
    state.workspace_root = opts.workspace_root.clone();
    state.repos = if opts.workspace.is_empty() {
        dashboard_repos(&config, &state.repo_label)
    } else {
        workspace_repos(&opts.workspace, &config)
    };

    if let Some(file) = opts.focus_workflow.as_deref()
        && let Some(idx) = state
            .workflows
            .iter()
            .position(|w| w.file_name == file || w.name.eq_ignore_ascii_case(file))
        {
            state.workflow_cursor = idx;
        }
    state.view = opts.initial_view;
    if state.view == View::Repos {
        // Start the cursor on the repo we were launched in, if it's listed.
        if let Some(i) = state.repos.iter().position(|r| r.spec == state.repo_label) {
            state.repo_cursor = i;
        }
    }

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut state, provider.clone(), config).await;
    restore_terminal(&mut terminal).ok();
    result
}

/// Rows for the multi-repo dashboard: everything in `[provider] repos`, with the
/// currently active repo prepended if it isn't already listed. Duplicates are
/// dropped so listing the active repo explicitly is harmless.
fn dashboard_repos(cfg: &Config, active: &str) -> Vec<RepoCard> {
    let mut specs: Vec<String> = Vec::new();
    if !active.is_empty() {
        specs.push(active.to_string());
    }
    for r in &cfg.provider.repos {
        let r = r.trim();
        if !r.is_empty() && !specs.iter().any(|s| s == r) {
            specs.push(r.to_string());
        }
    }
    specs.into_iter().map(RepoCard::new).collect()
}

/// Dashboard rows for a scanned workspace: one per discovered checkout, plus any
/// remote-only repos from `[provider] repos` that aren't already checked out.
///
/// A checkout without a GitHub origin still gets a row — you can review and
/// commit its changes, there's just no CI to show.
fn workspace_repos(paths: &[std::path::PathBuf], cfg: &Config) -> Vec<RepoCard> {
    // One `git remote get-url` process per repo, run in parallel: they're
    // independent and this happens before the first frame is drawn, so serially
    // spawning a dozen processes is startup latency the user sees.
    let mut cards: Vec<RepoCard> = paths
        .par_iter()
        .map(|p| {
            let remote = crate::git::remote_url(p)
                .ok()
                .and_then(|url| crate::provider::github::parse_remote_url(&url).ok())
                .map(|s| format!("{}/{}", s.owner, s.repo));
            RepoCard::local(p.clone(), remote)
        })
        .collect();
    // `spec` is the key every async event is routed by, so it has to be unique.
    // Two grouped checkouts can share a basename (`web/app` and `mobile/app`);
    // qualify the later one with its parent directory rather than letting both
    // rows collide on "app".
    let mut seen: Vec<String> = Vec::new();
    for card in &mut cards {
        if !seen.contains(&card.spec) {
            seen.push(card.spec.clone());
            continue;
        }
        let qualified = card
            .path
            .as_ref()
            .and_then(|p| {
                let parent = p.parent()?.file_name()?.to_string_lossy().into_owned();
                Some(format!("{parent}/{}", card.spec))
            })
            .unwrap_or_else(|| format!("{} ({})", card.spec, seen.len()));
        card.spec = qualified;
        seen.push(card.spec.clone());
    }

    for extra in &cfg.provider.repos {
        let extra = extra.trim();
        if extra.is_empty() || cards.iter().any(|c| c.remote.as_deref() == Some(extra)) {
            continue;
        }
        cards.push(RepoCard::new(extra.to_string()));
    }
    cards
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
    // Rebound on repo switch from the dashboard, so it can't be a parameter binding.
    let mut provider = provider;
    let km = resolve_keymap(&config.keys)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    if state.view == View::Repos {
        spawn_fetch_repo_cards(&provider, state, &tx);
    } else {
        spawn_repo_status_fetch(provider.clone(), tx.clone());
        if let Some(w) = state.selected_workflow().cloned() {
            spawn_fetch_workflow_preview(provider.clone(), w.file_name, tx.clone(), state);
        }
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
                    && let Some(action) = handle_key(state, key, &mut provider, &tx, &km).await
                    && let AppEvent::Quit = action { return Ok(()) }
            }
            Some(app_evt) = rx.recv() => {
                match app_evt {
                    AppEvent::Quit => return Ok(()),
                    AppEvent::RepoStatuses(runs) => {
                        // Runs are newest-first, so the first hit per workflow wins.
                        // The API's run list doesn't carry the workflow filename, so
                        // fall back to matching the run's name against the workflow's
                        // display name — that's the same string GitHub reports for both.
                        let mut seen = std::collections::HashSet::new();
                        for r in runs {
                            let slot = match &r.workflow_file {
                                Some(file) => state
                                    .workflows
                                    .iter_mut()
                                    .find(|w| &w.file_name == file),
                                None => state
                                    .workflows
                                    .iter_mut()
                                    .find(|w| w.name == r.display_title),
                            };
                            if let Some(w) = slot
                                && seen.insert(w.file_name.clone())
                            {
                                w.last_status = Some(r.status);
                                w.last_run_at = Some(r.updated_at);
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
                            // Sound notification: play when a run we saw as Running
                            // finishes, even if we're only viewing the runs list.
                            // Shares `watch_seen_running` with RunDetailLoaded, so the
                            // `remove` dedupes and avoids a double-ding in Watch view.
                            let label = state.repo_label.clone();
                            for r in &runs {
                                announce_if_finished(state, r, &label, &config);
                            }
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
                        // Sound + desktop notification when a run we saw as Running finishes.
                        let label = state.repo_label.clone();
                        announce_if_finished(state, &detail.run, &label, &config);
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
                        let job_steps: Vec<Step> = state.log_job_idx
                            .and_then(|ji| state.run_detail.as_ref()?.jobs.get(ji))
                            .map(|j| j.steps.clone())
                            .unwrap_or_default();
                        state.log_sections = parse_log_sections(&lines);
                        let (step_line_starts, step_names) =
                            compute_step_line_starts(&lines, &job_steps);
                        state.log_step_line_starts = step_line_starts;
                        state.log_step_names = step_names;
                        state.log_raw = lines.clone();
                        if let Some((step_name, started_hms, _completed_hms)) = state.log_pending_section.take() {
                            if !state.log_step_names.is_empty() {
                                // Find the API step by name, then extract all lines for it.
                                let needle = step_name.trim().to_lowercase();
                                let step_idx = state.log_step_names.iter()
                                    .position(|s| s.trim().to_lowercase() == needle)
                                    .or_else(|| {
                                        started_hms.as_deref().map(|hms| {
                                            let target = find_raw_line_for_time(&lines, hms);
                                            state.log_step_line_starts.iter()
                                                .enumerate()
                                                .filter(|&(_, &l)| l <= target)
                                                .map(|(i, _)| i)
                                                .next_back()
                                                .unwrap_or(0)
                                        })
                                    })
                                    .unwrap_or(0);
                                let extracted = extract_step_by_line_range(&lines, step_idx, &state.log_step_line_starts);
                                state.log_section_idx = Some(step_idx);
                                state.log_lines = extracted;
                            } else {
                                // Fallback: section-based approach.
                                let idx = started_hms.as_deref()
                                    .and_then(|start| find_section_by_time(&lines, start))
                                    .or_else(|| {
                                        let needle = step_name.trim().to_lowercase();
                                        state.log_sections.iter()
                                            .position(|s| s.trim().to_lowercase() == needle)
                                            .or_else(|| state.log_sections.iter()
                                                .position(|s| {
                                                    let sl = s.trim().to_lowercase();
                                                    sl.contains(&needle) || needle.contains(sl.as_str())
                                                }))
                                    });
                                state.log_section_idx = idx;
                                state.log_lines = idx.map(|i| extract_log_section(&lines, i)).unwrap_or(lines);
                            }
                        } else {
                            state.log_section_idx = None;
                            state.log_lines = lines;
                        }
                        state.log_scroll = 0;
                        state.init_log_groups();
                        state.recompute_log_rendered();
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RepoCardLoaded(spec, runs) => {
                        // Announce finished runs from any dashboard row, not just
                        // the active repo — watching several repos at once is the
                        // whole point of the dashboard.
                        for r in &runs {
                            announce_if_finished(state, r, &spec, &config);
                        }
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            card.runs = runs;
                            card.error = None;
                            card.loaded = true;
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RepoCardFailed(spec, err) => {
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            card.error = Some(err);
                            card.loaded = true;
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::GitStatusLoaded(spec, status) => {
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            card.git = Some(status.clone());
                        }
                        if let Some(gv) = state.git_view.as_mut()
                            && gv.spec == spec
                        {
                            // Keep the cursor in range as files leave the list.
                            gv.cursor = gv.cursor.min(status.entries.len().saturating_sub(1));
                            gv.status = Some(status);
                            gv.busy = false;
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::GitOpDone(spec, result) => {
                        if let Some(gv) = state.git_view.as_mut()
                            && gv.spec == spec
                        {
                            gv.busy = false;
                        }
                        match result {
                            Ok(msg) => state.set_status(msg),
                            Err(err) => state.set_status(err),
                        }
                        state.pending = state.pending.saturating_sub(1);
                        // The working tree moved; re-read it.
                        if let Some(gv) = state.git_view.as_ref() {
                            let (spec, path) = (gv.spec.clone(), gv.path.clone());
                            spawn_git_status(spec, path, tx.clone(), state);
                        }
                    }
                    AppEvent::RepoSwitched { label, branch, workflows } => {
                        state.repo_label = label;
                        state.current_branch = branch;
                        state.workflows = sort_with_favorites(workflows, &config);
                        state.workflow_cursor = 0;
                        if let Ok(spec) = RepoSpec::parse(&state.repo_label) {
                            state.history = History::load_for_repo(&spec.owner, &spec.repo);
                        }
                        for w in &mut state.workflows {
                            if let Some(entry) = state.history.last_run(&w.file_name) {
                                w.last_run_at = Some(entry.created_at);
                                w.last_status = Some(entry.status);
                            }
                        }
                        state.switch_view(View::Workflows);
                        state.pending = state.pending.saturating_sub(1);
                        spawn_repo_status_fetch(provider.clone(), tx.clone());
                        if let Some(w) = state.selected_workflow().cloned() {
                            spawn_fetch_workflow_preview(provider.clone(), w.file_name, tx.clone(), state);
                        }
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
                    // The finder holds indices into the list it was opened over.
                    // Refreshing that list underneath it would shift every index,
                    // so Enter would commit to whatever slid into the slot.
                    if state.pending == 0 && state.finder.is_none() {
                        match state.view {
                            View::Repos => {
                                spawn_fetch_repo_cards(&provider, state, &tx);
                            }
                            View::Watch => {
                                if let Some(file) = state.workflow_for_runs.clone() {
                                    spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
                                }
                                if let Some(run) = state.runs.first().cloned() {
                                    spawn_fetch_run_detail(provider.clone(), run.id, tx.clone(), state);
                                }
                            }
                            View::Runs => {
                                let has_active = state.runs.iter().any(|r| !r.status.is_terminal());
                                if has_active {
                                    if let Some(file) = state.workflow_for_runs.clone() {
                                        spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
                                    }
                                }
                            }
                            View::RunDetail => {
                                let run_active = state
                                    .run_detail
                                    .as_ref()
                                    .map(|d| !d.run.status.is_terminal())
                                    .or_else(|| state.selected_run().map(|r| !r.status.is_terminal()))
                                    .unwrap_or(false);
                                if run_active {
                                    if let Some(run) = state.selected_run().cloned() {
                                        spawn_fetch_run_detail(provider.clone(), run.id, tx.clone(), state);
                                    }
                                }
                            }
                            _ => {}
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
    provider: &mut Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    km: &Keymap,
) -> Option<AppEvent> {
    // ctrl+c always quits regardless of keymap
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(AppEvent::Quit);
    }
    // The finder is a modal overlay: while it's open it owns every key.
    if state.finder.is_some() {
        handle_finder_key(state, key, km, provider, tx);
        return None;
    }
    // While typing a commit message, every key belongs to the editor.
    if state.view == View::GitStatus
        && state.git_view.as_ref().is_some_and(|g| g.commit_input.is_some())
    {
        handle_commit_input(state, key, tx);
        return None;
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

    // The help overlay sits above everything except the text inputs handled
    // above — while it's open it swallows keys rather than acting on the view
    // behind it.
    if state.show_help {
        match key.code {
            KeyCode::Down => state.help_scroll = state.help_scroll.saturating_add(1),
            KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
            KeyCode::PageDown => state.help_scroll = state.help_scroll.saturating_add(10),
            KeyCode::PageUp => state.help_scroll = state.help_scroll.saturating_sub(10),
            _ if key_is(&key, km.down) => {
                state.help_scroll = state.help_scroll.saturating_add(1)
            }
            _ if key_is(&key, km.up) => state.help_scroll = state.help_scroll.saturating_sub(1),
            // Anything else — `?`, Esc, q, Enter — dismisses it.
            _ => {
                state.show_help = false;
                state.needs_clear = true;
            }
        }
        return None;
    }

    // Global: keybinding reference.
    if key_is(&key, km.help) {
        state.show_help = true;
        state.help_scroll = 0;
        return None;
    }

    // Global: quit
    if key_is(&key, km.quit) {
        return Some(AppEvent::Quit);
    }

    // Global: open the fuzzy finder over whatever list the current view shows.
    if key_is(&key, km.finder) {
        open_finder(state);
        return None;
    }

    // Global: jump to the multi-repo dashboard.
    if key_is(&key, km.repos_view) && state.view != View::Repos {
        // The active repo is always row one, so a single row means nothing was
        // configured — a one-row dashboard would just look broken.
        if state.repos.len() <= 1 {
            state.set_status(
                "no other repos configured — add `repos = [\"owner/name\", …]` under [provider]"
                    .into(),
            );
        } else {
            if let Some(i) = state.repos.iter().position(|r| r.spec == state.repo_label) {
                state.repo_cursor = i;
            }
            state.switch_view(View::Repos);
            spawn_fetch_repo_cards(provider, state, tx);
        }
        return None;
    }

    // Global: back — Esc is always accepted as a fallback regardless of config
    if key_is(&key, km.back) || key.code == KeyCode::Esc {
        match state.view {
            View::Repos => return Some(AppEvent::Quit),
            View::GitStatus => {
                state.git_view = None;
                state.switch_view(View::Repos);
            }
            // The dashboard is "above" the workflow list, so back steps up to it
            // when there is one to step up to.
            View::Workflows if state.repos.len() > 1 => {
                if let Some(i) = state.repos.iter().position(|r| r.spec == state.repo_label) {
                    state.repo_cursor = i;
                }
                state.switch_view(View::Repos);
                spawn_fetch_repo_cards(provider, state, tx);
            }
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
        if state.view == View::Repos {
            if let Some(card) = state.repos.get(state.repo_cursor) {
                let url = format!("https://github.com/{}/actions", card.spec);
                let _ = open::that(&url);
                state.set_status(format!("opened {}", card.spec));
            }
            return None;
        }
        if let Some(url) = state.run_detail.as_ref().map(|d| d.run.url.clone())
            .or_else(|| state.runs.get(state.run_cursor).map(|r| r.url.clone()))
        {
            let _ = open::that(&url);
            state.set_status("opened in browser".to_string());
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

    if key_is(&key, km.yank) {
        let text: Option<String> = match state.view {
            View::Repos => state.repos.get(state.repo_cursor).map(|c| c.spec.clone()),
            View::GitStatus => state
                .git_view
                .as_ref()
                .and_then(|g| g.selected())
                .map(|e| e.path.clone()),
            View::Workflows => state.selected_workflow().map(|w| w.file_name.clone()),
            View::Runs | View::Watch => state
                .run_detail.as_ref().map(|d| d.run.url.clone())
                .or_else(|| state.selected_run().map(|r| r.url.clone())),
            View::RunDetail => state.run_detail.as_ref().and_then(|detail| {
                let items = build_detail_items(detail);
                match items.get(state.detail_cursor) {
                    Some(DetailItem::Job(ji)) => detail.jobs.get(*ji).map(|j| j.name.clone()),
                    Some(DetailItem::Step { job: ji, step: si }) => detail.jobs
                        .get(*ji)
                        .and_then(|j| j.steps.get(*si))
                        .map(|s| s.name.clone()),
                    None => None,
                }
            }),
            View::Logs => state.log_rendered
                .get(state.log_line_cursor as usize)
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect()),
            View::Diff => state.run_detail.as_ref().map(|d| d.run.url.clone()),
            View::TriggerPrompt => None,
        };
        if let Some(text) = text {
            match yank_to_clipboard(&text) {
                Ok(()) => {
                    let preview: String = text.chars().take(50).collect();
                    let ellipsis = if text.chars().count() > 50 { "…" } else { "" };
                    state.set_status(format!("yanked: {preview}{ellipsis}"));
                }
                Err(e) => state.set_status(format!("yank failed: {e}")),
            }
        }
        return None;
    }

    match state.view {
        View::Repos => {
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                move_cursor(&mut state.repo_cursor, state.repos.len(), 1);
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                move_cursor(&mut state.repo_cursor, state.repos.len(), -1);
            } else if key_is(&key, km.confirm) || key.code == KeyCode::Enter {
                switch_to_selected_repo(state, provider, tx);
            } else if key_is(&key, km.git_view) {
                open_git_view(state, tx);
            }
        }
        View::GitStatus => {
            let len = state.git_view.as_ref().map(|g| g.entries().len()).unwrap_or(0);
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                if let Some(g) = state.git_view.as_mut() {
                    move_cursor(&mut g.cursor, len, 1);
                }
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                if let Some(g) = state.git_view.as_mut() {
                    move_cursor(&mut g.cursor, len, -1);
                }
            } else if key_is(&key, km.git_stage) {
                toggle_stage_at_cursor(state, tx);
            } else if key_is(&key, km.git_stage_all) {
                spawn_git_op(state, tx, |dir| {
                    crate::git::stage_all(dir).map(|_| "staged all changes".to_string())
                });
            } else if key_is(&key, km.git_commit) {
                begin_commit(state);
            } else if key_is(&key, km.git_push) {
                push_current(state, tx);
            } else if key_is(&key, km.git_refresh) {
                if let Some(g) = state.git_view.as_ref() {
                    let (spec, path) = (g.spec.clone(), g.path.clone());
                    spawn_git_status(spec, path, tx.clone(), state);
                }
            } else if key_is(&key, km.trigger) {
                // Hand off to CI: switch the app to this repo and show its
                // workflows, where `t` triggers as usual.
                switch_to_selected_repo(state, provider, tx);
            }
        }
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
            } else if key_is(&key, km.watch)
                && let Some(w) = state.selected_workflow().cloned() {
                    state.switch_view(View::Watch);
                    state.workflow_for_runs = Some(w.file_name.clone());
                    state.runs.clear();
                    spawn_fetch_runs(provider.clone(), w.file_name, tx.clone(), state);
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
                if let Some(file) = state.workflow_for_runs.clone()
                    && let Some(w) = state.workflows.iter().find(|w| w.file_name == file).cloned() {
                        trigger_workflow(state, &w, provider, tx);
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
            } else if key_is(&key, km.rerun_failed)
                && let Some(r) = state.selected_run().cloned() {
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
        View::RunDetail => {
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                let max = state.run_detail.as_ref().map(|d| build_detail_items(d).len()).unwrap_or(0);
                move_cursor(&mut state.detail_cursor, max, 1);
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                let max = state.run_detail.as_ref().map(|d| build_detail_items(d).len()).unwrap_or(0);
                move_cursor(&mut state.detail_cursor, max, -1);
            } else if key_is(&key, km.diff) {
                state.switch_view(View::Diff);
            } else if (key_is(&key, km.confirm) || key.code == KeyCode::Enter || key_is(&key, km.open_logs))
                && let Some(detail) = &state.run_detail {
                    let items = build_detail_items(detail);
                    match items.get(state.detail_cursor).copied() {
                        Some(DetailItem::Job(ji)) => {
                            if let Some(job) = detail.jobs.get(ji).cloned() {
                                state.log_pending_section = None;
                                state.log_job_idx = Some(ji);
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
                                let hms = |dt: chrono::DateTime<chrono::Utc>| {
                                    format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second())
                                };
                                state.log_pending_section = Some((
                                    step.name.clone(),
                                    step.started_at.map(&hms),
                                    step.completed_at.map(&hms),
                                ));
                                state.log_job_idx = Some(ji);
                                state.switch_view(View::Logs);
                                state.log_lines = vec!["loading...".into()];
                                spawn_fetch_logs(provider.clone(), job.id, tx.clone(), state);
                            }
                        }
                        None => {}
                    }
                }
        }
        View::Logs => {
            let total_rendered = state.log_rendered.len() as u16;
            let viewport = state.last_logs_viewport_height.get().max(1);
            let max_cursor = total_rendered.saturating_sub(1);

            if key_is(&key, km.down) || key.code == KeyCode::Down {
                state.log_line_cursor = state.log_line_cursor.saturating_add(1).min(max_cursor);
                state.keep_cursor_visible();
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                state.log_line_cursor = state.log_line_cursor.saturating_sub(1);
                state.keep_cursor_visible();
            } else if key_is(&key, km.page_down) || key.code == KeyCode::PageDown {
                state.log_line_cursor = state.log_line_cursor.saturating_add(viewport).min(max_cursor);
                state.keep_cursor_visible();
            } else if key_is(&key, km.page_up) || key.code == KeyCode::PageUp {
                state.log_line_cursor = state.log_line_cursor.saturating_sub(viewport);
                state.keep_cursor_visible();
            } else if key_is(&key, km.scroll_top) {
                state.log_line_cursor = 0;
                state.log_scroll = 0;
            } else if key_is(&key, km.scroll_bottom) {
                state.log_line_cursor = max_cursor;
                state.log_scroll = state.max_log_scroll();
            } else if key.code == KeyCode::Enter {
                let cursor = state.log_line_cursor;
                if let Some(&gi) = state.log_rendered_group_map.get(&cursor) {
                    if state.log_collapsed.contains(&gi) {
                        state.log_collapsed.remove(&gi);
                    } else {
                        state.log_collapsed.insert(gi);
                    }
                    state.recompute_log_rendered();
                    // After toggle, re-read where the group header landed and keep cursor there
                    if let Some(&new_row) = state.log_group_header_rows.get(gi) {
                        state.log_line_cursor = new_row;
                        state.keep_cursor_visible();
                        state.recompute_log_rendered();
                    }
                }
            } else if key_is(&key, km.search) {
                state.log_search_input = Some(String::new());
            } else if key_is(&key, km.log_focus) {
                toggle_log_focus(state);
            } else if key_is(&key, km.next_error) {
                jump_log_error(state, 1);
            } else if key_is(&key, km.prev_error) {
                jump_log_error(state, -1);
            } else if key_is(&key, km.next_step) {
                if state.log_search_query.is_some() {
                    jump_log_match(state, 1);
                } else if !state.log_step_line_starts.is_empty() {
                    let n = state.log_step_line_starts.len();
                    let next = if let Some(cur) = state.log_section_idx { (cur + 1).min(n - 1) } else { 0 };
                    let extracted = extract_step_by_line_range(&state.log_raw, next, &state.log_step_line_starts);
                    state.log_section_idx = Some(next);
                    state.log_lines = extracted;
                    state.log_scroll = 0;
                    state.clear_log_search();
                    state.init_log_groups();
                    state.recompute_log_rendered();
                } else {
                    let n = state.log_sections.len();
                    if n > 0 {
                        let next = state.log_section_idx.map(|i| (i + 1).min(n - 1)).unwrap_or(0);
                        state.log_section_idx = Some(next);
                        state.log_lines = extract_log_section(&state.log_raw, next);
                        state.log_scroll = 0;
                        state.clear_log_search();
                        state.init_log_groups();
                        state.recompute_log_rendered();
                    }
                }
            } else if key_is(&key, km.prev_step) {
                if state.log_search_query.is_some() {
                    jump_log_match(state, -1);
                } else if !state.log_step_line_starts.is_empty() {
                    match state.log_section_idx {
                        None => {}
                        Some(0) => {
                            state.log_section_idx = None;
                            state.log_lines = state.log_raw.clone();
                            state.log_scroll = 0;
                            state.clear_log_search();
                            state.init_log_groups();
                            state.recompute_log_rendered();
                        }
                        Some(current) => {
                            let prev = current - 1;
                            let extracted = extract_step_by_line_range(&state.log_raw, prev, &state.log_step_line_starts);
                            state.log_section_idx = Some(prev);
                            state.log_lines = extracted;
                            state.log_scroll = 0;
                            state.clear_log_search();
                            state.init_log_groups();
                            state.recompute_log_rendered();
                        }
                    }
                } else {
                    match state.log_section_idx {
                        None => {}
                        Some(0) => {
                            state.log_section_idx = None;
                            state.log_lines = state.log_raw.clone();
                            state.log_scroll = 0;
                            state.clear_log_search();
                            state.init_log_groups();
                            state.recompute_log_rendered();
                        }
                        Some(i) => {
                            let prev = i - 1;
                            state.log_section_idx = Some(prev);
                            state.log_lines = extract_log_section(&state.log_raw, prev);
                            state.log_scroll = 0;
                            state.clear_log_search();
                            state.init_log_groups();
                            state.recompute_log_rendered();
                        }
                    }
                }
            } else if key_is(&key, km.all_steps) {
                state.log_section_idx = None;
                state.log_lines = state.log_raw.clone();
                state.log_scroll = 0;
                state.clear_log_search();
                state.init_log_groups();
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
                if let Some(p) = state.trigger_prompt.as_mut()
                    && let Some(f) = p.current_field_mut()
                        && f.options.as_deref().is_some_and(|o| o.iter().any(|x| x == "yes")) {
                            f.value = "yes".to_string();
                        }
            } else if key_is(&key, km.tp_no) {
                if let Some(p) = state.trigger_prompt.as_mut()
                    && let Some(f) = p.current_field_mut()
                        && f.options.as_deref().is_some_and(|o| o.iter().any(|x| x == "no")) {
                            f.value = "no".to_string();
                        }
            } else if key_is(&key, km.confirm) || key.code == KeyCode::Enter || key_is(&key, km.tp_edit) {
                if let Some(p) = state.trigger_prompt.as_mut()
                    && p.current_field().is_some() {
                        p.editing = true;
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

/// Toggle "show only errors and warnings (plus context)".
///
/// Leaving focus mode restores the collapsed-by-default group state rather than
/// dumping the user into a fully expanded 40k-line log.
fn toggle_log_focus(state: &mut AppState) {
    state.log_focus = !state.log_focus;
    if state.log_focus && state.log_error_lines.is_empty() && state.log_warn_lines.is_empty() {
        state.log_focus = false;
        state.set_status("focus: no errors or warnings in this view".into());
        return;
    }
    // Anchor on the source line under the cursor so the viewport doesn't jump
    // to an unrelated part of the log when the filter flips.
    let anchor = state
        .log_rendered_src
        .get(state.log_line_cursor as usize)
        .copied();
    state.log_scroll = 0;
    state.log_line_cursor = 0;
    state.recompute_log_rendered();
    if let Some(src) = anchor
        && let Some(row) = state.rendered_row_for_src(src)
    {
        state.log_line_cursor = row;
        state.center_cursor();
        state.recompute_log_rendered();
    }
    let msg = if state.log_focus {
        format!(
            "focus on — {} error(s), {} warning(s)",
            state.log_error_lines.len(),
            state.log_warn_lines.len()
        )
    } else {
        "focus off".into()
    };
    state.set_status(msg);
}

/// Move the cursor to the next (`dir > 0`) or previous error line, wrapping at
/// the ends. Expands the containing group if the error is folded away.
fn jump_log_error(state: &mut AppState, dir: i32) {
    if state.log_error_lines.is_empty() {
        state.set_status("no errors in this view".into());
        return;
    }
    let cur_src = state
        .log_rendered_src
        .get(state.log_line_cursor as usize)
        .copied()
        .unwrap_or(0);
    let target = if dir > 0 {
        state
            .log_error_lines
            .iter()
            .find(|&&l| l > cur_src)
            .copied()
            .unwrap_or(state.log_error_lines[0])
    } else {
        state
            .log_error_lines
            .iter()
            .rev()
            .find(|&&l| l < cur_src)
            .copied()
            .unwrap_or_else(|| *state.log_error_lines.last().unwrap())
    };

    // An error inside a collapsed group has no rendered row to jump to.
    if !state.log_focus
        && let Some(gi) = state.group_containing(target)
    {
        state.log_collapsed.remove(&gi);
    }
    state.recompute_log_rendered();

    if let Some(row) = state.rendered_row_for_src(target) {
        state.log_line_cursor = row;
        state.center_cursor();
        state.recompute_log_rendered();
    }
    let pos = state
        .log_error_lines
        .iter()
        .position(|&l| l == target)
        .map(|i| i + 1)
        .unwrap_or(0);
    state.set_status(format!("error {}/{}", pos, state.log_error_lines.len()));
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

/// Put the current search hit on screen. Uses the rendered-row map rather than
/// re-deriving offsets, so it accounts for wrapping the same way error jumps do.
fn scroll_to_current_match(state: &mut AppState) {
    let Some(idx) = state.log_search_match_idx else { return };
    let Some(&src_line) = state.log_search_matches.get(idx) else { return };
    // Matches are collected over every source line, but groups start collapsed —
    // so a hit inside a folded group has no rendered row and the jump would
    // silently do nothing. Unfold it first, exactly as error jumps do.
    if !state.log_focus
        && let Some(gi) = state.group_containing(src_line)
    {
        state.log_collapsed.remove(&gi);
    }
    state.recompute_log_rendered();
    if let Some(row) = state.rendered_row_for_src(src_line) {
        state.log_line_cursor = row;
        state.center_cursor();
        // The cursor highlight and the current-match colour both live in the
        // rendered lines, so they need a second pass once the cursor has moved.
        state.recompute_log_rendered();
    }
}

/// Build the finder candidate list for whatever the current view is showing.
/// Views with nothing list-shaped to search (the trigger modal, the diff) just
/// report that and stay put.
fn open_finder(state: &mut AppState) {
    let finder = match state.view {
        View::Repos => {
            let items = state
                .repos
                .iter()
                .enumerate()
                .map(|(i, c)| (i, c.spec.clone()))
                .collect();
            Some(Finder::new(FinderKind::Repos, items))
        }
        View::Workflows => {
            let items = state
                .workflows
                .iter()
                .enumerate()
                .map(|(i, w)| (i, format!("{}  {}", w.name, w.file_name)))
                .collect();
            Some(Finder::new(FinderKind::Workflows, items))
        }
        View::Runs | View::Watch => {
            let items = state
                .runs
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    (
                        i,
                        format!("{}  {}  #{}", r.head_branch, r.commit_msg, r.id),
                    )
                })
                .collect();
            Some(Finder::new(FinderKind::Runs, items))
        }
        View::RunDetail => state.run_detail.as_ref().map(|detail| {
            let items = build_detail_items(detail)
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let label = match item {
                        DetailItem::Job(ji) => detail.jobs[*ji].name.clone(),
                        DetailItem::Step { job, step } => {
                            format!("{}  {}", detail.jobs[*job].steps[*step].name, detail.jobs[*job].name)
                        }
                    };
                    (i, label)
                })
                .collect();
            Finder::new(FinderKind::DetailItems, items)
        }),
        View::GitStatus => state.git_view.as_ref().map(|g| {
            let items = g
                .entries()
                .iter()
                .enumerate()
                .map(|(i, e)| (i, e.path.clone()))
                .collect();
            Finder::new(FinderKind::GitEntries, items)
        }),
        View::Logs | View::Diff | View::TriggerPrompt => None,
    };
    match finder {
        Some(f) if !f.items.is_empty() => state.finder = Some(f),
        Some(_) => state.set_status("nothing to search here yet".into()),
        None => state.set_status("finder is not available in this view".into()),
    }
}

/// Drive the finder overlay. Typing filters, arrows/`ctrl+n`/`ctrl+p` move,
/// Enter commits the highlighted item onto the underlying cursor, Esc cancels.
fn handle_finder_key(
    state: &mut AppState,
    key: KeyEvent,
    km: &Keymap,
    provider: &mut Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(finder) = state.finder.as_mut() else {
        return;
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // `handled` guards the toggle below: a key the overlay already acted on must
    // not *also* close it. With the default `ctrl+p` binding that mattered a
    // lot — ctrl+p moved the selection up and then immediately dismissed the
    // finder, so "previous match" was unreachable. Likewise a plain-character
    // binding would be typed into the query and then close the overlay.
    let handled = match key.code {
        KeyCode::Esc => {
            state.finder = None;
            true
        }
        KeyCode::Down => {
            let len = finder.matches.len();
            move_cursor(&mut finder.cursor, len, 1);
            true
        }
        KeyCode::Up => {
            let len = finder.matches.len();
            move_cursor(&mut finder.cursor, len, -1);
            true
        }
        KeyCode::Char('n') if ctrl => {
            let len = finder.matches.len();
            move_cursor(&mut finder.cursor, len, 1);
            true
        }
        KeyCode::Char('p') if ctrl => {
            let len = finder.matches.len();
            move_cursor(&mut finder.cursor, len, -1);
            true
        }
        KeyCode::Backspace => {
            finder.query.pop();
            finder.recompute();
            true
        }
        KeyCode::Enter => {
            let Some(target) = finder.selected_target() else {
                state.finder = None;
                return;
            };
            let kind = finder.kind;
            state.finder = None;
            commit_finder_choice(state, kind, target, provider, tx);
            true
        }
        KeyCode::Char(c) if !ctrl => {
            finder.query.push(c);
            finder.recompute();
            true
        }
        _ => false,
    };
    // A finder key the overlay has no other use for still toggles it closed.
    if !handled && state.finder.is_some() && key_is(&key, km.finder) {
        state.finder = None;
    }
}

/// Apply a finder selection: move the underlying cursor and refresh whatever
/// side panel depends on it.
fn commit_finder_choice(
    state: &mut AppState,
    kind: FinderKind,
    target: usize,
    provider: &mut Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    match kind {
        FinderKind::Repos => {
            state.repo_cursor = target.min(state.repos.len().saturating_sub(1));
        }
        FinderKind::Workflows => {
            state.workflow_cursor = target.min(state.workflows.len().saturating_sub(1));
            maybe_fetch_workflow_preview(state, provider, tx);
        }
        FinderKind::Runs => {
            state.run_cursor = target.min(state.runs.len().saturating_sub(1));
            maybe_fetch_preview(state, provider, tx);
        }
        FinderKind::GitEntries => {
            if let Some(g) = state.git_view.as_mut() {
                g.cursor = target.min(g.entries().len().saturating_sub(1));
            }
        }
        FinderKind::DetailItems => {
            let max = state
                .run_detail
                .as_ref()
                .map(|d| build_detail_items(d).len())
                .unwrap_or(0);
            state.detail_cursor = target.min(max.saturating_sub(1));
        }
    }
}

/// Stage or unstage the file under the cursor, whichever way it isn't already.
fn toggle_stage_at_cursor(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(entry) = state.git_view.as_ref().and_then(|g| g.selected()).cloned() else {
        return;
    };
    // A file that is staged *and* further modified stages the rest — that's the
    // more useful reading of "toggle" than throwing the staged part away.
    let unstage = entry.is_staged() && !entry.has_unstaged();
    let path = entry.path;
    if unstage {
        spawn_git_op(state, tx, move |dir| {
            crate::git::unstage(dir, &path).map(|_| format!("unstaged {path}"))
        });
    } else {
        spawn_git_op(state, tx, move |dir| {
            crate::git::stage(dir, &path).map(|_| format!("staged {path}"))
        });
    }
}

/// Open the commit-message editor, refusing when the index is empty.
fn begin_commit(state: &mut AppState) {
    let Some(gv) = state.git_view.as_mut() else {
        return;
    };
    if gv.busy {
        state.set_status("a git command is already running".into());
        return;
    }
    if gv.staged_count() == 0 {
        let has_changes = !gv.entries().is_empty();
        state.set_status(if has_changes {
            "nothing staged — Space stages the selected file, a stages everything".into()
        } else {
            "working tree is clean".to_string()
        });
        return;
    }
    gv.commit_input = Some(String::new());
}

/// Type the commit message. Enter commits, Esc abandons the draft.
fn handle_commit_input(
    state: &mut AppState,
    key: KeyEvent,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(gv) = state.git_view.as_mut() else {
        return;
    };
    let Some(buf) = gv.commit_input.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            gv.commit_input = None;
        }
        KeyCode::Enter => {
            let msg = std::mem::take(buf).trim().to_string();
            gv.commit_input = None;
            if msg.is_empty() {
                state.set_status("commit aborted: empty message".into());
                return;
            }
            spawn_git_op(state, tx, move |dir| {
                crate::git::commit(dir, &msg)?;
                let sha = crate::git::head_sha(dir).unwrap_or_else(|_| "HEAD".into());
                Ok(format!("committed {sha} — push, then `t` to run CI"))
            });
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

/// Push the current branch, setting upstream on first push.
///
/// This is the step that makes a commit visible to CI: `workflow_dispatch` runs
/// against the remote, so triggering before pushing would run the *old* code.
fn push_current(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    // Check this before reading `status`: a commit in flight has not been folded
    // into the ahead count yet, so we'd otherwise report "nothing to push".
    if gv.busy {
        state.set_status("a git command is already running".into());
        return;
    }
    let Some(status) = gv.status.clone() else {
        state.set_status("status not loaded yet".into());
        return;
    };
    // `push --set-upstream origin HEAD` while detached would create a remote
    // branch literally named `HEAD`. Refuse rather than let git make that mess.
    if status.detached {
        state.set_status("detached HEAD — check out a branch before pushing".into());
        return;
    }
    if status.has_upstream && status.ahead == 0 {
        state.set_status("nothing to push — branch is up to date".into());
        return;
    }
    let branch = status.branch.clone();
    let has_upstream = status.has_upstream;
    state.set_status(format!("pushing {branch}…"));
    spawn_git_op(state, tx, move |dir| {
        crate::git::push(dir, &branch, has_upstream)
    });
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

fn yank_to_clipboard(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text))
        .map_err(|e| e.to_string())
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
    help: (KeyCode, KeyModifiers),
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
    log_focus: (KeyCode, KeyModifiers),
    next_error: (KeyCode, KeyModifiers),
    prev_error: (KeyCode, KeyModifiers),
    finder: (KeyCode, KeyModifiers),
    repos_view: (KeyCode, KeyModifiers),
    git_view: (KeyCode, KeyModifiers),
    git_stage: (KeyCode, KeyModifiers),
    git_stage_all: (KeyCode, KeyModifiers),
    git_commit: (KeyCode, KeyModifiers),
    git_push: (KeyCode, KeyModifiers),
    git_refresh: (KeyCode, KeyModifiers),
    trigger: (KeyCode, KeyModifiers),
    watch: (KeyCode, KeyModifiers),
    open_browser: (KeyCode, KeyModifiers),
    cancel_run: (KeyCode, KeyModifiers),
    rerun: (KeyCode, KeyModifiers),
    rerun_failed: (KeyCode, KeyModifiers),
    diff: (KeyCode, KeyModifiers),
    yank: (KeyCode, KeyModifiers),
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
    if event.code != code {
        return false;
    }
    // Terminals deliver Shift+r as `Char('R')` *plus* a SHIFT modifier
    // (crossterm's `char_code_to_event`), while config writes it as plain `"R"`.
    // Comparing modifiers strictly would make every uppercase binding dead, so
    // for character keys the case of the char is the shift signal and SHIFT
    // itself is ignored. Ctrl/Alt still have to match exactly.
    if matches!(code, KeyCode::Char(_)) {
        event.modifiers.difference(KeyModifiers::SHIFT) == mods.difference(KeyModifiers::SHIFT)
    } else {
        event.modifiers == mods
    }
}

fn resolve_keymap(cfg: &KeymapConfig) -> Result<Keymap> {
    Ok(Keymap {
        quit:          parse_key(&cfg.quit)?,
        back:          parse_key(&cfg.back)?,
        help:          parse_key(&cfg.help)?,
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
        log_focus:     parse_key(&cfg.log_focus)?,
        next_error:    parse_key(&cfg.next_error)?,
        prev_error:    parse_key(&cfg.prev_error)?,
        finder:        parse_key(&cfg.finder)?,
        repos_view:    parse_key(&cfg.repos_view)?,
        git_view:      parse_key(&cfg.git_view)?,
        git_stage:     parse_key(&cfg.git_stage)?,
        git_stage_all: parse_key(&cfg.git_stage_all)?,
        git_commit:    parse_key(&cfg.git_commit)?,
        git_push:      parse_key(&cfg.git_push)?,
        git_refresh:   parse_key(&cfg.git_refresh)?,
        trigger:       parse_key(&cfg.trigger)?,
        watch:         parse_key(&cfg.watch)?,
        open_browser:  parse_key(&cfg.open_browser)?,
        cancel_run:    parse_key(&cfg.cancel_run)?,
        rerun:         parse_key(&cfg.rerun)?,
        rerun_failed:  parse_key(&cfg.rerun_failed)?,
        diff:          parse_key(&cfg.diff)?,
        yank:          parse_key(&cfg.yank)?,
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


fn compute_step_line_starts(raw: &[String], steps: &[Step]) -> (Vec<usize>, Vec<String>) {
    if steps.is_empty() || raw.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // All depth-0 ##[group] positions: (line_idx, "HH:MM:SS", group_name).
    // Group names that start with "Run " are step-entry markers (used: or run: steps).
    // Sub-operation groups (Getting Git version info, Installed versions, etc.) never
    // start with "Run " and should not be treated as step boundaries.
    let group_positions: Vec<(usize, String, String)> = {
        let mut result = Vec::new();
        let mut depth = 0usize;
        for (i, line) in raw.iter().enumerate() {
            let content = strip_time_prefix(line);
            let is_group = content.starts_with("##[group]") || content.starts_with("##[section]");
            let is_end = content.starts_with("##[endgroup]");
            if is_group {
                if depth == 0 {
                    let gname = strip_ansi(
                        content.strip_prefix("##[group]")
                            .or_else(|| content.strip_prefix("##[section]"))
                            .unwrap_or(""),
                    );
                    result.push((i, line.get(..8).unwrap_or("").to_string(), gname));
                }
                depth += 1;
            } else if is_end {
                depth = depth.saturating_sub(1);
            }
        }
        result
    };

    let mut starts = Vec::with_capacity(steps.len());
    let mut names = Vec::with_capacity(steps.len());
    // Monotonic cursor: groups assigned to earlier steps are never reused.
    let mut group_cursor = 0usize;

    for (si, step) in steps.iter().enumerate() {
        let step_hms = step.started_at.map(|dt|
            format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second())
        );

        if si == 0 {
            // First step (Set up job) always starts at line 0 — it owns the preamble before
            // the first group and the setup sub-groups.
            starts.push(0);
            names.push(step.name.clone());
            continue;
        }

        // Primary: find the first "Run …"-prefixed group at or after group_cursor with
        // timestamp >= this step's start.  "Run " groups are the canonical step-entry
        // markers emitted by the runner for every uses:/run: step.
        let found = step_hms.as_deref().and_then(|hms| {
            group_positions.iter()
                .enumerate()
                .skip(group_cursor)
                .find(|(_, (_, t, n))| t.as_str() >= hms && n.starts_with("Run "))
                .map(|(gi, &(line_idx, _, _))| (gi, line_idx))
        });
        // Fallback: any group with timestamp >= step start (handles steps without "Run " headers).
        let found = found.or_else(|| {
            step_hms.as_deref().and_then(|hms| {
                group_positions.iter()
                    .enumerate()
                    .skip(group_cursor)
                    .find(|(_, (_, t, _))| t.as_str() >= hms)
                    .map(|(gi, &(line_idx, _, _))| (gi, line_idx))
            })
        });

        if let Some((gi, line_idx)) = found {
            group_cursor = gi + 1;
            starts.push(line_idx);
        } else {
            // No group found (post-steps, Complete job): fall back to raw timestamp line.
            let line_idx = step_hms.as_deref()
                .map(|hms| find_raw_line_for_time(raw, hms))
                .unwrap_or_else(|| starts.last().copied().unwrap_or(0));
            starts.push(line_idx.min(raw.len().saturating_sub(1)));
        }
        names.push(step.name.clone());
    }

    let any_matched = starts.iter().any(|&l| l > 0)
        || steps.first().and_then(|s| s.started_at).is_some();
    if any_matched { (starts, names) } else { (Vec::new(), Vec::new()) }
}

fn find_raw_line_for_time(raw: &[String], hms: &str) -> usize {
    for (i, line) in raw.iter().enumerate() {
        if let Some(t) = line.get(..8)
            && t.as_bytes().get(2) == Some(&b':')
            && t.as_bytes().get(5) == Some(&b':')
            && t >= hms
        {
            return i;
        }
    }
    raw.len()
}

fn extract_step_by_line_range(raw: &[String], step_idx: usize, line_starts: &[usize]) -> Vec<String> {
    let start = line_starts.get(step_idx).copied().unwrap_or(0);
    let end = line_starts.get(step_idx + 1).copied().unwrap_or(raw.len());
    raw[start.min(raw.len())..end.min(raw.len())].to_vec()
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
                    // After closing the top-level group, keep capturing lines
                    // that appear before the next top-level group — these are
                    // output lines that belong to this section (e.g. pytest
                    // errors that appear after ##[endgroup] in the job log).
                    depth = 0;
                    continue;
                }
            }
            depth = depth.saturating_sub(1);
        } else if capturing {
            result.push(line.clone());
        }
    }
    result
}

/// Find the index of the first top-level ##[group] whose timestamp is >= start_hms.
/// Because extract_log_section stops at the NEXT top-level group, using this index
/// correctly bounds the step's content even when the next step starts in the same second.
fn find_section_by_time(raw: &[String], start_hms: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut section_count = 0usize;
    for line in raw {
        let content = strip_time_prefix(line);
        let is_group = content.starts_with("##[group]") || content.starts_with("##[section]");
        let is_endgroup = content.starts_with("##[endgroup]");
        if is_group {
            if depth == 0 {
                if let Some(t) = line.get(..8)
                    && t.as_bytes().get(2) == Some(&b':') && t.as_bytes().get(5) == Some(&b':') && t >= start_hms {
                        return Some(section_count);
                    }
                section_count += 1;
            }
            depth += 1;
        } else if is_endgroup {
            depth = depth.saturating_sub(1);
        }
    }
    None
}

/// Refresh every dashboard row. One request per repo; they land independently so
/// a slow or broken repo doesn't hold up the rest.
fn spawn_fetch_repo_cards(
    provider: &Arc<GitHubProvider>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let rows: Vec<(String, Option<String>, Option<std::path::PathBuf>)> = state
        .repos
        .iter()
        .map(|c| (c.spec.clone(), c.remote.clone(), c.path.clone()))
        .collect();

    for (label, remote, path) in rows {
        // Local checkouts report their working-tree state whether or not they
        // have a remote — that half of the row is useful on its own.
        if let Some(path) = path {
            spawn_git_status(label.clone(), path, tx.clone(), state);
        }
        let Some(remote) = remote else { continue };
        let Ok(spec) = RepoSpec::parse(&remote) else {
            continue;
        };
        let p = Arc::new(provider.for_repo(spec));
        let tx2 = tx.clone();
        state.pending += 1;
        tokio::spawn(async move {
            let evt = match p.list_repo_runs(20).await {
                Ok(runs) => AppEvent::RepoCardLoaded(label, runs),
                // `{:#}` walks the anyhow chain — the outermost context alone is
                // just "list all repo runs", which tells the user nothing.
                Err(e) => AppEvent::RepoCardFailed(label, format!("{e:#}")),
            };
            let _ = tx2.send(evt);
        });
    }
}

/// Read a checkout's working-tree state off-thread.
///
/// `git status` is fast but not instant on a large repo, and the event loop must
/// not stall mid-frame — so every git call goes through `spawn_blocking`.
fn spawn_git_status(
    spec: String,
    path: std::path::PathBuf,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    state.pending += 1;
    tokio::task::spawn_blocking(move || {
        let evt = match crate::git::status(&path) {
            Ok(s) => AppEvent::GitStatusLoaded(spec, s),
            // Deliberately *not* `GitOpDone`: that event re-reads the status as
            // its final step, so a persistent failure (checkout deleted, no
            // `git` on PATH) would spin failure -> refresh -> failure forever.
            Err(e) => AppEvent::TaskError(format!("git status: {e:#}")),
        };
        let _ = tx.send(evt);
    });
}

/// Run one git mutation off-thread; the result refreshes the status view.
fn spawn_git_op<F>(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>, op: F)
where
    F: FnOnce(&std::path::Path) -> anyhow::Result<String> + Send + 'static,
{
    let Some(gv) = state.git_view.as_mut() else {
        return;
    };
    if gv.busy {
        state.set_status("a git command is already running".into());
        return;
    }
    gv.busy = true;
    let (spec, path) = (gv.spec.clone(), gv.path.clone());
    let tx = tx.clone();
    state.pending += 1;
    tokio::task::spawn_blocking(move || {
        let result = op(&path).map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::GitOpDone(spec, result));
    });
}

/// Open the working-tree view for the dashboard row under the cursor.
fn open_git_view(
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(card) = state.repos.get(state.repo_cursor) else {
        return;
    };
    let Some(path) = card.path.clone() else {
        state.set_status(format!(
            "{} has no local checkout — only repos found on disk can be committed",
            card.spec
        ));
        return;
    };
    let (spec, has_ci) = (card.spec.clone(), card.has_ci());
    state.git_view = Some(crate::app::state::GitView::new(spec.clone(), path.clone(), has_ci));
    state.switch_view(View::GitStatus);
    spawn_git_status(spec, path, tx.clone(), state);
}

/// Point the whole app at the repo under the dashboard cursor.
///
/// Workflows come from the API rather than the filesystem — we have no checkout
/// of the other repos — and the trigger ref switches to that repo's default
/// branch, since the local branch name almost certainly doesn't exist there.
fn switch_to_selected_repo(
    state: &mut AppState,
    provider: &mut Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(card) = state.repos.get(state.repo_cursor) else {
        return;
    };
    let Some(remote) = card.remote.clone() else {
        state.set_status(format!(
            "{} has no GitHub remote — nothing to run CI against",
            card.spec
        ));
        return;
    };
    let label = card.spec.clone();
    let local_path = card.path.clone();
    let spec = match RepoSpec::parse(&remote) {
        Ok(s) => s,
        Err(e) => {
            state.set_status(format!("bad repo `{remote}`: {e}"));
            return;
        }
    };

    *provider = Arc::new(provider.for_repo(spec));
    state.runs.clear();
    state.run_detail = None;
    state.runs_preview = None;
    state.runs_preview_id = None;
    state.workflow_preview_file = None;
    state.workflow_preview_runs.clear();
    state.workflow_for_runs = None;
    state.workflows.clear();
    state.set_status(format!("loading {label}…"));

    let p = provider.clone();
    let tx2 = tx.clone();
    state.pending += 1;
    tokio::spawn(async move {
        // With a checkout on disk, read the workflows from it: the YAML is
        // authoritative, it costs no API calls, and the trigger ref should be
        // the branch that checkout is actually on. Fall back to the API for
        // remote-only rows.
        let local = match &local_path {
            Some(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    let wfs = crate::provider::discovery::discover_workflows(&path).ok()?;
                    let branch = crate::git::current_branch(&path).ok()?;
                    Some((wfs, branch))
                })
                .await
                .ok()
                .flatten()
            }
            None => None,
        };

        if let Some((workflows, branch)) = local
            && !workflows.is_empty()
        {
            let _ = tx2.send(AppEvent::RepoSwitched {
                label,
                branch,
                workflows,
            });
            return;
        }

        let (workflows, branch) = tokio::join!(p.list_workflows(), p.default_branch());
        match workflows {
            Ok(workflows) => {
                let _ = tx2.send(AppEvent::RepoSwitched {
                    label,
                    branch: branch.unwrap_or_else(|_| "main".into()),
                    workflows,
                });
            }
            Err(e) => {
                let _ = tx2.send(AppEvent::TaskError(format!("load {label}: {e:#}")));
            }
        }
    });
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
    if let Some(w) = state.selected_workflow().cloned()
        && state.workflow_preview_file.as_deref() != Some(w.file_name.as_str()) {
            spawn_fetch_workflow_preview(provider.clone(), w.file_name, tx.clone(), state);
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
    if let Some(r) = state.selected_run().cloned()
        && state.runs_preview_id != Some(r.id) {
            spawn_fetch_run_preview(provider.clone(), r.id, tx.clone(), state);
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

const FINISHED_SOUND: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/finished.mp3"));
const FAIL_SOUND: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fail.mp3"));

/// Play the appropriate notification sound for a finished run: the fail sound
/// on failure/cancellation, otherwise the completion sound.
fn play_terminal_sound(status: Status, config: &Config) {
    let (configured, bundled, name) = if status.is_failure() {
        (&config.ui.fail_sound, FAIL_SOUND, "fail.mp3")
    } else {
        (&config.ui.complete_sound, FINISHED_SOUND, "finished.mp3")
    };
    if let Some(path) = sound_path(configured, bundled, name) {
        play_sound(&path);
    }
}

/// Track a run across polls and announce it exactly once, when it transitions
/// from in-flight to finished.
///
/// A run is only announced if we saw it running first — otherwise every startup
/// would fire a burst of notifications for runs that finished hours ago.
fn announce_if_finished(state: &mut AppState, run: &Run, repo_label: &str, config: &Config) {
    if !run.status.is_terminal() {
        state.watch_seen_running.insert(run.id);
        return;
    }
    if !state.watch_seen_running.remove(&run.id) {
        return;
    }
    notify_run_finished(run, repo_label, config);
}

/// Sound + desktop notification for a finished run, gated on `ui.notify`.
fn notify_run_finished(run: &Run, repo_label: &str, config: &Config) {
    let wanted = match config.ui.notify_mode() {
        NotifyMode::Never => false,
        NotifyMode::Failure => run.status.is_failure(),
        NotifyMode::Always => true,
    };
    if !wanted {
        return;
    }
    if config.ui.notify_sound {
        play_terminal_sound(run.status, config);
    }
    if config.ui.notify_desktop {
        let summary = format!(
            "{} {}",
            if run.status.is_failure() { "✗" } else { "✓" },
            run.display_title
        );
        let body = format!(
            "{repo_label} · {} · {:?}",
            run.head_branch, run.status
        );
        desktop_notify(summary, body, run.status.is_failure());
    }
}

/// Raise an OS notification off-thread. Failures (no notification daemon, no
/// D-Bus session over SSH) are ignored — the sound and the TUI still work.
fn desktop_notify(summary: String, body: String, is_failure: bool) {
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&summary).body(&body).appname("jog");
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            n.urgency(if is_failure {
                notify_rust::Urgency::Critical
            } else {
                notify_rust::Urgency::Normal
            });
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _ = is_failure;
        let _ = n.show();
    });
}

/// Resolve a notification sound path: the configured file if set, otherwise the
/// bundled bytes extracted into the user's cache dir on first use.
fn sound_path(configured: &str, bundled: &'static [u8], name: &str) -> Option<String> {
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    let dir = dirs::cache_dir()?.join("jog");
    let path = dir.join(name);
    if !path.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&path, bundled).ok()?;
    }
    Some(path.to_string_lossy().into_owned())
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
    fn workspace_disambiguates_repos_sharing_a_basename() {
        let root = std::path::PathBuf::from("/tmp/jog-dup");
        let cards = workspace_repos(
            &[root.join("web/app"), root.join("mobile/app")],
            &Config::default(),
        );
        let specs: Vec<&str> = cards.iter().map(|c| c.spec.as_str()).collect();
        // Both would otherwise key on "app", so events for one would land on the other.
        assert_eq!(specs, vec!["app", "mobile/app"]);
        assert_eq!(
            specs.len(),
            specs.iter().collect::<std::collections::HashSet<_>>().len(),
            "row keys must be unique"
        );
    }

    #[test]
    fn workspace_rows_carry_path_and_remote() {
        let dir = std::env::temp_dir().join("jog-ws-rows");
        let cards = workspace_repos(
            &[dir.join("alpha"), dir.join("beta")],
            &Config::default(),
        );
        assert_eq!(cards.len(), 2);
        // No git repo actually exists at these paths, so no remote is resolved
        // and the row falls back to the directory name.
        assert_eq!(cards[0].spec, "alpha");
        assert_eq!(cards[0].remote, None);
        assert!(!cards[0].has_ci());
        assert_eq!(cards[0].path.as_deref(), Some(dir.join("alpha").as_path()));
    }

    #[test]
    fn workspace_appends_configured_repos_not_on_disk() {
        let dir = std::env::temp_dir().join("jog-ws-rows");
        let cards = workspace_repos(&[dir.join("alpha")], &cfg_with_repos(&["acme/remote-only"]));
        let specs: Vec<&str> = cards.iter().map(|c| c.spec.as_str()).collect();
        assert_eq!(specs, vec!["alpha", "acme/remote-only"]);
        assert!(cards[1].has_ci());
        assert!(cards[1].path.is_none());
    }

    #[test]
    fn local_card_prefers_remote_name_over_directory() {
        let card = RepoCard::local("/tmp/checkout-dir".into(), Some("acme/api".into()));
        assert_eq!(card.spec, "acme/api");
        assert!(card.has_ci());
        let local_only = RepoCard::local("/tmp/checkout-dir".into(), None);
        assert_eq!(local_only.spec, "checkout-dir");
        assert!(!local_only.has_ci());
    }

    #[test]
    fn dirty_count_reflects_working_tree() {
        let mut card = RepoCard::local("/tmp/x".into(), None);
        assert_eq!(card.dirty_count(), 0);
        card.git = Some(crate::git::parse_status("## main\0 M a.rs\0?? b.rs\0"));
        assert_eq!(card.dirty_count(), 2);
    }


    fn logs_state_with(lines: &[&str]) -> AppState {
        let mut st = empty_state();
        st.log_lines = lines.iter().map(|s| s.to_string()).collect();
        st.last_logs_viewport_height.set(20);
        st.last_logs_viewport_width.set(100);
        st.init_log_groups();
        st.recompute_log_rendered();
        st
    }

    #[test]
    fn search_reaches_a_match_inside_a_collapsed_group() {
        let mut st = logs_state_with(&[
            "##[group]setup",
            "buried needle here",
            "##[endgroup]",
            "tail",
        ]);
        assert!(
            !st.log_collapsed.is_empty(),
            "groups start collapsed, which is what makes this case possible"
        );
        // The buried line has no rendered row while the group is folded.
        assert_eq!(st.rendered_row_for_src(1), None);

        st.log_search_query = Some("needle".into());
        st.recompute_log_matches();
        assert_eq!(st.log_search_matches, vec![1]);

        scroll_to_current_match(&mut st);

        // The group is unfolded and the cursor now sits on the match.
        assert!(!st.log_collapsed.contains(&0), "group should be unfolded");
        let row = st
            .rendered_row_for_src(1)
            .expect("match must have a rendered row after unfolding");
        assert_eq!(st.log_line_cursor, row);
    }

    #[test]
    fn focus_mode_turns_itself_off_when_there_is_nothing_to_focus() {
        let mut st = logs_state_with(&["##[error]boom", "context"]);
        st.log_focus = true;
        // Simulate stepping to a section with no errors or warnings.
        st.log_lines = vec!["all quiet".into(), "still quiet".into()];
        st.init_log_groups();
        assert!(
            !st.log_focus,
            "focus must clear, or the pane would render completely empty"
        );
        assert!(st.compute_hidden_lines().is_empty());
    }

    #[test]
    fn focus_mode_persists_when_the_new_section_still_has_errors() {
        let mut st = logs_state_with(&["##[error]a"]);
        st.log_focus = true;
        st.log_lines = vec!["fine".into(), "##[error]b".into()];
        st.init_log_groups();
        assert!(st.log_focus, "focus should survive a step that still has errors");
    }

    #[test]
    fn uppercase_bindings_match_shifted_keys() {
        // Terminals send Shift+r as Char('R') + SHIFT; config writes plain "R".
        let binding = parse_key("R").unwrap();
        let pressed = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(key_is(&pressed, binding));
        // Lowercase must not be caught by the uppercase binding.
        let lower = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(!key_is(&lower, binding));
    }

    #[test]
    fn ctrl_bindings_still_require_ctrl() {
        let binding = parse_key("ctrl+p").unwrap();
        assert!(key_is(
            &KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            binding
        ));
        assert!(!key_is(
            &KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            binding
        ));
    }

    #[test]
    fn non_char_bindings_compare_modifiers_strictly() {
        let binding = parse_key("Enter").unwrap();
        assert!(key_is(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), binding));
        assert!(!key_is(
            &KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            binding
        ));
    }

    #[test]
    fn default_keymap_resolves() {
        // Every default binding must parse, or the TUI refuses to start.
        assert!(resolve_keymap(&KeymapConfig::default()).is_ok());
    }

    fn cfg_with_repos(repos: &[&str]) -> Config {
        let mut c = Config::default();
        c.provider.repos = repos.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn dashboard_puts_active_repo_first_without_duplicating() {
        let cards = dashboard_repos(&cfg_with_repos(&["o/b", "o/a"]), "o/a");
        let specs: Vec<&str> = cards.iter().map(|c| c.spec.as_str()).collect();
        assert_eq!(specs, vec!["o/a", "o/b"]);
    }

    #[test]
    fn dashboard_includes_active_repo_when_unlisted() {
        let cards = dashboard_repos(&cfg_with_repos(&["o/b"]), "o/a");
        let specs: Vec<&str> = cards.iter().map(|c| c.spec.as_str()).collect();
        assert_eq!(specs, vec!["o/a", "o/b"]);
    }

    #[test]
    fn dashboard_skips_blank_entries() {
        let cards = dashboard_repos(&cfg_with_repos(&["", "  ", "o/b"]), "o/a");
        assert_eq!(cards.len(), 2);
    }

    fn run_with(id: u64, status: Status) -> Run {
        Run {
            id,
            display_title: "ci".into(),
            head_branch: "main".into(),
            commit_msg: String::new(),
            status,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            url: String::new(),
            workflow_file: None,
        }
    }

    fn empty_state() -> AppState {
        AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        )
    }

    #[test]
    fn notify_modes_parse() {
        let mode = |s: &str| {
            let mut c = Config::default();
            c.ui.notify = s.into();
            c.ui.notify_mode()
        };
        assert_eq!(mode("never"), NotifyMode::Never);
        assert_eq!(mode("Failure"), NotifyMode::Failure);
        assert_eq!(mode("always"), NotifyMode::Always);
        // Unknown values must not break startup.
        assert_eq!(mode("nonsense"), NotifyMode::Always);
        assert_eq!(Config::default().ui.notify_mode(), NotifyMode::Always);
    }

    #[test]
    fn announce_only_fires_after_seeing_a_run_in_flight() {
        let mut st = empty_state();
        let mut cfg = Config::default();
        // Keep the test silent and headless.
        cfg.ui.notify_sound = false;
        cfg.ui.notify_desktop = false;

        // A run that was already finished when we first saw it is not announced.
        announce_if_finished(&mut st, &run_with(1, Status::Success), "o/r", &cfg);
        assert!(!st.watch_seen_running.contains(&1));

        // Seen running, then finished: tracked, then consumed exactly once.
        announce_if_finished(&mut st, &run_with(2, Status::Running), "o/r", &cfg);
        assert!(st.watch_seen_running.contains(&2));
        announce_if_finished(&mut st, &run_with(2, Status::Failure), "o/r", &cfg);
        assert!(
            !st.watch_seen_running.contains(&2),
            "a finished run must not stay tracked, or it would re-announce"
        );
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\x1b[36;1mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn bundled_sound_extracts_to_cache() {
        let path = sound_path("", FAIL_SOUND, "fail.mp3").expect("fail sound path");
        let data = std::fs::read(&path).expect("extracted fail sound");
        assert_eq!(data.as_slice(), FAIL_SOUND);

        let path = sound_path("", FINISHED_SOUND, "finished.mp3").expect("finished sound path");
        let data = std::fs::read(&path).expect("extracted finished sound");
        assert_eq!(data.as_slice(), FINISHED_SOUND);
    }

    #[test]
    fn configured_sound_takes_precedence() {
        assert_eq!(
            sound_path("/custom/boom.wav", FAIL_SOUND, "fail.mp3").as_deref(),
            Some("/custom/boom.wav")
        );
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
