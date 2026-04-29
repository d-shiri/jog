use anyhow::{Context, Result};
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

use crate::app::state::{AppState, TriggerPrompt, View};
use crate::config::Config;
use crate::provider::github::{GitHubProvider, current_branch};
use crate::provider::{Provider, Run, RunDetail, Status, Workflow};

mod views;

pub enum AppEvent {
    WorkflowStatus(String, Option<Run>),
    RunsLoaded(String, Vec<Run>),
    RunDetailLoaded(RunDetail),
    LogsLoaded(Vec<String>),
    Status(String),
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
    let mut state = AppState::new(repo_label, branch, sort_with_favorites(workflows, &config));

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
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    spawn_initial_status_fetches(state, &provider, &tx);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(config.ui.poll_interval_ms.max(500)));
    tick.tick().await; // discard first immediate tick

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
                    if let Some(action) = handle_key(state, key, &provider, &tx).await {
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
                    AppEvent::WorkflowStatus(file, run) => {
                        if let Some(w) = state.workflows.iter_mut().find(|w| w.file_name == file) {
                            w.last_status = run.as_ref().map(|r| r.status);
                            w.last_run_at = run.map(|r| r.updated_at);
                        }
                    }
                    AppEvent::RunsLoaded(file, runs) => {
                        if state.workflow_for_runs.as_deref() == Some(file.as_str()) {
                            state.runs = runs;
                            state.run_cursor = 0;
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RunDetailLoaded(detail) => {
                        state.run_detail = Some(detail);
                        state.job_cursor = 0;
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::LogsLoaded(lines) => {
                        state.log_lines = lines;
                        state.log_scroll = 0;
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::Status(msg) => {
                        state.status_msg = Some(msg);
                    }
                }
            }
            _ = tick.tick() => {
                if state.view == View::Watch {
                    // Refresh the runs list so a freshly triggered run replaces
                    // the previous one as `runs.first()`. Without this, the
                    // tick below keeps polling the OLD run id forever.
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

async fn handle_key(
    state: &mut AppState,
    key: KeyEvent,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> Option<AppEvent> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(AppEvent::Quit);
    }
    // While typing into a trigger-prompt field, route ALL keys to the editor
    // so 'q', 'j', 't', etc. are treated as text input rather than commands.
    if state.view == View::TriggerPrompt
        && state
            .trigger_prompt
            .as_ref()
            .map(|p| p.editing)
            .unwrap_or(false)
    {
        handle_trigger_prompt_edit(state, key);
        return None;
    }
    match (state.view, key.code) {
        (_, KeyCode::Char('q')) => return Some(AppEvent::Quit),
        (_, KeyCode::Esc) => match state.view {
            View::Workflows => return Some(AppEvent::Quit),
            View::Runs => {
                state.switch_view(View::Workflows);
                state.runs.clear();
                state.workflow_for_runs = None;
            }
            View::RunDetail | View::Watch => {
                state.switch_view(View::Runs);
                state.run_detail = None;
            }
            View::Logs => {
                state.switch_view(View::RunDetail);
                state.log_lines.clear();
            }
            View::TriggerPrompt => {
                cancel_trigger_prompt(state);
            }
        },
        (View::Workflows, KeyCode::Char('j') | KeyCode::Down) => {
            move_cursor(&mut state.workflow_cursor, state.workflows.len(), 1);
        }
        (View::Workflows, KeyCode::Char('k') | KeyCode::Up) => {
            move_cursor(&mut state.workflow_cursor, state.workflows.len(), -1);
        }
        (View::Workflows, KeyCode::Enter) => {
            if let Some(w) = state.selected_workflow().cloned() {
                state.switch_view(View::Runs);
                state.workflow_for_runs = Some(w.file_name.clone());
                state.runs.clear();
                spawn_fetch_runs(provider.clone(), w.file_name, tx.clone(), state);
            }
        }
        (View::Workflows, KeyCode::Char('t')) => {
            trigger_workflow_at_cursor(state, provider, tx);
        }
        (View::Workflows, KeyCode::Char('w')) => {
            if let Some(w) = state.selected_workflow().cloned() {
                state.switch_view(View::Watch);
                state.workflow_for_runs = Some(w.file_name.clone());
                state.runs.clear();
                spawn_fetch_runs(provider.clone(), w.file_name, tx.clone(), state);
            }
        }
        (View::Workflows, KeyCode::Char('o')) => {
            if let Some(w) = state.selected_workflow().cloned() {
                let p = provider.clone();
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    if let Ok(Some(run)) = p.get_latest_run(&w.file_name).await {
                        let _ = open::that(run.url.clone());
                        let _ = tx2.send(AppEvent::Status(format!("opened run {}", run.id)));
                    }
                });
            }
        }
        (View::Runs, KeyCode::Char('j') | KeyCode::Down) => {
            move_cursor(&mut state.run_cursor, state.runs.len(), 1);
        }
        (View::Runs, KeyCode::Char('k') | KeyCode::Up) => {
            move_cursor(&mut state.run_cursor, state.runs.len(), -1);
        }
        (View::Runs, KeyCode::Enter) => {
            if let Some(r) = state.selected_run().cloned() {
                state.switch_view(View::RunDetail);
                spawn_fetch_run_detail(provider.clone(), r.id, tx.clone(), state);
            }
        }
        (View::Runs, KeyCode::Char('w')) => {
            state.switch_view(View::Watch);
        }
        (View::Runs, KeyCode::Char('t')) => {
            // Trigger a fresh run of the workflow we're viewing.
            if let Some(file) = state.workflow_for_runs.clone() {
                if let Some(w) = state.workflows.iter().find(|w| w.file_name == file).cloned() {
                    trigger_workflow(state, &w, provider, tx);
                }
            }
        }
        (View::Runs, KeyCode::Char('x')) => {
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
        }
        (View::Runs, KeyCode::Char('r')) => {
            // lowercase r = rerun all jobs in the selected run
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
        }
        (View::Runs, KeyCode::Char('R')) => {
            // uppercase R = rerun only failed jobs
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
        (View::RunDetail, KeyCode::Char('j') | KeyCode::Down) => {
            let max = state.run_detail.as_ref().map(|d| d.jobs.len()).unwrap_or(0);
            move_cursor(&mut state.job_cursor, max, 1);
        }
        (View::RunDetail, KeyCode::Char('k') | KeyCode::Up) => {
            let max = state.run_detail.as_ref().map(|d| d.jobs.len()).unwrap_or(0);
            move_cursor(&mut state.job_cursor, max, -1);
        }
        (View::RunDetail, KeyCode::Enter | KeyCode::Char('l')) => {
            if let Some(detail) = &state.run_detail {
                if let Some(job) = detail.jobs.get(state.job_cursor).cloned() {
                    state.switch_view(View::Logs);
                    state.log_lines = vec!["loading...".into()];
                    spawn_fetch_logs(provider.clone(), job.id, tx.clone(), state);
                }
            }
        }
        (View::Logs, KeyCode::Char('j') | KeyCode::Down) => {
            state.log_scroll = state.log_scroll.saturating_add(1);
        }
        (View::Logs, KeyCode::Char('k') | KeyCode::Up) => {
            state.log_scroll = state.log_scroll.saturating_sub(1);
        }
        (View::Logs, KeyCode::PageDown | KeyCode::Char('d')) => {
            state.log_scroll = state.log_scroll.saturating_add(20);
        }
        (View::Logs, KeyCode::PageUp | KeyCode::Char('u')) => {
            state.log_scroll = state.log_scroll.saturating_sub(20);
        }
        (View::Logs, KeyCode::Char('g')) => {
            state.log_scroll = 0;
        }
        (View::TriggerPrompt, KeyCode::Char('j') | KeyCode::Down) => {
            if let Some(p) = state.trigger_prompt.as_mut() {
                let len = p.fields.len();
                move_cursor(&mut p.cursor, len, 1);
            }
        }
        (View::TriggerPrompt, KeyCode::Char('k') | KeyCode::Up) => {
            if let Some(p) = state.trigger_prompt.as_mut() {
                let len = p.fields.len();
                move_cursor(&mut p.cursor, len, -1);
            }
        }
        (View::TriggerPrompt, KeyCode::Char(' ')) => {
            if let Some(p) = state.trigger_prompt.as_mut() {
                p.cycle_option();
            }
        }
        (View::TriggerPrompt, KeyCode::Enter | KeyCode::Char('i')) => {
            if let Some(p) = state.trigger_prompt.as_mut() {
                if p.current_field().is_some() {
                    p.editing = true;
                }
            }
        }
        (View::TriggerPrompt, KeyCode::Char('t')) => {
            submit_trigger_prompt(state, provider, tx);
        }
        _ => {}
    }
    None
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

fn spawn_initial_status_fetches(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    for w in &state.workflows {
        let file = w.file_name.clone();
        let p = provider.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let latest = p.get_latest_run(&file).await.ok().flatten();
            let _ = tx2.send(AppEvent::WorkflowStatus(file, latest));
        });
    }
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
                let _ = tx.send(AppEvent::Status(format!("get run failed: {e}")));
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
                let _ = tx.send(AppEvent::Status(format!("logs failed: {e}")));
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
        state.status_msg = Some(format!(
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
    state.status_msg = Some(format!(
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
        state.status_msg = Some(format!("missing required: {}", missing.join(", ")));
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
