use anyhow::{Context, Result, anyhow};
use chrono::Timelike;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
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
    AppState, BatchCommit, BatchPhase, DetailItem, FailureDigest, Finder, FinderKind, GitOp, Hit,
    PushPrompt, PushWatch, RepoCard, Theme, TriggerPrompt, View, build_detail_items,
    classify_log_severity,
};
use crate::config::KeymapConfig;
use crate::config::{Config, NotifyMode};
use crate::history::History;
use crate::provider::github::{
    ApiError, ApiFault, GitHubProvider, Quota, RepoSpec, classify_error, current_branch,
};
use crate::provider::{PrInfo, Provider, Run, RunDetail, Status, Step, Workflow};

mod motion;
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
    RepoCardFailed(String, ApiError),
    /// What is left of the hour's API budget. `None` when even that could not
    /// be read — the meter goes blank rather than lying about a stale figure.
    QuotaLoaded(Option<Quota>),
    /// Jobs and steps of the run currently in flight on one dashboard row —
    /// what the activity strip needs to name the step, not just the workflow.
    RepoProgressLoaded(String, RunDetail),
    /// Working-tree state for a local checkout.
    GitStatusLoaded(String, crate::git::RepoStatus),
    /// The combined working-tree diff: repo spec, then every changed file with
    /// its sections, in status order.
    GitDiffLoaded(String, Vec<(String, Vec<crate::git::DiffSection>)>),
    /// A git command finished. `Some(msg)` reports success, `Err` the failure;
    /// either way the status is refreshed afterwards.
    GitOpDone(String, Result<String, String>),
    /// Live output from a running commit/push — mostly hook output. Batched
    /// rather than one event per line: every event redraws, and a pytest run
    /// would otherwise repaint the screen a few thousand times.
    GitOpOutput(String, Vec<(String, bool)>),
    /// Which hook, if any, the command that just started will run. Resolved off
    /// the UI thread, so it names the pane a moment after it opens.
    GitOpStarted(String, Option<String>),
    /// The open PR for a branch the git view is sitting on: repo spec, the
    /// branch the question was asked about, and the answer.
    PrLoaded(String, String, Option<PrInfo>),
    /// Recent runs of a repo whose push is being followed into CI.
    PushWatchRuns(String, Vec<Run>),
    /// The log-so-far of the running job the Watch view is following: job id,
    /// job name, and the tail of its lines. `None` when GitHub has accepted
    /// the job but has no log text to serve yet.
    WatchTailLoaded(u64, String, Option<Vec<String>>),
    /// Service health from the Uptime Kuma status page, or why it couldn't be
    /// read.
    KumaLoaded(Result<Vec<crate::kuma::Service>, String>),
    /// The extracted "why it failed" window for a failed run open in the
    /// detail view — see [`FailureDigest`]. Empty `lines` means the log had
    /// nothing to say (or could not be fetched); the pane just stays absent.
    DigestLoaded {
        run_id: u64,
        job: String,
        step: Option<String>,
        lines: Vec<String>,
        errors: Vec<usize>,
    },
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
    if let Some(icon) = config.ui.github_icon.clone() {
        state.forge_icon = icon;
    }
    if let Some(icon) = config.ui.bell_icon.clone() {
        state.bell_icon = icon;
    }
    if let Some(icon) = config.ui.bell_off_icon.clone() {
        state.bell_off_icon = icon;
    }
    state.file_icons = config.ui.file_icons;
    state.notify_enabled = config.ui.notify_mode() != NotifyMode::Never
        && (config.ui.notify_sound || config.ui.notify_desktop);
    resolve_theme(&mut state, &config);
    state.workspace_root = opts.workspace_root.clone();
    state.repos = if opts.workspace.is_empty() {
        dashboard_repos(&config, &state.repo_label)
    } else {
        // Workspace mode: `repo_label` is whichever scanned repo happened to
        // have a usable remote, not somewhere the user asked to be.
        state.repo_label_implicit = true;
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
        // The dashboard's rows sweep in from the top on the first frame, the
        // same entrance they play on every later return to the view.
        state.dash_opened_tick = Some(state.tick_count);
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
    group_by_owner(specs.into_iter().map(RepoCard::new).collect())
}

/// Gather each owner's repos together, owners in the order they first appear.
///
/// The dashboard groups rows under an owner heading, and a heading can only
/// describe a contiguous run — a list that goes muufree, drposture, muufree
/// would grow two muufree headings and imply a structure that isn't there.
///
/// A stable sort rather than an alphabetical one: whatever order the repos were
/// configured or discovered in is a choice someone made, and this disturbs it
/// only as much as grouping requires.
fn group_by_owner(mut cards: Vec<RepoCard>) -> Vec<RepoCard> {
    let mut order: Vec<&str> = Vec::new();
    let rank: Vec<usize> = cards
        .iter()
        .map(|c| {
            let owner = c.spec.split_once('/').map(|(o, _)| o).unwrap_or("");
            match order.iter().position(|o| *o == owner) {
                Some(i) => i,
                None => {
                    order.push(owner);
                    order.len() - 1
                }
            }
        })
        .collect();
    let mut with_rank: Vec<(usize, RepoCard)> = rank.into_iter().zip(cards.drain(..)).collect();
    with_rank.sort_by_key(|(r, _)| *r);
    with_rank.into_iter().map(|(_, c)| c).collect()
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
    group_by_owner(cards)
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

/// Pick the palette: `[ui] theme`, then `[ui.colors]` on top, then fold the
/// result down if the terminal can't do 24-bit colour.
///
/// Every problem here is reported rather than swallowed. A colour setting that
/// silently does nothing is indistinguishable from a broken one, and the user
/// has no way to tell which they are looking at.
fn resolve_theme(state: &mut AppState, config: &Config) {
    let mut notes: Vec<String> = Vec::new();
    state.theme = match Theme::by_name(&config.ui.theme) {
        Some(t) => t,
        None => {
            notes.push(format!(
                "unknown theme {:?} — try {}",
                config.ui.theme,
                Theme::NAMES.join(", ")
            ));
            Theme::default()
        }
    };
    let rejected = state.theme.apply_overrides(&config.ui.colors);
    if !rejected.is_empty() {
        notes.push(format!("[ui.colors]: ignored {}", rejected.join(", ")));
    }
    if !crate::app::state::terminal_has_truecolor() {
        state.theme.degrade_to_256();
    }
    if !notes.is_empty() {
        state.set_status(notes.join(" · "));
    }
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
    state.kuma = config.uptime_kuma.clone();
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
    state.poll_ticks = (poll_interval.as_millis() / 100).max(1) as u64;
    tick.tick().await;

    // Whether the coming frame is worth painting. Every keypress and every
    // task reply says yes; the 100ms tick only says yes while something on
    // screen is actually moving, plus once a second so the header's clocks
    // stay honest. An idle dashboard used to rebuild the whole frame ten
    // times a second to display a screen that was not changing.
    let mut redraw = true;
    loop {
        if state.needs_clear {
            terminal.clear()?;
            state.needs_clear = false;
            redraw = true;
        }
        announce_prompt(state, &config);
        batch_auto_return(state);
        if redraw {
            terminal.draw(|f| views::render(f, state))?;
        }
        redraw = true;

        tokio::select! {
            maybe_evt = events.next() => {
                match maybe_evt {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if let Some(action) = handle_key(state, key, &mut provider, &tx, &km).await
                            && let AppEvent::Quit = action { return Ok(()) }
                    }
                    // The wheel is the same gesture as ↑/↓, three rows per
                    // notch. Routed through handle_key so whatever the arrow
                    // keys drive in the current view — hook output, a diff, a
                    // log, a list — the wheel drives too.
                    Some(Ok(Event::Mouse(m))) => {
                        let code = match m.kind {
                            MouseEventKind::ScrollDown => Some(KeyCode::Down),
                            MouseEventKind::ScrollUp => Some(KeyCode::Up),
                            _ => None,
                        };
                        if let Some(code) = code {
                            for _ in 0..3 {
                                let key = KeyEvent::new(code, KeyModifiers::NONE);
                                // ↑/↓ never map to Quit, so the action is moot.
                                let _ = handle_key(state, key, &mut provider, &tx, &km).await;
                            }
                        }
                        // A click is the same gesture as moving to a row and
                        // pressing Enter — selecting on the first click,
                        // opening on a click at the selection.
                        if m.kind == MouseEventKind::Down(MouseButton::Left) {
                            handle_click(state, m.column, m.row, &mut provider, &tx, &km).await;
                        }
                    }
                    _ => {}
                }
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
                            // Hold the selection across the refresh, by run id
                            // rather than by position. The Runs view re-fetches
                            // on every poll while anything is in flight — which
                            // is precisely when the list is worth reading — so
                            // resetting to the top here threw the cursor off
                            // whatever had been scrolled to, every few seconds.
                            //
                            // Watch is the exception and keeps the old rule: it
                            // follows the newest run by definition.
                            let held = (state.view != View::Watch)
                                .then(|| state.selected_run().map(|r| r.id))
                                .flatten();
                            state.runs = runs;
                            state.run_cursor = held
                                .and_then(|id| state.runs.iter().position(|r| r.id == id))
                                .unwrap_or(0);
                            // The preview belongs to the cursor, not to the head
                            // of the list.
                            if let Some(r) = state.runs.get(state.run_cursor).cloned() {
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
                        // A failed run opens with the cursor already on what
                        // failed: the first broken step is the only row anyone
                        // opens a red run to see, and Enter from there is its
                        // log. A green run starts at the top as before.
                        state.detail_cursor = if detail.run.status.is_failure() {
                            first_failed_item(&detail).unwrap_or(0)
                        } else {
                            0
                        };
                        state.run_detail = Some(detail);
                        state.pending = state.pending.saturating_sub(1);
                        maybe_spawn_digest(&provider, state, &tx);
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
                        land_on_first_error(state);
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RepoCardLoaded(spec, runs) => {
                        // Announce finished runs from any dashboard row, not just
                        // the active repo — watching several repos at once is the
                        // whole point of the dashboard.
                        for r in &runs {
                            announce_if_finished(state, r, &spec, &config);
                        }
                        let tick = state.tick_count;
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            // Mark the row as moved only when its CI actually
                            // moved. The first load is not a change — every row
                            // flashing at startup would train the eye to ignore
                            // the one thing the flash is for.
                            let before = card.runs.first().map(|r| (r.id, r.status));
                            let after = runs.first().map(|r| (r.id, r.status));
                            if card.loaded && before != after {
                                card.changed_tick = Some(tick);
                            }
                            // A landing outranks a mere change: a run this row
                            // had in flight is now terminal, and how it ended
                            // is worth a longer, coloured breath.
                            let landed = card
                                .active_runs()
                                .find_map(|old| {
                                    runs.iter()
                                        .find(|n| n.id == old.id && n.status.is_terminal())
                                        .map(|n| n.status)
                                });
                            if card.loaded && let Some(verdict) = landed {
                                card.settled_tick = Some((tick, verdict));
                            }
                            card.runs = runs;
                            card.error = None;
                            card.loaded = true;
                        }
                        note_all_green(state);
                        // A row answered with no hold running means the trouble
                        // is over, so the next refusal starts at the bottom of
                        // the ladder rather than the top of the last one. A hold
                        // still running is left to expire on its own: one round
                        // can have rows answered and rows refused, and cutting
                        // the wait short walks straight back into the wall.
                        if !state.api_held() {
                            state.clear_api_hold();
                        }
                        sync_repo_progress(&provider, state, &spec, &tx);
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::RepoCardFailed(spec, err) => {
                        // "rate limited" alone leaves the one question it raises
                        // unanswered, and the rejection doesn't carry the clock.
                        // Re-read rather than trust the last poll's figure:
                        // being turned away is itself news about the budget.
                        match err.fault {
                            ApiFault::RateLimited => {
                                spawn_quota_probe(&provider, state, &tx);
                                // The hour's budget is gone: the reset is the
                                // only moment worth asking again at, and it is
                                // the one figure the rejection didn't carry.
                                let reset = state.quota.map(|q| q.reset);
                                state.hold_api(reset);
                            }
                            // Pace, not budget. Nobody told us how long, so the
                            // ladder guesses — and stops guessing on the first
                            // poll that lands.
                            ApiFault::Throttled => state.hold_api(None),
                            _ => {}
                        }
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            card.error = Some(err);
                            card.loaded = true;
                        }
                        // A row we can no longer fetch has nothing to report as
                        // in flight; leaving the old entry would strand a
                        // spinner that never advances.
                        state.run_progress.remove(&spec);
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::QuotaLoaded(q) => {
                        // Cleared even when the answer is `None`: a read that
                        // failed must not wedge the flag and mute every retry.
                        state.quota_pending = false;
                        if let Some(q) = q {
                            // The reading the refusal sent us for. If the hour's
                            // budget really is gone, the reset is the first
                            // moment anything can work — sooner than that the
                            // ladder would spend requests proving it again.
                            if state.api_held() && q.used >= q.limit {
                                state.hold_api(Some(q.reset));
                            }
                            if record_quota(state, q) && !state.notify_snoozed() {
                                play_quota_alarm(&config);
                            }
                        }
                    }
                    AppEvent::RepoProgressLoaded(spec, detail) => {
                        // Only fold in detail for a run the row is still waiting
                        // on: a push during the fetch means this answer
                        // describes a run that has already been superseded.
                        if let Some(ds) = state.run_progress.get_mut(&spec)
                            && let Some(i) = ds.iter().position(|d| d.run.id == detail.run.id)
                        {
                            if detail.run.status.is_terminal() {
                                ds.remove(i);
                            } else {
                                ds[i] = detail;
                            }
                            // Never leave an empty list behind: the strip asks
                            // whether the key is there at all.
                            if ds.is_empty() {
                                state.run_progress.remove(&spec);
                            }
                        }
                        note_all_green(state);
                    }
                    AppEvent::GitStatusLoaded(spec, status) => {
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            card.git = Some(status.clone());
                        }
                        let mut ask_pr: Option<(String, String)> = None;
                        if let Some(gv) = state.git_view.as_mut()
                            && gv.spec == spec
                        {
                            // The branch is known now, so the PR question is
                            // askable. Once per branch, not once per refresh:
                            // the answer only moves when the branch does, or
                            // when a push clears it to force a re-ask.
                            if gv.has_ci
                                && !status.detached
                                && gv.pr.as_ref().is_none_or(|(b, _)| *b != status.branch)
                            {
                                gv.pr = Some((status.branch.clone(), None));
                                ask_pr = Some((gv.spec.clone(), status.branch.clone()));
                            }
                            // Keep the cursor in range as files leave the list.
                            gv.cursor = gv.cursor.min(status.entries.len().saturating_sub(1));
                            gv.status = Some(status);
                            gv.busy = false;
                        }
                        if let Some((spec, branch)) = ask_pr {
                            spawn_fetch_pr(state, &provider, &tx, &spec, &branch);
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::PrLoaded(spec, branch, pr) => {
                        if let Some(gv) = state.git_view.as_mut()
                            && gv.spec == spec
                        {
                            gv.pr = Some((branch, pr));
                        }
                    }
                    AppEvent::PushWatchRuns(spec, runs) => {
                        state.pending = state.pending.saturating_sub(1);
                        settle_push_watch(state, &spec, &runs, &config);
                    }
                    AppEvent::DigestLoaded { run_id, job, step, lines, errors } => {
                        if state.digest_pending == Some(run_id) {
                            state.digest_pending = None;
                        }
                        if !lines.is_empty() {
                            state.failure_digest = Some(FailureDigest {
                                run_id,
                                job_name: job,
                                step_name: step,
                                lines,
                                error_rows: errors,
                            });
                        }
                    }
                    AppEvent::WatchTailLoaded(job_id, job_name, lines) => {
                        state.watch_tail_pending = false;
                        // Track refusals per job so a log GitHub will never
                        // serve stops being asked for. A body that does arrive
                        // clears the count — the same job answering again is
                        // the archive having landed.
                        match &lines {
                            Some(_) => {
                                state.watch_tail_misses.remove(&job_id);
                            }
                            None => {
                                let n = state.watch_tail_misses.entry(job_id).or_insert(0);
                                *n = n.saturating_add(1);
                            }
                        }
                        // Only worth holding while a view showing a tail is
                        // asking: Watch's pane, or the dashboard's side pane.
                        if matches!(state.view, View::Watch | View::Repos) {
                            // "Nothing to serve yet" keeps what the last fetch
                            // showed rather than blanking a pane the user is
                            // reading — the endpoint flickers between the two
                            // while the archive is being assembled.
                            let prev = state.watch_tail.take().filter(|t| t.job_id == job_id);
                            state.watch_tail = match (lines, prev) {
                                (Some(lines), _) => Some(crate::app::state::WatchTail {
                                    job_id,
                                    job_name,
                                    lines,
                                }),
                                (None, prev) => prev,
                            };
                        }
                    }
                    AppEvent::GitDiffLoaded(spec, files) => {
                        // Only fold in a diff that still matches the repo the
                        // view is asking for: switching repos quickly can land
                        // two responses out of order.
                        if let Some(dv) = state.git_diff.as_mut()
                            && dv.spec == spec
                        {
                            dv.set_files(files);
                        }
                        state.pending = state.pending.saturating_sub(1);
                    }
                    AppEvent::GitOpStarted(spec, hook) => {
                        if let Some(op) = state.git_ops.get_mut(&spec)
                            && !op.finished
                        {
                            op.hook = hook;
                        }
                    }
                    AppEvent::GitOpOutput(spec, lines) => {
                        if let Some(op) = state.git_ops.get_mut(&spec) {
                            for (line, partial) in lines {
                                op.push_line(line, partial);
                            }
                        }
                    }
                    AppEvent::GitOpDone(spec, result) => {
                        let failed = result.is_err();
                        if let Some(gv) = state.git_view.as_mut()
                            && gv.spec == spec
                        {
                            gv.busy = false;
                        }
                        let viewport = state.last_op_viewport_height.get().max(1) as usize;
                        // Read before the entry is removed below: what the op
                        // was is the difference between a commit worth offering
                        // to push and a `git add` that changed nothing outside.
                        let verb = state.git_ops.get(&spec).map(|o| o.verb.clone());
                        let had_op = match state.git_ops.entry(spec.clone()) {
                            std::collections::hash_map::Entry::Occupied(mut e) => {
                                if failed {
                                    let op = e.get_mut();
                                    op.finished = true;
                                    op.failed = true;
                                    // Land on the first error rather than the
                                    // tail. A pytest tail is a summary count;
                                    // the assertion is further up.
                                    op.scroll_to_top();
                                    op.jump_error(true, viewport);
                                } else {
                                    // Succeeded: the status line carries the
                                    // summary and there is nothing left to read.
                                    e.remove();
                                }
                                true
                            }
                            std::collections::hash_map::Entry::Vacant(_) => false,
                        };
                        // Only commit/push are in the map, and only those are
                        // worth a sound: a failed `git add` is instant and its
                        // message is already on screen.
                        if failed && had_op && !state.notify_snoozed() {
                            play_op_failed_sound(&config);
                        }
                        // A commit landing is the other thing worth flashing a
                        // row for — it is the moment that repo's state changed,
                        // and it may not be the row you are looking at.
                        let tick = state.tick_count;
                        if let Some(card) = state.repos.iter_mut().find(|c| c.spec == spec) {
                            card.changed_tick = Some(tick);
                        }
                        // Before the status line, so a batch that has more repos
                        // to do overwrites it with what it moved on to.
                        batch_on_op_done(state, &tx, &spec, &result);
                        match result {
                            // "Nothing to commit" is a refusal, not a result —
                            // it stays plain rather than claiming a success.
                            Ok(msg) => match msg.strip_prefix(BATCH_NOTHING) {
                                Some(why) => state.set_status(format!("{spec}: {why}")),
                                None => state.set_status_ok(msg),
                            },
                            Err(err) => state.set_status_err(err),
                        }
                        state.pending = state.pending.saturating_sub(1);
                        if !failed && verb.as_deref() == Some("commit") {
                            offer_push(state, &spec);
                        }
                        if !failed && verb.as_deref() == Some("push") {
                            // The push is the moment jog's job starts, not
                            // ends: follow it into CI and announce how that
                            // goes, whatever view is on screen by then.
                            start_push_watch(state, &spec);
                            if let Some(gv) = state.git_view.as_mut()
                                && gv.spec == spec
                            {
                                // A first push publishes the branch, which is
                                // the moment a PR becomes possible — say so,
                                // since nothing on GitHub will.
                                let published = gv
                                    .status
                                    .as_ref()
                                    .is_some_and(|s| !s.has_upstream && !s.detached);
                                // Whatever the push changed about the PR
                                // question, ask it again.
                                gv.pr = None;
                                if published {
                                    let key = state.keymap.open_browser.clone();
                                    state.set_status_ok(format!(
                                        "branch published — {key} opens a pull request"
                                    ));
                                }
                            }
                        }
                        // The working tree moved; re-read it.
                        if let Some(gv) = state.git_view.as_ref() {
                            let (spec, path) = (gv.spec.clone(), gv.path.clone());
                            spawn_git_status(spec, path, tx.clone(), state);
                        }
                        // Staging splits a file's changes differently between
                        // index and working tree, so an open diff of it is now
                        // describing the old arrangement.
                        if state.git_diff.is_some() {
                            open_git_diff(state, &tx);
                        }
                    }
                    AppEvent::RepoSwitched { label, branch, workflows } => {
                        state.repo_label = label;
                        state.repo_label_implicit = false;
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
                    AppEvent::KumaLoaded(result) => {
                        state.kuma_pending = false;
                        match result {
                            Ok(services) => {
                                // Resolve which row each monitor decorates:
                                // explicit map first, then name-matching. Redone
                                // on every answer because both sides can move —
                                // monitors get added, repos get switched.
                                let specs: Vec<String> =
                                    state.repos.iter().map(|c| c.spec.clone()).collect();
                                let explicit = config
                                    .uptime_kuma
                                    .as_ref()
                                    .map(|k| k.map.clone())
                                    .unwrap_or_default();
                                state.service_repos = services
                                    .iter()
                                    .filter_map(|s| {
                                        crate::kuma::repo_for_service(&s.name, &explicit, &specs)
                                            .map(|repo| (s.name.clone(), repo))
                                    })
                                    .collect();
                                state.services = services;
                                state.kuma_fetched_at = Some(chrono::Utc::now());
                                // Fresh readings landing on an open card get
                                // the same entrance the card itself had: the
                                // verdicts on screen are new, and a silent
                                // swap would let a changed one go unnoticed.
                                if state.show_services {
                                    state.services_opened_tick = Some(state.tick_count);
                                }
                            }
                            // Said once, not once per poll: a URL typo and a
                            // server reboot read the same from here, and the
                            // stale readings (if any) stay on screen either way.
                            Err(e) => {
                                if !state.kuma_error_shown {
                                    state.set_status_err(format!("uptime kuma: {e}"));
                                    state.kuma_error_shown = true;
                                }
                            }
                        }
                    }
                    AppEvent::Status(msg) => {
                        state.set_status(msg);
                    }
                    AppEvent::TaskError(msg) => {
                        state.set_status_err(msg);
                        state.pending = state.pending.saturating_sub(1);
                    }
                }
            }
            _ = tick.tick() => {
                state.tick_count += 1;
                // Animations earn the 10fps; a still screen redraws once a
                // second, which is what keeps the header clock and the poll
                // countdown moving. Every non-tick branch redraws regardless.
                redraw = state.tick_count % 10 == 0
                    || animations_active(state)
                    || views::services_revealing(state)
                    || views::help_revealing(state)
                    || views::dash_revealing(state)
                    || views::diff_bands_revealing(state);
                if state.status_msg.is_some() && state.tick_count.saturating_sub(state.status_msg_tick) > 30 {
                    state.status_msg = None;
                    redraw = true;
                }
                let now = tokio::time::Instant::now();
                if now.duration_since(last_poll) >= poll_interval {
                    last_poll = now;
                    state.last_poll_tick = state.tick_count;
                    // Outside the view match below: every view spends from the
                    // same bucket, and a meter that stops moving when you walk
                    // into the logs is worse than none — that is exactly where
                    // the budget goes. It also keeps going while everything else
                    // is held off after a refusal: at most one request per
                    // poll, exempt from the limit it reports, and the meter is
                    // how you tell which of the two limits you are waiting on.
                    spawn_quota_probe(&provider, state, &tx);
                    // Same standing as the quota probe: every view cares
                    // whether production is up, it spends nothing from the
                    // GitHub budget, and an API hold is no reason to stop
                    // watching a different server entirely.
                    spawn_kuma_probe(state, &tx, false);
                    // The finder holds indices into the list it was opened over.
                    // Refreshing that list underneath it would shift every index,
                    // so Enter would commit to whatever slid into the slot.
                    //
                    // A commit sitting in a 90-second `pre-commit` hook counts
                    // towards `pending` — it should keep the footer spinning —
                    // but it fetches nothing and changes no CI state, so it must
                    // not pause polling: a run finishing in another repo would
                    // go unannounced until the hook was done.
                    let in_hooks = state.git_ops.values().filter(|o| !o.finished).count();
                    // Turned away for asking too fast: asking again on schedule
                    // is what keeps us there. The dashboard's own half of the
                    // poll — every repo's working tree — costs nothing and
                    // carries on inside `spawn_fetch_repo_cards`.
                    let held = state.api_held();
                    if state.pending == in_hooks && state.finder.is_none() {
                        match state.view {
                            View::Repos => {
                                spawn_fetch_repo_cards(&provider, state, &tx);
                                // The side pane's live tail, only while the
                                // terminal is wide enough to be showing one.
                                if !held {
                                    spawn_dash_tail(&provider, state, &tx);
                                }
                            }
                            _ if held => {}
                            View::Watch => {
                                if let Some(file) = state.workflow_for_runs.clone() {
                                    spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
                                }
                                if let Some(run) = state.runs.first().cloned() {
                                    spawn_fetch_run_detail(provider.clone(), run.id, tx.clone(), state);
                                }
                                spawn_watch_tail(&provider, state, &tx);
                            }
                            View::Runs => {
                                let has_active = state.runs.iter().any(|r| !r.status.is_terminal());
                                if has_active
                                    && let Some(file) = state.workflow_for_runs.clone()
                                {
                                    spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
                                }
                            }
                            View::RunDetail => {
                                let run_active = state
                                    .run_detail
                                    .as_ref()
                                    .map(|d| !d.run.status.is_terminal())
                                    .or_else(|| state.selected_run().map(|r| !r.status.is_terminal()))
                                    .unwrap_or(false);
                                if run_active
                                    && let Some(run) = state.selected_run().cloned()
                                {
                                    spawn_fetch_run_detail(provider.clone(), run.id, tx.clone(), state);
                                }
                            }
                            _ => {}
                        }
                        // View-independent: a pushed repo is followed into CI
                        // even while its owner reads some other repo's logs.
                        if !held {
                            poll_push_watches(state, &provider, &tx);
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
    // The push question the commit just raised, likewise: it is on screen
    // because it is the next thing to decide.
    if state.push_prompt.is_some() {
        handle_push_prompt(state, key, tx);
        return None;
    }
    // Same for the one message a batch commit is about to apply everywhere.
    if state.view == View::BatchCommit
        && state.batch.as_ref().is_some_and(|b| b.input.is_some())
    {
        handle_batch_input(state, key, tx);
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
    // Esc/Backspace clears an active log query before falling through to view-back.
    if state.view == View::Logs
        && is_back_fallback(&key)
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
        handle_help_key(state, key, km);
        return None;
    }

    // The services overlay is a card, not a document: nothing in it scrolls,
    // so any key puts it away — except refresh, which re-asks Kuma right now
    // instead of waiting out the cadence.
    if state.show_services {
        if key_is(&key, km.refresh) {
            spawn_kuma_probe(state, tx, true);
            return None;
        }
        state.show_services = false;
        state.services_opened_tick = None;
        state.needs_clear = true;
        return None;
    }

    // Global: keybinding reference.
    if key_is(&key, km.help) {
        state.show_help = true;
        state.help_scroll = 0;
        state.help_search.clear();
        state.help_typing = false;
        // The entrance is timed from the keypress, like the services card's —
        // and once help has been found, the footer's beacon retires.
        state.help_opened_tick = Some(state.tick_count);
        state.help_seen = true;
        return None;
    }

    // Global: ask for whatever this screen shows, again. One key, one meaning,
    // on every view — the card that re-fetches its monitors above is the same
    // key doing the same thing, and a dashboard that only ever refreshed on
    // its own schedule was the odd one out.
    if key_is(&key, km.refresh) {
        refresh_current(state, provider, tx);
        return None;
    }

    // Global: service health by name, wherever you are.
    if key_is(&key, km.services) {
        state.show_services = true;
        // The reveal is timed from the keypress, so the card plays its
        // entrance every time it is asked for rather than once a session.
        state.services_opened_tick = Some(state.tick_count);
        return None;
    }

    // Global: snooze notifications — 30m, then 60m, then back on.
    if key_is(&key, km.snooze) {
        toggle_snooze(state);
        return None;
    }

    // Global: quit
    if key_is(&key, km.quit) {
        // Quitting mid-batch would kill a `pre-commit` hook in the middle of
        // rewriting files. Asked of the *op*, not the phase: Esc stops the batch
        // but deliberately lets the repo in flight finish, and quitting during
        // that window would kill the very hook the stop promised to leave alone.
        if state.batch.as_ref().is_some_and(|b| {
            b.is_working() || b.items.iter().any(|i| state.op_running(&i.spec))
        }) {
            state.set_status("a commit is running — wait for it, or Esc then q".into());
            return None;
        }
        return Some(AppEvent::Quit);
    }

    // Global: open the fuzzy finder over whatever list the current view shows.
    if key_is(&key, km.finder) {
        open_finder(state);
        return None;
    }

    // Global: jump to the multi-repo dashboard.
    if key_is(&key, km.repos_view) && state.view != View::Repos {
        // …except while a batch is live. It is the only view that can stop or
        // resume the run, nothing else switches to it, and a batch left running
        // off-screen would commit repo after repo with no way back in.
        if batch_is_live(state) {
            state.switch_view(View::BatchCommit);
            return None;
        }
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

    // A *finished* hook-output pane takes `back` before the view does: what you
    // do after reading the failure is fix the file it pointed at, so dismissing
    // the output should not also throw you out of the repo. A command still
    // running keeps its pane instead — `back` steps out of the view and leaves
    // it to finish, and the output is waiting on the way back in.
    if (key_is(&key, km.back) || is_back_fallback(&key))
        && state.view == View::GitStatus
        && state.current_op().is_some_and(|o| o.finished)
        && !batch_owns_current_op(state)
    {
        let spec = state.git_view.as_ref().map(|g| g.spec.clone());
        if let Some(spec) = spec {
            state.git_ops.remove(&spec);
        }
        return None;
    }

    // Global: back — Esc and Backspace are always accepted as fallbacks
    // regardless of config.
    if key_is(&key, km.back) || is_back_fallback(&key) {
        match state.view {
            View::Repos => return Some(AppEvent::Quit),
            View::GitStatus => {
                state.git_view = None;
                // Leaving the repo entirely also drops any diff of its files,
                // so re-entering never opens on the previous repo's contents.
                state.git_diff = None;
                // Opened from a paused batch to fix the repo that failed: back
                // goes back to the pause, with retry and skip still on offer.
                state.switch_view(if batch_is_live(state) {
                    View::BatchCommit
                } else {
                    View::Repos
                });
            }
            View::GitDiff => {
                state.git_diff = None;
                state.switch_view(View::GitStatus);
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
            View::BatchCommit => {
                let Some(batch) = state.batch.as_mut() else {
                    state.switch_view(View::Repos);
                    return None;
                };
                match batch.phase {
                    // Stop starting repos. The one already in a hook is left to
                    // finish — a half-run `pre-commit` is worse than a slow one.
                    BatchPhase::Committing | BatchPhase::Pushing | BatchPhase::Paused => {
                        let running = batch.is_working();
                        batch.abort();
                        state.set_status(if running {
                            "batch stopped — the repo in flight will finish".into()
                        } else {
                            "batch stopped".to_string()
                        });
                    }
                    // The summary has been read; drop it and the marks with it.
                    _ => {
                        state.batch = None;
                        state.repo_marks.clear();
                        state.switch_view(View::Repos);
                    }
                }
            }
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
        if state.view == View::GitStatus {
            open_pr_in_browser(state);
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
            // With hook output on screen, the whole failure is what you want on
            // the clipboard — to paste into an editor, a bug report, or a chat.
            View::GitStatus => state.current_op().map(|o| o.text()).or_else(|| {
                state
                    .git_view
                    .as_ref()
                    .and_then(|g| g.selected())
                    .map(|e| e.path.clone())
            }),
            View::GitDiff => state
                .git_diff
                .as_ref()
                .map(|d| d.current_file().unwrap_or_else(|| d.file.clone())),
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
                .get(state.log_line_cursor)
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect()),
            View::Diff => state.run_detail.as_ref().map(|d| d.run.url.clone()),
            View::TriggerPrompt => None,
            // Whatever the batch is showing output for — the failure you are
            // about to paste into an editor or a chat.
            View::BatchCommit => state
                .batch
                .as_ref()
                .and_then(|b| b.current())
                .and_then(|i| state.git_ops.get(&i.spec))
                .map(|o| o.text()),
        };
        if let Some(text) = text {
            match yank_to_clipboard(&text) {
                Ok(()) => {
                    let preview: String = text.chars().take(50).collect();
                    let ellipsis = if text.chars().count() > 50 { "…" } else { "" };
                    state.set_status(format!("yanked: {preview}{ellipsis}"));
                }
                Err(e) => state.set_status_err(format!("yank failed: {e}")),
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
            } else if key_is(&key, km.repo_mark) {
                toggle_repo_mark(state);
            } else if key_is(&key, km.batch_commit) {
                start_batch_commit(state);
            }
        }
        View::BatchCommit => {
            // While a hook is talking, the movement keys belong to its output —
            // it is the thing that just changed, and reading it is the point.
            let op_spec = state
                .batch
                .as_ref()
                .and_then(|b| b.current())
                .map(|i| i.spec.clone());
            if let Some(spec) = op_spec
                && state.git_ops.contains_key(&spec)
                && handle_op_pane_key(state, key, km, &spec)
            {
                return None;
            }
            match state.batch.as_ref().map(|b| b.phase) {
                Some(BatchPhase::Paused) => {
                    if key_is(&key, km.batch_retry) {
                        if let Some(b) = state.batch.as_mut() {
                            b.retry();
                        }
                        batch_step(state, tx);
                    } else if key_is(&key, km.batch_skip) {
                        if let Some(b) = state.batch.as_mut() {
                            b.skip();
                        }
                        batch_step(state, tx);
                    } else if key_is(&key, km.git_view) {
                        open_git_view_for_paused(state, tx);
                    }
                }
                Some(BatchPhase::AskPush)
                    if key_is(&key, km.git_push)
                        || key_is(&key, km.confirm)
                        || key.code == KeyCode::Enter =>
                {
                    batch_start_push(state, tx);
                }
                _ => {}
            }
        }
        View::GitStatus => {
            // With hook output on screen the movement keys belong to it — it is
            // the thing that just changed, and reading it is why it is there.
            // Everything else (stage, commit again) still falls through.
            let op_spec = state.git_view.as_ref().map(|g| g.spec.clone());
            if let Some(spec) = op_spec
                && state.git_ops.contains_key(&spec)
                && handle_op_pane_key(state, key, km, &spec)
            {
                return None;
            }
            let len = state.git_view.as_ref().map(|g| g.entries().len()).unwrap_or(0);
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                if let Some(g) = state.git_view.as_mut() {
                    move_cursor(&mut g.cursor, len, 1);
                }
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                if let Some(g) = state.git_view.as_mut() {
                    move_cursor(&mut g.cursor, len, -1);
                }
            } else if key_is(&key, km.git_diff)
                || key_is(&key, km.confirm)
                || key.code == KeyCode::Enter
            {
                open_git_diff(state, tx);
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
            } else if key_is(&key, km.trigger) {
                // Hand off to CI: switch the app to this repo and show its
                // workflows, where `t` triggers as usual.
                switch_to_selected_repo(state, provider, tx);
            }
        }
        View::GitDiff => {
            let page = state.last_diff_viewport_height.get().max(1) as isize;
            let viewport = state.last_diff_viewport_height.get() as usize;
            if key_is(&key, km.down) || key.code == KeyCode::Down {
                if let Some(d) = state.git_diff.as_mut() {
                    d.scroll_by(1, viewport);
                }
            } else if key_is(&key, km.up) || key.code == KeyCode::Up {
                if let Some(d) = state.git_diff.as_mut() {
                    d.scroll_by(-1, viewport);
                }
            } else if key_is(&key, km.page_down) || key.code == KeyCode::PageDown {
                if let Some(d) = state.git_diff.as_mut() {
                    d.scroll_by(page, viewport);
                }
            } else if key_is(&key, km.page_up) || key.code == KeyCode::PageUp {
                if let Some(d) = state.git_diff.as_mut() {
                    d.scroll_by(-page, viewport);
                }
            } else if key_is(&key, km.scroll_top) {
                if let Some(d) = state.git_diff.as_mut() {
                    d.scroll.set(0);
                }
            } else if key_is(&key, km.scroll_bottom) {
                if let Some(d) = state.git_diff.as_mut() {
                    let bottom = d.max_scroll(viewport);
                    d.scroll.set(bottom);
                }
            } else if key_is(&key, km.next_step) {
                step_diff_file(state, 1);
            } else if key_is(&key, km.prev_step) {
                step_diff_file(state, -1);
            } else if key_is(&key, km.git_stage) {
                // Staging from here is the whole point of reading the diff:
                // decide, act, move on without a round trip through the list.
                // The cursor is first pulled to the file under the scroll, so
                // the toggle hits what the eye is on, not what `d` was
                // pressed on screens ago.
                if let Some(i) = state.git_diff.as_ref().and_then(|d| d.current_file_idx()) {
                    sync_status_cursor_to(state, i);
                }
                toggle_stage_at_cursor(state, tx);
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
                    let hms = |dt: chrono::DateTime<chrono::Utc>| {
                        format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second())
                    };
                    match items.get(state.detail_cursor).copied() {
                        Some(DetailItem::Job(ji)) => {
                            if let Some(job) = detail.jobs.get(ji).cloned() {
                                // A failed job's logs open at the step that
                                // broke, not at line one of a checkout step
                                // nobody is here to read.
                                state.log_pending_section = job
                                    .steps
                                    .iter()
                                    .find(|s| s.status == Status::Failure)
                                    .map(|s| {
                                        (
                                            s.name.clone(),
                                            s.started_at.map(hms),
                                            s.completed_at.map(hms),
                                        )
                                    });
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
                                state.log_pending_section = Some((
                                    step.name.clone(),
                                    step.started_at.map(hms),
                                    step.completed_at.map(hms),
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
            let total_rendered = state.log_rendered.len();
            let viewport = state.last_logs_viewport_height.get().max(1) as usize;
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
                if state.expand_fold_at(cursor) {
                    // Keep the eye where it was: the revealed lines push
                    // everything below down, so hold the line that was under
                    // the cursor rather than the row number.
                    let anchor = state.log_rendered_src.get(cursor).copied();
                    state.recompute_log_rendered();
                    if let Some(row) = anchor.and_then(|s| state.rendered_row_for_src(s)) {
                        state.log_line_cursor = row;
                        state.keep_cursor_visible();
                        state.recompute_log_rendered();
                    }
                } else if let Some(&gi) = state.log_rendered_group_map.get(&cursor) {
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
                    set_log_section(state, extracted, Some(next));
                } else {
                    let n = state.log_sections.len();
                    if n > 0 {
                        let next = state.log_section_idx.map(|i| (i + 1).min(n - 1)).unwrap_or(0);
                        let extracted = extract_log_section(&state.log_raw, next);
                        set_log_section(state, extracted, Some(next));
                    }
                }
            } else if key_is(&key, km.prev_step) {
                if state.log_search_query.is_some() {
                    jump_log_match(state, -1);
                } else if !state.log_step_line_starts.is_empty() {
                    match state.log_section_idx {
                        None => {}
                        Some(0) => {
                            let all = state.log_raw.clone();
                            set_log_section(state, all, None);
                        }
                        Some(current) => {
                            let prev = current - 1;
                            let extracted = extract_step_by_line_range(&state.log_raw, prev, &state.log_step_line_starts);
                            set_log_section(state, extracted, Some(prev));
                        }
                    }
                } else {
                    match state.log_section_idx {
                        None => {}
                        Some(0) => {
                            let all = state.log_raw.clone();
                            set_log_section(state, all, None);
                        }
                        Some(i) => {
                            let prev = i - 1;
                            let extracted = extract_log_section(&state.log_raw, prev);
                            set_log_section(state, extracted, Some(prev));
                        }
                    }
                }
            } else if key_is(&key, km.all_steps) {
                let all = state.log_raw.clone();
                set_log_section(state, all, None);
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

/// Keys while the help card is up. Extracted so the card's own little modes —
/// scrolling, and typing into its filter — can be exercised without standing a
/// provider and an event loop up around them.
fn handle_help_key(state: &mut AppState, key: KeyEvent, km: &Keymap) {
    // Clamped to what the card last drew, not merely saturated: notches
    // counted past the bottom would all have to be scrolled back through
    // before anything moved, which reads as a stuck wheel.
    let max = state.last_help_max_scroll.get();
    let down = |s: u16, by: u16| s.saturating_add(by).min(max);
    // Typing into the filter. Every printable key belongs to the query —
    // `q` and `?` included — so only the editing keys are read as keys.
    // The card re-filters on each keystroke rather than on `↵`: the whole
    // point is watching the reference narrow to the row you are after.
    if state.help_typing {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                state.help_search.clear();
                state.help_typing = false;
                state.help_scroll = 0;
            }
            // Hands the keyboard back to the card with the filter still
            // on: what you searched for stays, and now scrolls.
            KeyCode::Enter => state.help_typing = false,
            KeyCode::Backspace => {
                // Backspacing past the start leaves the field rather than
                // sitting in an empty one that swallows the next `?`.
                if state.help_search.pop().is_none() {
                    state.help_typing = false;
                }
                state.help_scroll = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                state.help_search.push(c);
                state.help_scroll = 0;
            }
            KeyCode::Down => state.help_scroll = down(state.help_scroll, 1),
            KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
            KeyCode::PageDown => state.help_scroll = down(state.help_scroll, 10),
            KeyCode::PageUp => state.help_scroll = state.help_scroll.saturating_sub(10),
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Down => state.help_scroll = down(state.help_scroll, 1),
        KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
        KeyCode::PageDown => state.help_scroll = down(state.help_scroll, 10),
        KeyCode::PageUp => state.help_scroll = state.help_scroll.saturating_sub(10),
        _ if key_is(&key, km.down) => state.help_scroll = down(state.help_scroll, 1),
        _ if key_is(&key, km.up) => state.help_scroll = state.help_scroll.saturating_sub(1),
        // The same key that searches a log searches the reference.
        _ if key_is(&key, km.search) => {
            state.help_typing = true;
            state.help_scroll = 0;
        }
        // Esc drops a filter before it closes the card: one key, undoing
        // the last thing it did.
        KeyCode::Esc if !state.help_search.is_empty() => {
            state.help_search.clear();
            state.help_scroll = 0;
        }
        // Anything else — `?`, Esc, q, Enter — dismisses it.
        _ => close_help(state),
    }
}

/// Put the help card away, filter and all: a card reopened still holding the
/// last question you asked it is a card that answers the wrong one.
fn close_help(state: &mut AppState) {
    state.show_help = false;
    state.help_opened_tick = None;
    state.help_search.clear();
    state.help_typing = false;
    state.needs_clear = true;
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
        .get(state.log_line_cursor)
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

/// Swap the visible log for another step's lines and rebuild every index that
/// hangs off them. Every step-navigation key funnels through here so they all
/// land the same way.
fn set_log_section(state: &mut AppState, lines: Vec<String>, section: Option<usize>) {
    state.log_section_idx = section;
    state.log_lines = lines;
    state.log_scroll = 0;
    state.clear_log_search();
    state.init_log_groups();
    state.recompute_log_rendered();
    land_on_first_error(state);
}

/// Open a freshly loaded log at its first error rather than at line 1.
///
/// The error is the reason the log was opened, and on a several-thousand-line
/// step it is nowhere near the top — landing there and making the user hunt for
/// it is the slow path. A log with no errors is left at the top, where reading
/// from the beginning is the point.
fn land_on_first_error(state: &mut AppState) {
    let Some(&first) = state.log_error_lines.first() else {
        return;
    };
    if let Some(gi) = state.group_containing(first) {
        state.log_collapsed.remove(&gi);
        state.recompute_log_rendered();
    }
    if let Some(row) = state.rendered_row_for_src(first) {
        state.log_line_cursor = row;
        state.center_cursor();
        state.recompute_log_rendered();
    }
    let n = state.log_error_lines.len();
    state.set_status(format!(
        "{n} error{} — opened at the first ({}/{} of the log) · e/E cycle · F fold the rest · g top",
        if n == 1 { "" } else { "s" },
        first + 1,
        state.log_lines.len(),
    ));
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
        .get(state.log_line_cursor)
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
        // The batch's list is short, fixed, and already all on screen.
        View::GitDiff | View::Logs | View::Diff | View::TriggerPrompt | View::BatchCommit => None,
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

/// Scroll keys for the hook-output pane. Returns whether the key was consumed.
///
/// Deliberately the same set as the log viewer, `e`/`E` included: a failed
/// `pre-commit` is a test log, so the keys that walk a CI log's errors should
/// walk this one's too.
fn handle_op_pane_key(state: &mut AppState, key: KeyEvent, km: &Keymap, spec: &str) -> bool {
    let viewport = state.last_op_viewport_height.get().max(1) as usize;
    let page = viewport as isize;
    let mut no_errors = false;
    let consumed = {
        let Some(op) = state.git_ops.get_mut(spec) else {
            return false;
        };
        if key_is(&key, km.down) || key.code == KeyCode::Down {
            op.scroll_by(1, viewport);
            true
        } else if key_is(&key, km.up) || key.code == KeyCode::Up {
            op.scroll_by(-1, viewport);
            true
        } else if key.code == KeyCode::PageDown {
            op.scroll_by(page, viewport);
            true
        } else if key.code == KeyCode::PageUp {
            op.scroll_by(-page, viewport);
            true
        } else if key_is(&key, km.scroll_top) {
            op.scroll_to_top();
            true
        } else if key_is(&key, km.scroll_bottom) {
            op.scroll_to_bottom();
            true
        } else if key_is(&key, km.next_error) {
            no_errors = !op.jump_error(true, viewport);
            true
        } else if key_is(&key, km.prev_error) {
            no_errors = !op.jump_error(false, viewport);
            true
        } else {
            false
        }
    };
    if no_errors {
        state.set_status("no errors in the output".into());
    }
    consumed
}

/// Open the commit-message editor, refusing when the index is empty.
fn begin_commit(state: &mut AppState) {
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    // `op_running` as well as `busy`: leaving the view and coming back clears
    // the flag but not the commit it was guarding. Checked here rather than
    // only at submit, so the refusal comes before the message is typed.
    if gv.busy || state.op_running(&gv.spec) {
        state.set_status("a git command is already running".into());
        return;
    }
    let Some(gv) = state.git_view.as_mut() else {
        return;
    };
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
            spawn_git_op_streaming(state, tx, "commit", "pre-commit", move |dir, out| {
                crate::git::commit(dir, &msg, out)?;
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

/// What a batch worker returns when its repo turned out to have nothing to do,
/// followed by the reason.
///
/// Carried through `GitOpDone`'s `Ok` because "nothing to commit" is a success:
/// a clean repo is not a failure, and pausing the batch on one would make an
/// already-committed repo look broken. Checked in the worker rather than
/// against the dashboard's cached status, which can be a poll out of date.
const BATCH_NOTHING: &str = "\u{0}nothing: ";

/// Whether a batch still has decisions or work left in it.
///
/// A live batch owns the way back: `View::BatchCommit` is only ever entered by
/// starting one, so anything that navigates away without this check strands the
/// run — still committing, or still paused waiting for retry/skip, with no key
/// that reaches it.
fn batch_is_live(state: &AppState) -> bool {
    state.batch.as_ref().is_some_and(|b| {
        b.is_working() || matches!(b.phase, BatchPhase::Paused | BatchPhase::AskPush)
    })
}

/// Whether the hook output on screen belongs to a live batch rather than to the
/// working-tree view that happens to be showing it.
///
/// Dismissing it from a repo opened *out of* a paused batch would gut the pause:
/// the batch view is showing that same output, and it is the entire account of
/// why the run stopped. So `back` there just goes back — the output stays, and
/// the batch is still the thing that owns it.
fn batch_owns_current_op(state: &AppState) -> bool {
    let Some(spec) = state.git_view.as_ref().map(|g| g.spec.as_str()) else {
        return false;
    };
    batch_is_live(state)
        && state
            .batch
            .as_ref()
            .and_then(|b| b.current())
            .is_some_and(|i| i.spec == spec)
}

/// Mark the row under the cursor for a batch commit.
///
/// Marking on its own does nothing at all — no staging, no commit. That is the
/// point: the dangerous key is the one that starts the batch, and this one can
/// be pressed freely while deciding.
fn toggle_repo_mark(state: &mut AppState) {
    let Some(card) = state.repos.get(state.repo_cursor) else {
        return;
    };
    // Same rule as opening the working tree: no checkout, nothing to commit.
    let (spec, local) = (card.spec.clone(), card.path.is_some());
    if !local {
        state.set_status(format!(
            "{spec} has no local checkout — only repos found on disk can be committed"
        ));
        return;
    }
    if !state.repo_marks.remove(&spec) {
        state.repo_marks.insert(spec);
    }
}

/// Open the message box for a batch commit over every marked repo.
fn start_batch_commit(state: &mut AppState) {
    let marked: Vec<(String, std::path::PathBuf)> = state
        .repos
        .iter()
        .filter(|c| state.repo_marks.contains(&c.spec))
        .filter_map(|c| c.path.clone().map(|p| (c.spec.clone(), p)))
        .collect();
    if marked.is_empty() {
        let key = views::display_key(&state.keymap.repo_mark).to_string();
        state.set_status(format!(
            "nothing marked — {key} marks the repo under the cursor, then C commits them all"
        ));
        return;
    }
    // Staging under a hook that is already reading the index would change the
    // commit out from under it.
    if let Some(busy) = marked.iter().find(|(spec, _)| state.op_running(spec)) {
        state.set_status(format!("{} is already mid-commit — wait for it", busy.0));
        return;
    }
    let items = marked
        .into_iter()
        .map(|(spec, path)| crate::app::state::BatchItem::new(spec, path))
        .collect();
    state.batch = Some(BatchCommit::new(items, state.tick_count));
    state.switch_view(View::BatchCommit);
}

/// Type the one message every repo in the batch will get.
fn handle_batch_input(state: &mut AppState, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(batch) = state.batch.as_mut() else {
        return;
    };
    let Some(buf) = batch.input.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => {
            // Nothing has run yet, so abandoning costs nothing — and the marks
            // survive, since re-marking four repos to fix a typo is a punishment.
            state.batch = None;
            state.switch_view(View::Repos);
        }
        KeyCode::Enter => {
            // Read rather than take: refusing an all-whitespace message must not
            // also wipe the draft, or Enter becomes a way to lose what you typed.
            let msg = buf.trim().to_string();
            if msg.is_empty() {
                state.set_status("type a message first — ↵ starts, Esc cancels".into());
                return;
            }
            batch.input = None;
            batch.message = msg;
            batch.phase = BatchPhase::Committing;
            batch_step(state, tx);
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

/// How long a finished batch's summary stays on screen before it takes itself
/// away — long enough to read one line of tally, short enough that it does not
/// feel like it is waiting for you.
const BATCH_DONE_DWELL_TICKS: u64 = 25;

/// Put the dashboard back after a batch that worked.
///
/// The card at the end of a clean run is a receipt: three green rows saying
/// "pushed" have nothing left to ask, and every next thing happens on the
/// dashboard behind them. So it leaves on its own, taking the marks with it the
/// way the back key does — and moving its tally to the status line, so the
/// number outlives the card.
///
/// Anything that went wrong stays put. A failure, or repos an abort never got
/// to, has no other record on screen, and a summary that scrolls itself away
/// before it is read is worse than no summary.
fn batch_auto_return(state: &mut AppState) {
    // Only from the batch's own view: if the user has walked somewhere else,
    // yanking them to the dashboard is a worse interruption than a stale card.
    if state.view != View::BatchCommit {
        return;
    }
    let Some(batch) = state.batch.as_ref() else {
        return;
    };
    if !batch.returns_on_its_own() {
        return;
    }
    let done = batch.done_tick.unwrap_or(state.tick_count);
    if state.tick_count.saturating_sub(done) < BATCH_DONE_DWELL_TICKS {
        return;
    }
    let t = batch.tally();
    let mut parts = vec![format!("{} committed", t.committed)];
    if t.pushed > 0 {
        parts.push(format!("{} pushed", t.pushed));
    }
    if t.nothing > 0 {
        parts.push(format!("{} had nothing to do", t.nothing));
    }
    state.batch = None;
    state.repo_marks.clear();
    state.switch_view(View::Repos);
    state.set_status_ok(parts.join(" · "));
}

/// Start the next repo in the batch, or wind the batch up.
///
/// Every move the batch makes goes through here, so "one repo at a time" is a
/// property of the code rather than something each call site has to remember.
fn batch_step(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let tick = state.tick_count;
    let Some(batch) = state.batch.as_mut() else {
        return;
    };
    if !batch.is_working() {
        return;
    }
    let phase_before = batch.phase;
    let Some(idx) = batch.advance(tick) else {
        // `advance` has already moved the phase on. If committing just ended
        // at the push question, ask it the way every other commit asks it — a
        // dialog with yes selected — not only as a banner that reads like a
        // result. Only on the transition itself: an Esc that dismissed the
        // dialog must not have it spring back on the next batch_step.
        if phase_before == BatchPhase::Committing && batch.phase == BatchPhase::AskPush {
            let committed = batch.tally().committed;
            state.push_prompt = Some(PushPrompt::for_batch(committed));
        }
        return;
    };
    let item = &batch.items[idx];
    let (spec, path) = (item.spec.clone(), item.path.clone());
    let pushing = batch.phase == BatchPhase::Pushing;
    let msg = batch.message.clone();

    if pushing {
        spawn_streaming_op_for(state, tx, spec, path, "push", "pre-push", move |dir, out| {
            // Read the branch here rather than trusting the dashboard's copy:
            // the commit that just landed changed the ahead count, and pushing
            // the wrong branch is not a recoverable mistake.
            let st = crate::git::status(dir)?;
            if st.detached {
                return Ok(format!("{BATCH_NOTHING}detached HEAD"));
            }
            if st.has_upstream && st.ahead == 0 {
                return Ok(format!("{BATCH_NOTHING}already up to date"));
            }
            crate::git::push(dir, &st.branch, st.has_upstream, out)
        });
    } else {
        spawn_streaming_op_for(
            state,
            tx,
            spec,
            path,
            "commit",
            "pre-commit",
            move |dir, out| {
                crate::git::stage_all(dir)?;
                // Asked after staging, so it answers the question that matters:
                // is there anything to commit *now*.
                if crate::git::status(dir)?.staged_count() == 0 {
                    return Ok(format!("{BATCH_NOTHING}working tree is clean"));
                }
                crate::git::commit(dir, &msg, out)?;
                Ok(crate::git::head_sha(dir).unwrap_or_else(|_| "HEAD".into()))
            },
        );
    }
}

/// Fold a finished git command into the batch, if it was the batch that asked.
fn batch_on_op_done(
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
    spec: &str,
    result: &Result<String, String>,
) {
    let Some(batch) = state.batch.as_mut() else {
        return;
    };
    // A commit the user started by hand in another repo is not ours to record.
    if batch.current().is_none_or(|i| i.spec != spec) {
        return;
    }
    match result {
        Ok(summary) => match summary.strip_prefix(BATCH_NOTHING) {
            Some(why) => batch.record_nothing(spec, why.to_string()),
            None => batch.record(spec, Ok(summary.clone())),
        },
        // Only the first line: the rest of the failure is in the op pane right
        // below, and a wall of pytest output does not fit on a list row.
        Err(e) => batch.record(spec, Err(e.lines().next().unwrap_or(e).to_string())),
    }
    // That repo's working tree just changed, and its dashboard row is showing
    // the old file count until the next poll — which could be seconds away.
    let path = batch.current().map(|i| i.path.clone());
    if let Some(path) = path {
        spawn_git_status(spec.to_string(), path, tx.clone(), state);
    }
    batch_step(state, tx);
}

/// Push every repo the batch committed, one at a time.
fn batch_start_push(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(batch) = state.batch.as_mut() else {
        return;
    };
    if batch.phase != BatchPhase::AskPush {
        return;
    }
    batch.phase = BatchPhase::Pushing;
    batch_step(state, tx);
}

/// Open the working tree of the repo the batch is paused on, to fix it.
///
/// The batch stays paused and stays in `state` — coming back with `H` lands on
/// the same pause, with retry and skip still on offer.
fn open_git_view_for_paused(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(item) = state.batch.as_ref().and_then(|b| b.current()) else {
        return;
    };
    let (spec, path) = (item.spec.clone(), item.path.clone());
    let has_ci = state
        .repos
        .iter()
        .find(|c| c.spec == spec)
        .is_some_and(|c| c.has_ci());
    state.git_view = Some(crate::app::state::GitView::new(spec.clone(), path.clone(), has_ci));
    state.switch_view(View::GitStatus);
    spawn_git_status(spec, path, tx.clone(), state);
}

/// Push the current branch, setting upstream on first push.
///
/// This is the step that makes a commit visible to CI: `workflow_dispatch` runs
/// against the remote, so triggering before pushing would run the *old* code.
/// Ask, the moment a commit lands, whether to push it.
///
/// Committing and pushing are one intention split across two keys, and the
/// second one is the one that gets forgotten — a repo sitting one commit ahead
/// is invisible to CI and to everyone else. So the question comes to you, with
/// yes already selected.
fn offer_push(state: &mut AppState, spec: &str) {
    // Only for the repo on screen. A batch walks repos you are not looking at
    // and asks its own push question once, at the end.
    if state.batch.is_some() {
        return;
    }
    let Some(gv) = state.git_view.as_ref().filter(|g| g.spec == spec) else {
        return;
    };
    let Some(status) = gv.status.as_ref() else {
        return;
    };
    // `push --set-upstream origin HEAD` while detached would create a remote
    // branch literally named `HEAD`, so there is no push here to offer.
    if status.detached {
        return;
    }
    // Somewhere to push to: either git already knows one, or the row was
    // scanned from a GitHub remote. Without either, yes would only produce an
    // error, and a dialog whose default answer fails is worse than no dialog.
    let known_remote = state
        .repos
        .iter()
        .any(|c| c.spec == spec && c.remote.is_some());
    if !status.has_upstream && !known_remote {
        return;
    }
    state.push_prompt = Some(PushPrompt {
        spec: spec.to_string(),
        branch: status.branch.clone(),
        has_upstream: status.has_upstream,
        yes: true,
        batch_count: None,
    });
}

/// Enter takes the highlighted answer, ←/→ move between them, Esc means no.
fn handle_push_prompt(state: &mut AppState, key: KeyEvent, tx: &mpsc::UnboundedSender<AppEvent>) {
    let set_yes = |state: &mut AppState, yes: bool| {
        if let Some(p) = state.push_prompt.as_mut() {
            p.yes = yes;
        }
    };
    match key.code {
        // The answer named outright, without walking to it first.
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            set_yes(state, true);
            accept_push_prompt(state, tx);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => state.push_prompt = None,
        KeyCode::Enter => accept_push_prompt(state, tx),
        KeyCode::Left
        | KeyCode::Right
        | KeyCode::Tab
        | KeyCode::BackTab
        | KeyCode::Char('h')
        | KeyCode::Char('l') => {
            let yes = state.push_prompt.as_ref().is_some_and(|p| p.yes);
            set_yes(state, !yes);
        }
        // Everything else is swallowed: the box is a question, and a keystroke
        // meant for the screen behind it would answer something else instead.
        _ => {}
    }
}

/// Push what the prompt was raised about, or drop it if the answer was no.
fn accept_push_prompt(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(p) = state.push_prompt.take() else {
        return;
    };
    if !p.yes {
        return;
    }
    // A batch's yes pushes everything the batch committed; the batch tracks
    // its own repos, so the prompt's spec/branch have nothing to add.
    if p.batch_count.is_some() {
        batch_start_push(state, tx);
        return;
    }
    // The push runs through the open working-tree view, so a view that has
    // moved on means this answer is about a repo that is no longer in front of
    // it — drop it rather than push the wrong one.
    if state.git_view.as_ref().is_none_or(|g| g.spec != p.spec) {
        return;
    }
    // Not `push_current`: that re-reads a working tree whose refresh is still in
    // flight, and would find the ahead count from before the commit landed.
    let (branch, has_upstream) = (p.branch.clone(), p.has_upstream);
    state.set_status(format!("pushing {branch}…"));
    spawn_git_op_streaming(state, tx, "push", "pre-push", move |dir, out| {
        crate::git::push(dir, &branch, has_upstream, out)
    });
}

fn push_current(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    // Check this before reading `status`: a commit in flight has not been folded
    // into the ahead count yet, so we'd otherwise report "nothing to push".
    if gv.busy || state.op_running(&gv.spec) {
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
    spawn_git_op_streaming(state, tx, "push", "pre-push", move |dir, out| {
        crate::git::push(dir, &branch, has_upstream, out)
    });
}

/// The GitHub coordinates behind a repo card, if it has any.
fn github_spec_for(state: &AppState, spec: &str) -> Option<RepoSpec> {
    let remote = state
        .repos
        .iter()
        .find(|c| c.spec == spec)
        .and_then(|c| c.remote.clone());
    RepoSpec::parse(remote.as_deref().unwrap_or(spec)).ok()
}

/// Ask GitHub whether the branch has an open PR.
///
/// Deliberately outside the `pending` counter: this decorates the git view
/// rather than being something the user asked for and waits on, and the header
/// must not say "fetching" for decoration.
fn spawn_fetch_pr(
    state: &AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    spec: &str,
    branch: &str,
) {
    let Some(rs) = github_spec_for(state, spec) else {
        return;
    };
    let p = provider.for_repo(rs);
    let (spec, branch, tx) = (spec.to_string(), branch.to_string(), tx.clone());
    tokio::spawn(async move {
        // An error reads as "no PR": the line simply stays absent, which is
        // the truthful render of "could not find one".
        let pr = p.pr_for_branch(&branch).await.ok().flatten();
        let _ = tx.send(AppEvent::PrLoaded(spec, branch, pr));
    });
}

/// `o` in the working tree: the branch's PR if it has one, otherwise the page
/// that would create it — the two answers to "take this branch to GitHub".
fn open_pr_in_browser(state: &mut AppState) {
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    if let Some(pr) = gv.open_pr() {
        let (url, number) = (pr.url.clone(), pr.number);
        let _ = open::that(&url);
        state.set_status(format!("opened PR #{number}"));
        return;
    }
    let Some(rs) = github_spec_for(state, &gv.spec) else {
        state.set_status("no GitHub remote to open".into());
        return;
    };
    let Some(status) = gv.status.as_ref() else {
        return;
    };
    if status.detached {
        state.set_status("detached HEAD — no branch to open a PR for".into());
        return;
    }
    if !status.has_upstream {
        state.set_status("push the branch first — a PR needs it on the remote".into());
        return;
    }
    let url = format!(
        "https://github.com/{}/{}/pull/new/{}",
        rs.owner, rs.repo, status.branch
    );
    let _ = open::that(&url);
    state.set_status(format!("opened create-PR page for {}", status.branch));
}

/// Follow a push into CI — see [`PushWatch`]. Replaces any watch already on
/// the repo: the newest push is the one whose runs matter now.
fn start_push_watch(state: &mut AppState, spec: &str) {
    if github_spec_for(state, spec).is_none() {
        return; // no GitHub remote — nothing will run anything to watch
    }
    let branch = state
        .git_view
        .as_ref()
        .filter(|g| g.spec == spec)
        .and_then(|g| g.status.as_ref())
        .or_else(|| {
            state
                .repos
                .iter()
                .find(|c| c.spec == spec)
                .and_then(|c| c.git.as_ref())
        })
        .filter(|s| !s.detached)
        .map(|s| s.branch.clone());
    state.push_watches.retain(|w| w.spec != spec);
    state.push_watches.push(PushWatch {
        spec: spec.to_string(),
        branch,
        pushed_at: chrono::Utc::now(),
        started_tick: state.tick_count,
        run: None,
    });
}

/// How long an unattached watch keeps looking before concluding the push
/// triggered nothing. Three minutes of 100ms ticks: GitHub usually stamps a
/// run within seconds, so this is generous — but a watch that cannot end
/// would poll forever on every push to a workflow-less repo.
const PUSH_WATCH_GIVE_UP_TICKS: u64 = 1800;

/// A hard ceiling on any watch, attached or not.
///
/// The deadline above only ever covered watches that never found a run. One
/// that *had* found a run had no deadline at all, so a run which neither settles
/// nor disappears — queued indefinitely because no runner is free — was followed
/// for as long as jog stayed open, spending a request per poll on an answer that
/// was never going to change. Two hours of 100ms ticks: longer than any CI run
/// worth following, short enough to end.
const PUSH_WATCH_MAX_TICKS: u64 = 72_000;

/// One request per watched repo per poll: the repo's newest runs, to find the
/// pushed run or to see it settle.
fn poll_push_watches(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if state.push_watches.is_empty() {
        return;
    }
    let now = state.tick_count;
    let mut gave_up: Vec<(String, bool)> = Vec::new();
    state.push_watches.retain(|w| {
        let age = now.saturating_sub(w.started_tick);
        let attached = w.run.is_some();
        let expired =
            age >= PUSH_WATCH_MAX_TICKS || (!attached && age >= PUSH_WATCH_GIVE_UP_TICKS);
        if expired {
            gave_up.push((w.spec.clone(), attached));
        }
        !expired
    });
    for (spec, attached) in gave_up {
        state.set_status(if attached {
            format!("{spec}: stopped following its run — still going after two hours")
        } else {
            format!("{spec}: pushed, but no CI run appeared")
        });
    }
    let specs: Vec<String> = state.push_watches.iter().map(|w| w.spec.clone()).collect();
    for spec in specs {
        let Some(rs) = github_spec_for(state, &spec) else {
            continue;
        };
        let p = provider.for_repo(rs);
        let tx2 = tx.clone();
        state.pending += 1;
        tokio::spawn(async move {
            let evt = match p.list_repo_runs(10).await {
                Ok(runs) => AppEvent::PushWatchRuns(spec, runs),
                Err(e) => AppEvent::TaskError(format!("push watch: {e:#}")),
            };
            let _ = tx2.send(evt);
        });
    }
}

/// Fold one scan of a watched repo's runs into its watch: attach the run the
/// push spawned, or announce that run settling and end the watch.
fn settle_push_watch(state: &mut AppState, spec: &str, runs: &[Run], config: &Config) {
    let Some(idx) = state.push_watches.iter().position(|w| w.spec == spec) else {
        return;
    };
    let w = &state.push_watches[idx];
    let newly = w.run.is_none();
    let current: Option<Run> = match &w.run {
        Some(tracked) => runs.iter().find(|r| r.id == tracked.id).cloned(),
        None => {
            // GitHub can stamp the run a moment before the push call returns
            // on our side; a little slack keeps a fast CI from being missed,
            // and the branch filter keeps the slack honest.
            let cutoff = w.pushed_at - chrono::Duration::seconds(30);
            runs.iter()
                .filter(|r| r.created_at >= cutoff)
                .filter(|r| w.branch.as_deref().is_none_or(|b| r.head_branch == b))
                .max_by_key(|r| r.created_at)
                .cloned()
        }
    };
    let Some(run) = current else {
        // A watch that had already adopted a run and can no longer find it is
        // done: `list_repo_runs` returns a fixed window over *every* workflow,
        // so on a busy repo the run drops out of it within minutes and is never
        // coming back. Kept here, the watch would poll this repo once per
        // interval for the rest of the session. A watch that has not adopted
        // anything yet is still waiting legitimately — that is what the deadline
        // in `poll_push_watches` is for — so it is left alone.
        if !newly {
            state.push_watches.remove(idx);
        }
        return;
    };
    if run.status.is_terminal() {
        state.push_watches.remove(idx);
        // Also drop it from the "seen running" set, so the dashboard's announcer
        // stops carrying a run that has already settled.
        state.watch_seen_running.remove(&run.id);
        let _ = notify_run_finished(state, &run, spec, config);
        let msg = format!(
            "{spec}: {} {}",
            if run.status.is_failure() { "✗" } else { "✓" },
            run.display_title,
        );
        if run.status.is_failure() {
            state.set_status_err(msg);
        } else {
            state.set_status_ok(msg);
        }
    } else {
        if newly {
            state.set_status(format!(
                "{spec}: CI picked up the push — {}",
                run.display_title
            ));
        }
        state.push_watches[idx].run = Some(run);
    }
}

/// Flat index (as `build_detail_items` counts rows) of the first failed step,
/// or failing that the first failed job — where the cursor lands on a red run.
fn first_failed_item(detail: &RunDetail) -> Option<usize> {
    let items = build_detail_items(detail);
    items
        .iter()
        .position(|it| {
            matches!(it, DetailItem::Step { job, step }
                if detail.jobs[*job].steps[*step].status == Status::Failure)
        })
        .or_else(|| {
            items.iter().position(|it| {
                matches!(it, DetailItem::Job(ji) if detail.jobs[*ji].status.is_failure())
            })
        })
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

/// Route a left click: find what the last frame drew under the pointer and
/// treat it as "move there — and open, if the cursor is already there". The
/// second click goes through `handle_key` as Enter, so every view keeps its
/// own idea of what opening means.
///
/// Overlays get what any key would give them: help and the services card are
/// dismissed, and the modal inputs (finder, prompts, text editors) keep the
/// screen — a stray click must not commit or cancel anything on their behalf.
async fn handle_click(
    state: &mut AppState,
    x: u16,
    y: u16,
    provider: &mut Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    km: &Keymap,
) {
    if state.show_help {
        close_help(state);
        return;
    }
    if state.show_services {
        state.show_services = false;
        state.services_opened_tick = None;
        state.needs_clear = true;
        return;
    }
    if state.finder.is_some()
        || state.push_prompt.is_some()
        || state.log_search_input.is_some()
        || state
            .git_view
            .as_ref()
            .is_some_and(|g| g.commit_input.is_some())
        || state.batch.as_ref().is_some_and(|b| b.input.is_some())
        || state.trigger_prompt.as_ref().is_some_and(|p| p.editing)
    {
        return;
    }
    let hit = state
        .hits
        .borrow()
        .iter()
        .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
        .map(|(_, h)| *h);
    let Some(hit) = hit else { return };
    // The view guard looks redundant — hits come from the frame this view just
    // drew — but it keeps a click racing a view switch from moving a cursor it
    // can no longer see.
    let mut open = false;
    match hit {
        Hit::Repo(i) if state.view == View::Repos => {
            open = state.repo_cursor == i;
            state.repo_cursor = i;
        }
        Hit::Workflow(i) if state.view == View::Workflows => {
            open = state.workflow_cursor == i;
            state.workflow_cursor = i;
            maybe_fetch_workflow_preview(state, provider, tx);
        }
        Hit::Run(i) if state.view == View::Runs => {
            open = state.run_cursor == i;
            state.run_cursor = i;
            maybe_fetch_preview(state, provider, tx);
        }
        Hit::DetailItem(i) if state.view == View::RunDetail => {
            open = state.detail_cursor == i;
            state.detail_cursor = i;
        }
        Hit::GitEntry(i) if state.view == View::GitStatus => {
            if let Some(g) = state.git_view.as_mut() {
                open = g.cursor == i;
                g.cursor = i;
            }
        }
        _ => {}
    }
    if open {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let _ = handle_key(state, enter, provider, tx, km).await;
    }
}

struct Keymap {
    quit: (KeyCode, KeyModifiers),
    back: (KeyCode, KeyModifiers),
    help: (KeyCode, KeyModifiers),
    refresh: (KeyCode, KeyModifiers),
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
    services: (KeyCode, KeyModifiers),
    snooze: (KeyCode, KeyModifiers),
    repo_mark: (KeyCode, KeyModifiers),
    batch_commit: (KeyCode, KeyModifiers),
    batch_retry: (KeyCode, KeyModifiers),
    batch_skip: (KeyCode, KeyModifiers),
    git_view: (KeyCode, KeyModifiers),
    git_stage: (KeyCode, KeyModifiers),
    git_stage_all: (KeyCode, KeyModifiers),
    git_commit: (KeyCode, KeyModifiers),
    git_push: (KeyCode, KeyModifiers),
    git_diff: (KeyCode, KeyModifiers),
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

/// Esc and Backspace both mean "back" everywhere a text field is not being
/// edited, whatever `[keys] back` is configured to. Text editors (search
/// prompts, commit message, trigger inputs) are dispatched before the global
/// back handler, so a Backspace while typing still deletes a character.
fn is_back_fallback(event: &KeyEvent) -> bool {
    matches!(event.code, KeyCode::Esc | KeyCode::Backspace)
        && event.modifiers.difference(KeyModifiers::SHIFT).is_empty()
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
        refresh:       parse_key(&cfg.refresh)?,
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
        services:      parse_key(&cfg.services)?,
        snooze:        parse_key(&cfg.snooze)?,
        repo_mark:     parse_key(&cfg.repo_mark)?,
        batch_commit:  parse_key(&cfg.batch_commit)?,
        batch_retry:   parse_key(&cfg.batch_retry)?,
        batch_skip:    parse_key(&cfg.batch_skip)?,
        git_view:      parse_key(&cfg.git_view)?,
        git_stage:     parse_key(&cfg.git_stage)?,
        git_stage_all: parse_key(&cfg.git_stage_all)?,
        git_commit:    parse_key(&cfg.git_commit)?,
        git_push:      parse_key(&cfg.git_push)?,
        git_diff:      parse_key(&cfg.git_diff)?,
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

        // A skipped step ran nothing and printed nothing, so it must not consume
        // a group header — the next step that really ran emitted that header,
        // and letting the skipped one claim it leaves the real step with an
        // empty log. Resolved to the following step's start below.
        if step.status == Status::Skipped {
            starts.push(SKIPPED_START);
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

    resolve_skipped_starts(&mut starts, raw.len());
    honour_end_of_step_markers(raw, &mut starts);

    let any_matched = starts.iter().any(|&l| l > 0)
        || steps.first().and_then(|s| s.started_at).is_some();
    if any_matched { (starts, names) } else { (Vec::new(), Vec::new()) }
}

/// Placeholder for a skipped step, replaced by the following step's start so
/// the range comes out empty.
const SKIPPED_START: usize = usize::MAX;

/// Give every skipped step the start of the next step that actually ran, which
/// makes its slice empty without shifting anything around it.
fn resolve_skipped_starts(starts: &mut [usize], total_lines: usize) {
    let mut next = total_lines;
    for s in starts.iter_mut().rev() {
        if *s == SKIPPED_START {
            *s = next;
        } else {
            next = *s;
        }
    }
}

/// Push step boundaries past the runner's own "this step is over" line.
///
/// The API reports step times to the second, while a whole step's output can
/// land inside one second — a `cargo test` run finishing at 09:59:58.388 and the
/// next step starting at 09:59:58 are indistinguishable by timestamp. The
/// timestamp fallback then hands the tail of a step to whatever came next
/// (usually a *skipped* step, which owns no output at all), and the failure the
/// user came to read disappears from the step that failed.
///
/// `##[error]Process completed with exit code N.` is the runner stating where
/// the step actually ended, so no boundary may fall before it.
fn honour_end_of_step_markers(raw: &[String], starts: &mut [usize]) {
    for i in 0..starts.len() {
        let from = starts[i];
        // A step that already owns no lines has no output and therefore no
        // marker of its own; scanning from here would find the *next* step's
        // marker and drag every boundary past it.
        if starts.get(i + 1).is_some_and(|&next| next <= from) {
            continue;
        }
        // Stop at the next step's own `Run …` header: past that, any marker
        // belongs to a later step and is not ours to claim.
        let limit = starts[i + 1..]
            .iter()
            .copied()
            .find(|&s| s > from && is_step_header(raw, s))
            .unwrap_or(raw.len());
        let Some(end) = (from..limit.min(raw.len()))
            .find(|&j| strip_time_prefix(&raw[j]).contains("Process completed with exit code"))
        else {
            continue;
        };
        // Everything that was placed inside this step's output moves to just
        // after the marker. Steps that owned nothing to begin with stay empty,
        // they just collapse at the new boundary instead of the old one.
        for s in starts[i + 1..].iter_mut() {
            if *s <= end {
                *s = (end + 1).min(raw.len());
            }
        }
    }
}

fn is_step_header(raw: &[String], line: usize) -> bool {
    raw.get(line).is_some_and(|l| {
        let c = strip_ansi(strip_time_prefix(l));
        c.starts_with("##[group]Run ") || c.starts_with("##[section]Run ")
    })
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
/// Everything the screen in front of you is showing, asked for again.
///
/// One key with one meaning on every view: the dashboard re-reads every repo's
/// working tree and re-fetches its CI, a run list re-fetches its runs, a log
/// re-fetches its job. Nothing in here mutates anything — the point of the key
/// is to distrust what is on screen, not to act on it.
///
/// The poll's gates do not apply. A poll is the app guessing when you want to
/// know; this is you saying so, which is exactly the moment to jump Kuma's
/// slower clock and the preview caches. The one gate that survives is a
/// rate-limit hold: asking again there is what extends the wait, so the local
/// half still happens and the header keeps counting the refusal down.
fn refresh_current(
    state: &mut AppState,
    provider: &Arc<GitHubProvider>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let held = state.api_held();
    // Feedback first: most of these are quiet round trips that change nothing
    // on screen when the answer matches what is already there, and a key that
    // appears to do nothing reads as a key that is not bound. A hold replaces
    // this with its countdown below.
    if !matches!(state.view, View::BatchCommit | View::TriggerPrompt) {
        state.set_status("refreshing…".into());
    }
    match state.view {
        View::Repos => {
            // Every row's working tree, ungated, plus its CI unless held —
            // `spawn_fetch_repo_cards` splits those two halves itself.
            spawn_fetch_repo_cards(provider, state, tx);
            if !held {
                spawn_dash_tail(provider, state, tx);
            }
            // The Live column comes from Kuma, which runs on a clock of
            // minutes. Forced: waiting one out is the thing being asked past.
            spawn_kuma_probe(state, tx, true);
        }
        View::Workflows => {
            if held {
                return held_status(state);
            }
            spawn_repo_status_fetch(provider.clone(), tx.clone());
            if let Some(w) = state.selected_workflow().cloned() {
                spawn_fetch_workflow_preview(provider.clone(), w.file_name, tx.clone(), state);
            }
        }
        View::Runs => {
            if held {
                return held_status(state);
            }
            if let Some(file) = state.workflow_for_runs.clone() {
                spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
            }
            // The preview pane is keyed by run id and would otherwise consider
            // itself already correct for the run the cursor is on.
            if let Some(r) = state.selected_run().cloned() {
                spawn_fetch_run_preview(provider.clone(), r.id, tx.clone(), state);
            }
        }
        // The diff reads the run detail that is already loaded, so refreshing
        // it is refreshing that.
        View::RunDetail | View::Diff => {
            if held {
                return held_status(state);
            }
            let id = state
                .run_detail
                .as_ref()
                .map(|d| d.run.id)
                .or_else(|| state.selected_run().map(|r| r.id));
            if let Some(id) = id {
                spawn_fetch_run_detail(provider.clone(), id, tx.clone(), state);
            }
        }
        View::Logs => {
            if held {
                return held_status(state);
            }
            // A running job's log grows; the archive it is read from serves
            // whatever exists so far, so this is how you see the rest of it.
            let job = state
                .log_job_idx
                .and_then(|i| state.run_detail.as_ref()?.jobs.get(i))
                .map(|j| j.id);
            if let Some(id) = job {
                spawn_fetch_logs(provider.clone(), id, tx.clone(), state);
            }
        }
        View::Watch => {
            if held {
                return held_status(state);
            }
            if let Some(file) = state.workflow_for_runs.clone() {
                spawn_fetch_runs(provider.clone(), file, tx.clone(), state);
            }
            if let Some(run) = state.runs.first().cloned() {
                spawn_fetch_run_detail(provider.clone(), run.id, tx.clone(), state);
            }
            spawn_watch_tail(provider, state, tx);
        }
        View::GitStatus => {
            if let Some(g) = state.git_view.as_ref() {
                let (spec, path) = (g.spec.clone(), g.path.clone());
                spawn_git_status(spec, path, tx.clone(), state);
            }
        }
        View::GitDiff => open_git_diff(state, tx),
        // A batch mid-run and a trigger prompt mid-answer are showing you what
        // you typed, not something fetched. There is nothing to ask again.
        View::BatchCommit | View::TriggerPrompt => {}
    }
}

/// What a refresh says when GitHub has told us to wait — the same countdown
/// the header is already showing, in the place you just pressed a key.
fn held_status(state: &mut AppState) {
    if let Some(left) = state.api_hold_left() {
        state.set_status(format!("holding off GitHub for {left}s — rate limited"));
    }
}

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
            spawn_git_status_gated(label.clone(), path, tx.clone(), state);
        }
        // Held off after a refusal — the row keeps the error it already shows,
        // and the header says how long. Re-asking is what extends the wait.
        if state.api_held() {
            continue;
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
                // Not `{e:#}`: that chain reads outermost-first, so a cell wide
                // enough for three words spends them on "list all repo runs".
                Err(e) => AppEvent::RepoCardFailed(label, classify_error(&e)),
            };
            let _ = tx2.send(evt);
        });
    }
}

/// Notice the workspace going quiet with every row green, once per lull.
///
/// A transition, not a state: "busy a moment ago, and now nothing in flight
/// with nothing red" is the moment the sweep celebrates. A plain all-green
/// predicate would fire on every poll of a quiet morning, and a celebration
/// on a schedule is a screensaver.
fn note_all_green(state: &mut AppState) {
    let busy = state.repos.iter().any(|c| c.active_runs().next().is_some());
    if state.ci_was_busy && !busy {
        let verdicts: Vec<Status> = state
            .repos
            .iter()
            .filter(|c| c.has_ci() && c.error.is_none())
            .filter_map(|c| c.latest_status())
            .collect();
        if !verdicts.is_empty() && verdicts.iter().all(|s| *s == Status::Success) {
            state.all_green_tick = Some(state.tick_count);
        }
    }
    state.ci_was_busy = busy;
}

/// Take a new budget reading, and say whether it is worth making a noise about.
///
/// The alarm is once per hour's budget, not once per poll: a warning that
/// repeats every few seconds is one you turn off, and then it is not a warning.
/// It re-arms when the quota resets, because a dashboard left open all day would
/// otherwise get exactly one warning in the morning.
fn record_quota(state: &mut AppState, q: Quota) -> bool {
    let alarm = q.is_critical() && !state.quota_alarmed;
    state.quota_alarmed = q.is_critical();
    state.quota = Some(q);
    if alarm {
        state.set_status(format!(
            "{}% of the GitHub API budget spent — quit jog until {} to keep the rest",
            q.percent(),
            q.reset.with_timezone(&chrono::Local).format("%H:%M")
        ));
    }
    alarm
}

/// Re-read the Uptime Kuma status page, when one is configured.
///
/// Blocking HTTP off-thread, exactly the shape of a git subprocess; nothing
/// here touches the GitHub budget or its rate-limit holds. One read per poll
/// serves every view — production being down matters just as much from
/// inside a log as from the dashboard.
fn spawn_kuma_probe(
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
    force: bool,
) {
    let Some(k) = &state.kuma else { return };
    if state.kuma_pending {
        return;
    }
    // Kuma's own cadence, not the CI poll's: the monitors behind the page
    // check on the order of minutes, so five-second reads mostly re-download
    // an answer that hasn't moved. First read goes out immediately; `force`
    // is the card's refresh key jumping the queue.
    let every_ticks = k.poll_interval_s.max(5) * 10;
    if !force
        && state.kuma_last_poll_tick != 0
        && state.tick_count.saturating_sub(state.kuma_last_poll_tick) < every_ticks
    {
        return;
    }
    state.kuma_last_poll_tick = state.tick_count.max(1);
    state.kuma_pending = true;
    let (url, slug, tx) = (k.url.clone(), k.status_page.clone(), tx.clone());
    tokio::task::spawn_blocking(move || {
        let res = crate::kuma::fetch(&url, &slug).map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::KumaLoaded(res));
    });
}

/// Re-read the API budget: what the header meter shows, and what tells a
/// rate-limited dashboard how long it will stay that way.
///
/// Usually free of a round trip as well as of the budget — the reading comes
/// off the headers of answers the poll was fetching anyway. Still once per
/// poll and never once per row, for the polls that fall back to `/rate_limit`.
fn spawn_quota_probe(
    provider: &Arc<GitHubProvider>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if state.quota_pending {
        return;
    }
    state.quota_pending = true;
    let (p, tx) = (provider.clone(), tx.clone());
    tokio::spawn(async move {
        let _ = tx.send(AppEvent::QuotaLoaded(p.quota().await.ok()));
    });
}

/// How many concurrent runs one dashboard row is followed down to its steps.
///
/// Every tracked run costs a request per poll, and the strip has room for four
/// lines in total — a repo that fans a push out across a twenty-leg matrix must
/// not spend the API budget on rows nobody can see.
const MAX_TRACKED_RUNS: usize = 4;

/// Follow the in-flight runs on one dashboard row down to their jobs and steps.
///
/// The row itself can only say *that* CI is going. This is what lets the strip
/// at the bottom name the workflows and the steps they are on right now — all
/// of them: a push that starts CI and a deploy is two runs, and following only
/// one leaves the other invisible until it fails.
///
/// Deliberately outside the `pending` counter: it is decoration over the row,
/// and a repo whose detail fetch is slow or broken must not pause the poll that
/// keeps every other row current.
fn sync_repo_progress(
    provider: &Arc<GitHubProvider>,
    state: &mut AppState,
    spec: &str,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let target = state.repos.iter().find(|c| c.spec == spec).map(|c| {
        (c.active_runs().take(MAX_TRACKED_RUNS).cloned().collect::<Vec<_>>(), c.remote.clone())
    });
    let Some((runs, remote)) = target else { return };
    if runs.is_empty() {
        // Nothing in flight anymore — dropping the entry is what makes the
        // strip clear itself the moment the last run lands.
        state.run_progress.remove(spec);
        return;
    }

    // Show what the row already knows immediately; the steps land a poll later.
    // Without this a run would sit invisible until its first detail response.
    // Rebuilt rather than patched so a run that has settled since the last poll
    // drops out even while its siblings keep going.
    let previous = state.run_progress.remove(spec).unwrap_or_default();
    let details = runs
        .iter()
        .map(|run| match previous.iter().find(|d| d.run.id == run.id) {
            Some(d) => RunDetail { run: run.clone(), jobs: d.jobs.clone() },
            None => RunDetail { run: run.clone(), jobs: Vec::new() },
        })
        .collect();
    state.run_progress.insert(spec.to_string(), details);

    let Some(remote) = remote else { return };
    let Ok(rspec) = RepoSpec::parse(&remote) else {
        return;
    };
    for run in runs {
        let p = provider.for_repo(rspec.clone());
        let (label, tx) = (spec.to_string(), tx.clone());
        tokio::spawn(async move {
            // Jobs only: the run itself came back with the row's poll a moment
            // ago, and re-fetching it doubled the traffic of the busiest part
            // of the poll for an answer we already hold.
            //
            // A failure here is silent on purpose: the row still reports the
            // run, and a toast per poll per repo would bury every other message.
            if let Ok(jobs) = p.run_jobs(run.id).await {
                let detail = RunDetail { run, jobs };
                let _ = tx.send(AppEvent::RepoProgressLoaded(label, detail));
            }
        });
    }
}

/// Read a checkout's working-tree state off-thread.
///
/// `git status` is fast but not instant on a large repo, and the event loop must
/// not stall mid-frame — so every git call goes through `spawn_blocking`.
/// The dashboard poll's ticks between unconditional `git status` runs — the
/// backstop for what the fingerprint cannot see (edits to tracked files touch
/// nothing under `.git`). Thirty seconds: an external edit shows up in the
/// Changes column within that, while the six polls in between cost a few
/// stats instead of a subprocess per repo each.
const GIT_STATUS_BACKSTOP_TICKS: u64 = 300;

/// `spawn_git_status`, but only when the repo looks like it moved.
///
/// For the dashboard poll alone: everything jog *does* to a repo refreshes
/// the status explicitly through `spawn_git_status`, so this gate only ever
/// skips re-asking a question whose answer nothing has touched.
fn spawn_git_status_gated(
    spec: String,
    path: std::path::PathBuf,
    tx: mpsc::UnboundedSender<AppEvent>,
    state: &mut AppState,
) {
    let now = state.tick_count;
    // A handful of stats on the event loop, not a subprocess: cheaper than
    // shipping the check off-thread and having to marshal the skip back.
    let fp = crate::git::fingerprint(&path);
    if let Some((prev, last_full)) = state.git_poll_gate.get(&spec)
        && *prev == fp
        && now.saturating_sub(*last_full) < GIT_STATUS_BACKSTOP_TICKS
    {
        return;
    }
    state.git_poll_gate.insert(spec.clone(), (fp, now));
    spawn_git_status(spec, path, tx, state);
}

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

/// Show the combined diff of every changed file, opened at the file under the
/// working-tree cursor.
///
/// Re-entrant on purpose: called again after a stage/unstage to redraw what
/// actually changed.
fn open_git_diff(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>) {
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    let entries: Vec<crate::git::StatusEntry> = gv.entries().to_vec();
    if entries.is_empty() {
        state.set_status("nothing to diff — the working tree is clean".into());
        return;
    }
    let opened_on = gv
        .selected()
        .map(|e| e.path.clone())
        .unwrap_or_else(|| entries[0].path.clone());
    let (spec, path) = (gv.spec.clone(), gv.path.clone());
    // Keep the old text on screen while the new one loads: swapping to an empty
    // pane and back makes a reload after staging flicker.
    let tick = state.tick_count;
    match state.git_diff.as_mut() {
        Some(dv) if dv.spec == spec => {
            dv.file = opened_on;
            dv.loading = true;
            dv.opened_tick = Some(tick);
        }
        _ => {
            let mut dv = crate::app::state::GitDiffView::new(spec.clone(), opened_on);
            dv.opened_tick = Some(tick);
            state.git_diff = Some(dv);
        }
    }
    state.switch_view(View::GitDiff);

    let tx = tx.clone();
    state.pending += 1;
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::with_capacity(entries.len());
        for entry in &entries {
            match crate::git::file_diff(&path, entry) {
                Ok(sections) => files.push((entry.path.clone(), sections)),
                Err(e) => {
                    let _ = tx.send(AppEvent::TaskError(format!("git diff: {e:#}")));
                    return;
                }
            }
        }
        let _ = tx.send(AppEvent::GitDiffLoaded(spec, files));
    });
}

/// Jump to the next/previous file's banner inside the combined diff.
///
/// The working-tree cursor follows along, so staging from here still acts on
/// the file that is on screen.
fn step_diff_file(state: &mut AppState, delta: i32) {
    let Some(dv) = state.git_diff.as_mut() else {
        return;
    };
    if dv.files.is_empty() {
        return;
    }
    let cur = dv.current_file_idx().unwrap_or(0) as i32;
    let next = (cur + delta).clamp(0, dv.files.len() as i32 - 1) as usize;
    dv.file = dv.files[next].clone();
    dv.pending_jump.set(Some(next));
    sync_status_cursor_to(state, next);
}

/// Point the working-tree cursor at the `i`th file of the open diff.
fn sync_status_cursor_to(state: &mut AppState, i: usize) {
    let Some(file) = state
        .git_diff
        .as_ref()
        .and_then(|d| d.files.get(i))
        .cloned()
    else {
        return;
    };
    if let Some(gv) = state.git_view.as_mut()
        && let Some(pos) = gv.entries().iter().position(|e| e.path == file)
    {
        gv.cursor = pos;
    }
}

/// Run one git mutation off-thread; the result refreshes the status view.
///
/// For the quick ones — staging, unstaging — that produce nothing worth
/// reading. `spawn_git_op_streaming` is the one for commands that run hooks.
fn spawn_git_op<F>(state: &mut AppState, tx: &mpsc::UnboundedSender<AppEvent>, op: F)
where
    F: FnOnce(&std::path::Path) -> anyhow::Result<String> + Send + 'static,
{
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    let (spec, path) = (gv.spec.clone(), gv.path.clone());
    // Staging while a hook is mid-commit would change the index out from under
    // the commit that is reading it.
    if gv.busy || state.op_running(&spec) {
        state.set_status("a git command is already running".into());
        return;
    }
    if let Some(gv) = state.git_view.as_mut() {
        gv.busy = true;
    }
    let tx = tx.clone();
    state.pending += 1;
    tokio::task::spawn_blocking(move || {
        let result = op(&path).map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::GitOpDone(spec, result));
    });
}

/// Run a git command whose output the user needs to see — commit and push,
/// where the work is done by a `pre-commit`/`pre-push` hook that can take a
/// minute and fail with a wall of pytest output.
///
/// Opens the op pane immediately so the view stops looking frozen, then feeds
/// it lines as the hook produces them.
fn spawn_git_op_streaming<F>(
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
    verb: &'static str,
    hook: &'static str,
    op: F,
) where
    F: FnOnce(&std::path::Path, &mut dyn FnMut(String, bool)) -> anyhow::Result<String>
        + Send
        + 'static,
{
    let Some(gv) = state.git_view.as_ref() else {
        return;
    };
    let (spec, path) = (gv.spec.clone(), gv.path.clone());
    if gv.busy || state.op_running(&spec) {
        state.set_status("a git command is already running".into());
        return;
    }
    if let Some(gv) = state.git_view.as_mut() {
        gv.busy = true;
    }
    spawn_streaming_op_for(state, tx, spec, path, verb, hook, op);
}

/// The same, for a repo that isn't the one the working-tree view is open on.
///
/// Split out for the batch commit, which walks repos the user is not looking
/// at. `git_ops` was already keyed by spec — this is the last thing that
/// assumed the op belonged to `git_view`.
fn spawn_streaming_op_for<F>(
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
    spec: String,
    path: std::path::PathBuf,
    verb: &'static str,
    hook: &'static str,
    op: F,
) where
    F: FnOnce(&std::path::Path, &mut dyn FnMut(String, bool)) -> anyhow::Result<String>
        + Send
        + 'static,
{
    let tick = state.tick_count;
    // Starts unnamed: whether a hook is installed takes two `git` calls to
    // answer, which the worker does rather than the UI thread.
    state
        .git_ops
        .insert(spec.clone(), GitOp::new(verb, None, tick));
    let tx = tx.clone();
    state.pending += 1;
    tokio::task::spawn_blocking(move || {
        let installed = crate::git::hook_path(&path, hook).map(|_| hook.to_string());
        let _ = tx.send(AppEvent::GitOpStarted(spec.clone(), installed));
        let mut batch = LineBatch::new(spec.clone(), tx.clone());
        let result =
            op(&path, &mut |line, partial| batch.push(line, partial)).map_err(|e| format!("{e:#}"));
        batch.flush();
        let _ = tx.send(AppEvent::GitOpDone(spec, result));
    });
}

/// Coalesces a command's output lines into batched events.
///
/// Every `AppEvent` costs a full redraw, and a hook running a test suite emits
/// thousands of lines in a burst. Flushing on a 100ms floor keeps the pane
/// visibly live — a line that arrives alone goes out immediately — while a
/// burst repaints ten times a second instead of ten thousand.
struct LineBatch {
    spec: String,
    tx: mpsc::UnboundedSender<AppEvent>,
    buf: Vec<(String, bool)>,
    /// `None` until the first flush, so the very first line goes out at once.
    last_flush: Option<std::time::Instant>,
}

impl LineBatch {
    fn new(spec: String, tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self {
            spec,
            tx,
            buf: Vec::new(),
            last_flush: None,
        }
    }

    fn push(&mut self, line: String, partial: bool) {
        // Consecutive partials are drafts of the same line; only the newest
        // matters, so a growing progress line doesn't inflate the batch.
        if let Some(last) = self.buf.last_mut()
            && last.1
        {
            *last = (line, partial);
        } else {
            self.buf.push((line, partial));
        }
        let due = self
            .last_flush
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
        if self.buf.len() >= 64 || due {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut self.buf);
        let _ = self.tx.send(AppEvent::GitOpOutput(self.spec.clone(), lines));
        self.last_flush = Some(std::time::Instant::now());
    }
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

/// How many lines of a running job's log the Watch view keeps. The pane shows
/// a dozen; the rest is slack for a burst between polls, not an archive — the
/// Logs view is the archive.
const WATCH_TAIL_KEEP: usize = 400;

/// How many empty answers a job's log gets before jog stops asking for it.
///
/// GitHub's REST log endpoint serves the *archive*, which does not exist
/// until the job finishes — a running job answers `BlobNotFound` every time.
/// A couple of tries covers the case where the archive is mid-assembly; past
/// that the pane shows the step feed instead and the poll keeps its budget.
const WATCH_TAIL_MAX_MISSES: u8 = 3;

/// Follow the running job's log while Watch is open: one fetch per poll, for
/// the tail pane under the step list.
///
/// GitHub serves a running job's log-so-far from the same endpoint as the
/// archive, just unreliably early on — a miss reports "nothing yet" rather
/// than erroring, and the next poll asks again. Deliberately outside the
/// `pending` counter, like the strip's detail fetches: it is decoration over
/// the view, and a slow log body must not pause the poll that keeps the steps
/// current.
fn spawn_watch_tail(
    provider: &Arc<GitHubProvider>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if state.watch_tail_pending {
        return;
    }
    let Some(detail) = &state.run_detail else {
        return;
    };
    if detail.run.status.is_terminal() {
        return;
    }
    let Some(job) = detail.jobs.iter().find(|j| j.status == Status::Running) else {
        return;
    };
    let (id, name) = (job.id, job.name.clone());
    if tail_given_up(state, id) {
        return;
    }
    state.watch_tail_pending = true;
    spawn_tail_fetch(provider.clone(), id, name, tx.clone());
}

/// Whether this job's log has already refused often enough to stop asking.
fn tail_given_up(state: &AppState, job_id: u64) -> bool {
    state
        .watch_tail_misses
        .get(&job_id)
        .is_some_and(|n| *n >= WATCH_TAIL_MAX_MISSES)
}

/// The fetch both panes share: the whole log body, trimmed to the tail jog
/// keeps. `None` covers every failure alike — "nothing there yet" and a hard
/// error read the same from a pane, which keeps what it already had.
fn spawn_tail_fetch(
    provider: Arc<GitHubProvider>,
    id: u64,
    name: String,
    tx: mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let lines = match provider.stream_logs(id).await {
            Ok(stream) => {
                let mut lines: Vec<String> = stream
                    .filter_map(|c| async { c.ok().map(|c| c.line) })
                    .collect()
                    .await;
                if lines.len() > WATCH_TAIL_KEEP {
                    lines.drain(..lines.len() - WATCH_TAIL_KEEP);
                }
                Some(lines)
            }
            Err(_) => None,
        };
        let _ = tx.send(AppEvent::WatchTailLoaded(id, name, lines));
    });
}

/// Widths below this cannot fit the dashboard's live pane, so nothing is
/// fetched for one either. 160 leaves the table at least 104 columns — enough
/// to keep every optional column it would have on a plain wide terminal.
pub(crate) const DASH_SPLIT_MIN_WIDTH: u16 = 160;

/// Live-tail fetch for the dashboard's side pane: the same endpoint, cap and
/// cadence as the Watch view's pane, aimed at whichever run
/// `dash_tail_target` picks. Skipped entirely on a terminal too narrow to
/// show the pane — the fetch would spend API budget on an invisible log.
fn spawn_dash_tail(
    provider: &Arc<GitHubProvider>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if state.watch_tail_pending || state.last_frame_width.get() < DASH_SPLIT_MIN_WIDTH {
        return;
    }
    let (label, id, name) = match state.dash_tail_target() {
        Some((spec, _, job)) => (spec.to_string(), job.id, job.name.clone()),
        None => return,
    };
    if tail_given_up(state, id) {
        return;
    }
    // A job id belongs to the repo that produced it, not to the checkout jog
    // was launched in. Every other dashboard fetch goes through `for_repo`;
    // this one asking the launch repo for another repo's job was a flat 404,
    // which the pane could only report as "still waiting".
    let Some(remote) = state
        .repos
        .iter()
        .find(|c| c.spec == label)
        .and_then(|c| c.remote.clone())
    else {
        return;
    };
    let Ok(rspec) = RepoSpec::parse(&remote) else {
        return;
    };
    state.watch_tail_pending = true;
    spawn_tail_fetch(Arc::new(provider.for_repo(rspec)), id, name, tx.clone());
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

/// How many lines of the failing step the failure digest keeps.
const DIGEST_LINES: usize = 12;

/// Fetch and boil down the failed job's log for the run open in the detail
/// view — the panel that answers "why is it red" before the log is opened.
///
/// Once per run: the digest is kept and the fetch marked in flight, so
/// re-entering the same red run costs nothing. Deliberately not counted in
/// `state.pending` — it is background reading, and pausing the polls for it
/// would trade live data for a convenience.
fn maybe_spawn_digest(
    provider: &Arc<GitHubProvider>,
    state: &mut AppState,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(detail) = &state.run_detail else {
        return;
    };
    if !detail.run.status.is_failure() {
        return;
    }
    let run_id = detail.run.id;
    if state
        .failure_digest
        .as_ref()
        .is_some_and(|d| d.run_id == run_id)
        || state.digest_pending == Some(run_id)
    {
        return;
    }
    let Some(job) = detail.jobs.iter().find(|j| j.status == Status::Failure) else {
        return;
    };
    let job_id = job.id;
    let job_name = job.name.clone();
    let steps = job.steps.clone();
    let step_name = steps
        .iter()
        .find(|s| s.status == Status::Failure)
        .map(|s| s.name.clone());
    state.digest_pending = Some(run_id);
    let (p, tx) = (provider.clone(), tx.clone());
    tokio::spawn(async move {
        let mut lines = Vec::new();
        if let Ok(mut s) = p.stream_logs(job_id).await {
            while let Some(chunk) = s.next().await {
                if let Ok(c) = chunk {
                    lines.push(c.line);
                }
            }
        }
        let (digest, errors) = extract_failure_digest(&lines, &steps, step_name.as_deref());
        let _ = tx.send(AppEvent::DigestLoaded {
            run_id,
            job: job_name,
            step: step_name,
            lines: digest,
            errors,
        });
    });
}

/// Boil a failed job's log down to the window worth reading: the failed step's
/// slice, opened a couple of lines above its first error — the error names the
/// failure, the lines over it usually name the file. A slice where nothing
/// classified as an error falls back to its tail, where test runners put their
/// summaries.
fn extract_failure_digest(
    raw: &[String],
    steps: &[Step],
    failed_step: Option<&str>,
) -> (Vec<String>, Vec<usize>) {
    if raw.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // The same step-slicing the log view uses, so the digest and the full log
    // agree about where the step starts.
    let slice: Vec<String> = failed_step
        .and_then(|name| {
            let (starts, names) = compute_step_line_starts(raw, steps);
            let needle = name.trim().to_lowercase();
            names
                .iter()
                .position(|s| s.trim().to_lowercase() == needle)
                .map(|idx| extract_step_by_line_range(raw, idx, &starts))
        })
        .unwrap_or_else(|| raw.to_vec());
    let (errors, _) = classify_log_severity(&slice);
    let start = match errors.first() {
        Some(&first) => first.saturating_sub(2),
        None => slice.len().saturating_sub(DIGEST_LINES),
    };
    let end = (start + DIGEST_LINES).min(slice.len());
    let window: Vec<String> = slice[start..end]
        .iter()
        .map(|l| strip_time_prefix(l).to_string())
        .collect();
    let error_rows = errors
        .iter()
        .filter(|&&e| e >= start && e < end)
        .map(|&e| e - start)
        .collect();
    (window, error_rows)
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
        let recall = state.history.last_dispatch_inputs(&workflow.file_name);
        state.trigger_prompt = Some(TriggerPrompt::from_workflow(workflow, return_view, recall));
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
    // What was typed is what will be wanted next time — the prompt opens
    // prefilled with it from here on.
    state.history.record_dispatch_inputs(&file, &inputs);
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
const QUOTA_SOUND: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/quota.wav"));
const ASK_SOUND: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/ask.wav"));

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

/// Sound for a commit or push that a hook rejected.
///
/// Reuses the CI fail sound and the same `notify` gate: a hook that fails after
/// you have walked away from a 90-second test suite is exactly the "something
/// broke" moment the sound exists for. Desktop notifications are skipped — the
/// commit was a keystroke ago, the user is still here.
fn play_op_failed_sound(config: &Config) {
    if config.ui.notify_mode() == NotifyMode::Never || !config.ui.notify_sound {
        return;
    }
    play_terminal_sound(Status::Failure, config);
}

/// The API budget is nearly gone.
///
/// Its own sound rather than the CI fail one: this is not a run that broke, it
/// is a warning that the next few minutes of watching will cost you the rest of
/// the hour — and the two call for opposite reactions. A failure asks you to
/// look; this asks you to leave.
fn play_quota_alarm(config: &Config) {
    if config.ui.notify_mode() == NotifyMode::Never || !config.ui.notify_sound {
        return;
    }
    if let Some(path) = sound_path(&config.ui.quota_sound, QUOTA_SOUND, "quota.wav") {
        play_sound(&path);
    }
}

/// A question has appeared and jog is holding still for an answer.
///
/// Called once per frame, so the chime hangs off the edge the dialog appears
/// on rather than off the frames it stays up for: a push question that goes
/// unanswered for a minute rings once, and walking Yes/No with ←/→ does not
/// ring at all. The batch commit this usually follows can run for a while, and
/// the whole point of asking is that you may not be watching when it lands.
///
/// Gated the same way every other sound is — `notify = "never"` and a snooze
/// mute it — plus its own switch, because a question is a different kind of
/// event from a run finishing.
fn announce_prompt(state: &mut AppState, config: &Config) {
    if state.push_prompt.is_none() {
        state.prompt_sounded = false;
        return;
    }
    if state.prompt_sounded {
        return;
    }
    state.prompt_sounded = true;
    if config.ui.notify_mode() == NotifyMode::Never
        || !config.ui.ask_sound_enabled
        || state.notify_snoozed()
    {
        return;
    }
    if let Some(path) = sound_path(&config.ui.ask_sound, ASK_SOUND, "ask.wav") {
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
    let _ = notify_run_finished(state, run, repo_label, config);
}

/// Sound + desktop notification for a finished run, gated on `ui.notify` — and
/// on the run not having been announced already.
///
/// Two watchers can see the same run settle on the same tick: the dashboard poll
/// and a push watch following your own push, both fetching the same repo. That
/// run is the one you care most about, and it was the one that got announced
/// twice — two sounds, two desktop notifications.
///
/// The ledger lives *inside* this function rather than in a wrapper around it,
/// so there is no second door: announcing a run without going through the dedupe
/// is not something a call site can do by accident. It takes `&mut AppState` for
/// that reason alone. Returns whether this call was the one that announced.
fn notify_run_finished(
    state: &mut AppState,
    run: &Run,
    repo_label: &str,
    config: &Config,
) -> bool {
    if !state.announced_runs.insert(run.id) {
        return false;
    }
    // Snoozed: the run still counts as announced — a snooze mutes the moment,
    // it must not queue the moment up for when the hour is over.
    if state.notify_snoozed() {
        return true;
    }
    notify_now(run, repo_label, config);
    true
}

/// Cycle the notification snooze: off → 30 minutes → 60 minutes → off.
///
/// One key, pressed until it says what you want — the alternative is a prompt,
/// and nobody entering a meeting wants a dialog about it.
fn toggle_snooze(state: &mut AppState) {
    let now = chrono::Utc::now();
    let key = views::display_key(&state.keymap.snooze).to_string();
    let left_min = state
        .snooze_until
        .filter(|t| *t > now)
        .map(|t| (t - now).num_minutes());
    match left_min {
        None => {
            state.snooze_until = Some(now + chrono::Duration::minutes(30));
            state.set_status(format!("notifications snoozed for 30m — {key} again for 60m"));
        }
        Some(m) if m < 31 => {
            state.snooze_until = Some(now + chrono::Duration::minutes(60));
            state.set_status(format!("notifications snoozed for 60m — {key} again to unmute"));
        }
        Some(_) => {
            state.snooze_until = None;
            state.set_status_ok("notifications back on".into());
        }
    }
}

/// Raise the notification, with no questions asked about whether it is a repeat.
fn notify_now(run: &Run, repo_label: &str, config: &Config) {
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
        desktop_notify(summary, body, run.url.clone(), run.status.is_failure());
    }
}

/// Raise an OS notification off-thread. Failures (no notification daemon, no
/// D-Bus session over SSH) are ignored — the sound and the TUI still work.
///
/// On freedesktop daemons, clicking the notification body fires the reserved
/// `default` action, so the run opens in the browser — but only if the app
/// registered that action and stays around to hear the reply. The thread
/// therefore parks in `wait_for_action` until the notification is clicked or
/// dismissed; it is detached, so nothing waits on it.
fn desktop_notify(summary: String, body: String, url: String, is_failure: bool) {
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
            if !url.is_empty() {
                n.action("default", "Open run");
            }
            if let Ok(handle) = n.show() {
                handle.wait_for_action(|action| {
                    if action == "default" {
                        let _ = open::that(&url);
                    }
                });
            }
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            let _ = (is_failure, url);
            let _ = n.show();
        }
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

/// Whether anything on screen is mid-animation — the difference between a
/// frame worth painting every 100ms and one worth painting once a second.
///
/// Deliberately over-inclusive: a wrongly-true answer costs one redundant
/// frame, a wrongly-false one freezes a spinner. Anything doubtful counts as
/// moving, and the once-a-second unconditional redraw in the tick arm bounds
/// whatever this misses at under a second of staleness.
fn animations_active(state: &AppState) -> bool {
    let tick = state.tick_count;
    let flashing = |at: Option<u64>, span: u64| at.is_some_and(|a| tick.saturating_sub(a) < span);
    let live = |r: &Run| !r.status.is_terminal();
    state.pending > 0
        // A down service keeps the header's heart beating, and the beat needs
        // every tick, not the idle one-per-second redraw.
        || state.services.iter().any(|s| s.state == crate::kuma::ServiceState::Down)
        // The footer's help beacon breathes for the first few seconds of a
        // session — frames it stops earning the moment it retires.
        || (!state.help_seen && tick < views::HELP_BEACON_TICKS)
        || state.git_ops.values().any(|o| !o.finished)
        || !state.run_progress.is_empty()
        || !state.push_watches.is_empty()
        || state.batch.is_some()
        || flashing(state.all_green_tick, views::CELEBRATE_TICKS)
        || state.repos.iter().any(|c| {
            (c.has_ci() && !c.loaded && c.error.is_none())
                || c.active_runs().next().is_some()
                || flashing(c.changed_tick, views::FLASH_TICKS)
                || flashing(c.settled_tick.map(|(t, _)| t), views::SETTLE_TICKS)
        })
        || state.runs.iter().any(live)
        || state.workflow_preview_runs.iter().any(live)
        || state
            .workflows
            .iter()
            .any(|w| matches!(w.last_status, Some(Status::Running | Status::Queued)))
        || state.run_detail.as_ref().is_some_and(|d| live(&d.run))
        || state.runs_preview.as_ref().is_some_and(|d| live(&d.run))
}

pub fn animated_glyph(s: Status, tick: u64) -> &'static str {
    if s == Status::Running {
        motion::Motion::new(tick).spinner()
    } else {
        status_glyph(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dashboard with two checkouts and one remote-only row.
    fn dashboard() -> AppState {
        let mut st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        for spec in ["acme/api", "acme/web"] {
            let mut card = RepoCard::new(spec.into());
            card.path = Some(std::path::PathBuf::from("/tmp").join(spec));
            card.git = Some(crate::git::parse_status("## main\0 M a.txt\0"));
            card.loaded = true;
            st.repos.push(card);
        }
        // No checkout: nothing on disk to stage.
        st.repos.push(RepoCard::new("acme/remote-only".into()));
        st.view = View::Repos;
        st
    }

    fn quota(used: u32) -> Quota {
        Quota { limit: 100, used, reset: chrono::Utc::now() + chrono::Duration::minutes(20) }
    }

    #[test]
    fn the_failure_digest_opens_a_little_above_the_first_error() {
        let mut lines: Vec<String> = (0..30).map(|i| format!("building thing {i}")).collect();
        lines[10] = "error[E0308]: mismatched types".into();
        let (window, errors) = extract_failure_digest(&lines, &[], None);
        assert_eq!(window.len(), DIGEST_LINES);
        // Two lines of lead-in above the error, and the error marked within.
        assert_eq!(window[0], "building thing 8");
        assert_eq!(errors, vec![2]);
        assert!(window[2].starts_with("error[E0308]"));
    }

    #[test]
    fn a_digest_with_nothing_classified_keeps_the_tail() {
        let lines: Vec<String> = (0..30).map(|i| format!("quiet line {i}")).collect();
        let (window, errors) = extract_failure_digest(&lines, &[], None);
        assert!(errors.is_empty());
        assert_eq!(window.last().map(String::as_str), Some("quiet line 29"));
        assert_eq!(window.len(), DIGEST_LINES);
    }

    #[test]
    fn snooze_cycles_thirty_sixty_off() {
        let mut st = dashboard();
        assert!(!st.notify_snoozed());
        toggle_snooze(&mut st);
        assert!(st.notify_snoozed(), "first press mutes");
        let first = st.snooze_until.unwrap();
        toggle_snooze(&mut st);
        assert!(st.snooze_until.unwrap() > first, "second press extends");
        toggle_snooze(&mut st);
        assert!(!st.notify_snoozed(), "third press unmutes");
    }

    #[test]
    fn a_snoozed_run_is_announced_silently_and_only_once() {
        let mut st = dashboard();
        toggle_snooze(&mut st);
        let run = run_with(7, Status::Failure);
        let cfg = Config::default();
        // Claims the announcement (so nothing replays it later) without a
        // sound or a notification — which the test can only assert indirectly:
        // the id is now in the ledger.
        assert!(notify_run_finished(&mut st, &run, "acme/api", &cfg));
        assert!(st.announced_runs.contains(&7));
        assert!(!notify_run_finished(&mut st, &run, "acme/api", &cfg));
    }

    #[test]
    fn the_all_green_moment_is_a_transition_not_a_state() {
        let mut st = dashboard();
        for (i, c) in st.repos.iter_mut().enumerate() {
            c.runs = vec![run_with(i as u64, Status::Success)];
            c.loaded = true;
        }
        // Quiet and green from the first look: nothing to celebrate — the
        // morning simply started well.
        note_all_green(&mut st);
        assert_eq!(st.all_green_tick, None);
        // A run takes off, then lands green: that is the moment.
        st.repos[0].runs[0].status = Status::Running;
        note_all_green(&mut st);
        st.tick_count = 50;
        st.repos[0].runs[0].status = Status::Success;
        note_all_green(&mut st);
        assert_eq!(st.all_green_tick, Some(50));
        // …and when one lands red instead, no sweep for a red board.
        st.repos[0].runs[0].status = Status::Running;
        note_all_green(&mut st);
        st.tick_count = 90;
        st.repos[0].runs[0].status = Status::Failure;
        note_all_green(&mut st);
        assert_eq!(st.all_green_tick, Some(50), "the old moment, not a new one");
    }

    #[test]
    fn an_idle_dashboard_earns_no_animation_frames() {
        let mut st = dashboard();
        for (i, c) in st.repos.iter_mut().enumerate() {
            c.runs = vec![run_with(i as u64, Status::Success)];
            c.loaded = true;
        }
        st.tick_count = 1000;
        assert!(!animations_active(&st), "everything settled, nothing moving");
        // One run in flight is one spinner spinning.
        st.repos[0].runs[0].status = Status::Running;
        assert!(animations_active(&st));
        st.repos[0].runs[0].status = Status::Success;
        // A flash mid-fade still needs its frames…
        st.repos[0].changed_tick = Some(995);
        assert!(animations_active(&st));
        // …and a faded one doesn't.
        st.repos[0].changed_tick = Some(500);
        assert!(!animations_active(&st));
        // A row still skeleton-loading shimmers.
        st.repos[1].loaded = false;
        assert!(animations_active(&st));
    }

    #[test]
    fn the_quota_alarm_sounds_once_per_budget_not_once_per_poll() {
        let mut st = dashboard();
        assert!(!record_quota(&mut st, quota(89)), "under the line is not news");
        assert!(record_quota(&mut st, quota(90)), "crossing it is");
        // Polls keep landing every few seconds. An alarm that fires on each one
        // is an alarm you silence, and then it is not an alarm.
        assert!(!record_quota(&mut st, quota(93)));
        assert!(!record_quota(&mut st, quota(100)));

        // The hour turns over and the budget refills: the next time it fills up
        // is a new fact, and a dashboard left open all day should hear about it.
        assert!(!record_quota(&mut st, quota(2)));
        assert!(record_quota(&mut st, quota(97)));
    }

    #[test]
    fn the_alarm_says_what_to_do_about_it() {
        let mut st = dashboard();
        record_quota(&mut st, quota(91));
        let msg = st.status_msg.clone().unwrap_or_default();
        // Not "rate limit warning": the number alone leaves you looking for the
        // action, and the action is to stop feeding the meter.
        assert!(msg.contains("91%"), "{msg}");
        assert!(msg.contains("quit jog"), "{msg}");
    }

    /// The working tree of `acme/api`, open, on a branch with an upstream.
    fn open_working_tree(header: &str) -> AppState {
        let mut st = dashboard();
        st.view = View::GitStatus;
        let mut gv = crate::app::state::GitView::new(
            "acme/api".into(),
            std::path::PathBuf::from("/tmp/acme/api"),
            true,
        );
        gv.status = Some(crate::git::parse_status(header));
        st.git_view = Some(gv);
        st
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_help_card_routes_every_printable_key_into_its_filter() {
        let km = resolve_keymap(&KeymapConfig::default()).unwrap();
        let mut st = dashboard();
        st.show_help = true;
        // `/` opens the field; after that `q` and `?` are text, not commands.
        handle_help_key(&mut st, press(KeyCode::Char('/')), &km);
        assert!(st.help_typing);
        for c in "q?st".chars() {
            handle_help_key(&mut st, press(KeyCode::Char(c)), &km);
        }
        assert_eq!(st.help_search, "q?st");
        assert!(st.show_help, "typing never closes the card");

        // Enter hands the keyboard back without dropping the filter.
        handle_help_key(&mut st, press(KeyCode::Enter), &km);
        assert!(!st.help_typing);
        assert_eq!(st.help_search, "q?st");

        // Esc drops the filter first, and only then closes the card.
        handle_help_key(&mut st, press(KeyCode::Esc), &km);
        assert!(st.show_help && st.help_search.is_empty());
        handle_help_key(&mut st, press(KeyCode::Esc), &km);
        assert!(!st.show_help);
    }

    #[test]
    fn backspacing_out_of_the_help_filter_leaves_the_field() {
        let km = resolve_keymap(&KeymapConfig::default()).unwrap();
        let mut st = dashboard();
        st.show_help = true;
        st.help_typing = true;
        handle_help_key(&mut st, press(KeyCode::Char('x')), &km);
        handle_help_key(&mut st, press(KeyCode::Backspace), &km);
        assert!(st.help_typing, "still editing an empty field");
        handle_help_key(&mut st, press(KeyCode::Backspace), &km);
        assert!(!st.help_typing, "backspacing past the start gives the keys back");
        assert!(st.show_help, "and does not take the card with it");
    }

    #[test]
    fn reopening_the_help_card_starts_from_the_whole_reference() {
        let mut st = dashboard();
        st.show_help = true;
        st.help_search = "push".into();
        st.help_typing = true;
        close_help(&mut st);
        assert!(st.help_search.is_empty() && !st.help_typing);
    }

    // tokio: saying yes spawns the first push op off-thread.
    #[tokio::test]
    async fn a_finished_batch_raises_the_same_push_dialog() {
        let mut st = dashboard();
        st.repo_marks.insert("acme/api".into());
        st.repo_marks.insert("acme/web".into());
        start_batch_commit(&mut st);
        let (tx, _rx) = mpsc::unbounded_channel();
        {
            let batch = st.batch.as_mut().unwrap();
            batch.input = None;
            batch.phase = BatchPhase::Committing;
            for item in &mut batch.items {
                item.state = crate::app::state::ItemState::Committed;
            }
        }
        // The last commit landing walks the queue and finds it empty — that
        // moment must raise the dialog, yes preselected, like any commit.
        batch_step(&mut st, &tx);
        let p = st.push_prompt.as_ref().expect("the batch asks with the dialog");
        assert_eq!(p.batch_count, Some(2));
        assert!(p.yes);

        // Esc declines the dialog without ending the batch: the banner (and
        // the push key) stay as the second chance…
        handle_push_prompt(&mut st, press(KeyCode::Esc), &tx);
        assert!(st.push_prompt.is_none());
        assert_eq!(st.batch.as_ref().unwrap().phase, BatchPhase::AskPush);
        // …and a stray batch_step must not resurrect the dismissed question.
        batch_step(&mut st, &tx);
        assert!(st.push_prompt.is_none());

        // Enter on the dialog is the whole point: it starts the pushes.
        st.push_prompt = Some(PushPrompt::for_batch(2));
        handle_push_prompt(&mut st, press(KeyCode::Enter), &tx);
        assert_eq!(st.batch.as_ref().unwrap().phase, BatchPhase::Pushing);
    }

    /// A batch of two, driven to the end of its push phase.
    fn pushed_batch() -> AppState {
        let mut st = dashboard();
        st.repo_marks.insert("acme/api".into());
        st.repo_marks.insert("acme/web".into());
        start_batch_commit(&mut st);
        let (tx, _rx) = mpsc::unbounded_channel();
        {
            let batch = st.batch.as_mut().unwrap();
            batch.input = None;
            batch.phase = BatchPhase::Pushing;
            for item in &mut batch.items {
                item.state = crate::app::state::ItemState::Pushed;
            }
        }
        // The queue is empty, so this is the step that winds the batch up.
        batch_step(&mut st, &tx);
        st
    }

    #[test]
    fn a_clean_batch_shows_its_receipt_and_then_goes_back_to_the_dashboard() {
        let mut st = pushed_batch();
        assert_eq!(st.batch.as_ref().unwrap().phase, BatchPhase::Done);
        let done = st.batch.as_ref().unwrap().done_tick.expect("finishing marks the tick");

        // The summary is there to be read: it does not blink out on the frame
        // it appeared on.
        st.tick_count = done + BATCH_DONE_DWELL_TICKS - 1;
        batch_auto_return(&mut st);
        assert!(st.batch.is_some(), "the receipt stays up long enough to read");
        assert_eq!(st.view, View::BatchCommit);

        st.tick_count = done + BATCH_DONE_DWELL_TICKS;
        batch_auto_return(&mut st);
        assert!(st.batch.is_none());
        assert_eq!(st.view, View::Repos);
        // The marks go with it, exactly as the back key would take them.
        assert!(st.repo_marks.is_empty());
        // …and the tally outlives the card it was drawn on.
        let msg = st.status_msg.clone().unwrap_or_default();
        assert!(msg.contains("2 committed"), "{msg}");
        assert!(msg.contains("2 pushed"), "{msg}");
    }

    #[test]
    fn a_batch_that_went_wrong_waits_to_be_dismissed() {
        // An abort leaves repos unattempted, and the card is the only place
        // that says which ones. It must not walk away from that.
        let mut st = dashboard();
        st.repo_marks.insert("acme/api".into());
        st.repo_marks.insert("acme/web".into());
        start_batch_commit(&mut st);
        {
            let batch = st.batch.as_mut().unwrap();
            batch.input = None;
            batch.phase = BatchPhase::Committing;
            batch.items[0].state = crate::app::state::ItemState::Failed("hook said no".into());
            batch.abort();
        }
        assert_eq!(st.batch.as_ref().unwrap().phase, BatchPhase::Done);
        assert!(!st.batch.as_ref().unwrap().returns_on_its_own());
        st.tick_count = 10_000;
        batch_auto_return(&mut st);
        assert!(st.batch.is_some(), "a failure is not a receipt");
        assert_eq!(st.view, View::BatchCommit);
    }

    #[test]
    fn a_finished_batch_left_behind_does_not_yank_the_view_back() {
        // Walked off to a repo's working tree while the batch wound up: the
        // dashboard can wait for the key rather than pulling you off the
        // screen you chose.
        let mut st = pushed_batch();
        st.switch_view(View::GitStatus);
        st.tick_count = 10_000;
        batch_auto_return(&mut st);
        assert_eq!(st.view, View::GitStatus);
        assert!(st.batch.is_some());
    }

    #[test]
    fn a_landed_commit_asks_about_pushing_with_yes_already_chosen() {
        let mut st = open_working_tree("## main...origin/main\0");
        offer_push(&mut st, "acme/api");
        let p = st.push_prompt.as_ref().expect("a commit raises the question");
        assert_eq!(p.branch, "main");
        // The whole point of the box: Enter alone finishes the job, without a
        // second key you have to remember on the way out of the commit.
        assert!(p.yes);
    }

    #[test]
    fn the_push_question_can_be_declined_and_stays_declined() {
        let (_g, tx) = ((), mpsc::unbounded_channel::<AppEvent>().0);
        let mut st = open_working_tree("## main...origin/main\0");

        offer_push(&mut st, "acme/api");
        handle_push_prompt(&mut st, press(KeyCode::Esc), &tx);
        assert!(st.push_prompt.is_none());
        assert!(!st.op_running("acme/api"), "Esc means not now, not later");

        // …and arrowing over to No and taking it is the same answer.
        offer_push(&mut st, "acme/api");
        handle_push_prompt(&mut st, press(KeyCode::Right), &tx);
        assert_eq!(st.push_prompt.as_ref().map(|p| p.yes), Some(false));
        handle_push_prompt(&mut st, press(KeyCode::Enter), &tx);
        assert!(st.push_prompt.is_none());
        assert!(!st.op_running("acme/api"));
    }

    #[test]
    fn nothing_to_push_to_means_nothing_to_ask() {
        // Detached: `push --set-upstream origin HEAD` would create a remote
        // branch called `HEAD`, so there is no sane yes to offer.
        let mut st = open_working_tree("## HEAD (no branch)\0");
        offer_push(&mut st, "acme/api");
        assert!(st.push_prompt.is_none());

        // A batch asks its own push question once, at the end — one box per
        // repo would be a queue of dialogs to click through.
        let mut st = open_working_tree("## main...origin/main\0");
        st.repo_marks.insert("acme/api".into());
        start_batch_commit(&mut st);
        offer_push(&mut st, "acme/api");
        assert!(st.push_prompt.is_none());
    }

    #[test]
    fn the_question_takes_the_keyboard_while_it_is_up() {
        let (_g, tx) = ((), mpsc::unbounded_channel::<AppEvent>().0);
        let mut st = open_working_tree("## main...origin/main\0");
        offer_push(&mut st, "acme/api");

        // `c` behind the box means "commit"; while the box is up it must mean
        // nothing at all, or answering one question starts another.
        handle_push_prompt(&mut st, press(KeyCode::Char('c')), &tx);
        assert!(st.push_prompt.is_some());
        assert!(
            st.git_view.as_ref().is_some_and(|g| g.commit_input.is_none()),
            "a key aimed at the screen behind the box must not reach it"
        );
    }

    #[test]
    fn only_a_repo_on_disk_can_be_marked_for_a_batch() {
        let mut st = dashboard();
        toggle_repo_mark(&mut st);
        assert!(st.repo_marks.contains("acme/api"));
        // Pressing it again takes it back out — marking is a toggle, not a
        // one-way door you have to restart the app to escape.
        toggle_repo_mark(&mut st);
        assert!(st.repo_marks.is_empty());

        st.repo_cursor = 2;
        toggle_repo_mark(&mut st);
        assert!(st.repo_marks.is_empty(), "a remote-only row has nothing to commit");
        assert!(
            st.status_msg.as_deref().is_some_and(|m| m.contains("no local checkout")),
            "got {:?}",
            st.status_msg
        );
    }

    // `open_git_view_for_paused` kicks off a `git status` read off-thread.
    #[tokio::test]
    async fn the_pause_keeps_its_hook_output_when_you_go_in_to_fix_the_repo() {
        let mut st = dashboard();
        st.repo_marks.insert("acme/api".into());
        start_batch_commit(&mut st);
        let batch = st.batch.as_mut().unwrap();
        batch.input = None;
        batch.phase = BatchPhase::Committing;
        batch.advance(0);
        batch.record("acme/api", Err("pre-commit hook failed".into()));

        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        op.push_line("assert 1 == 2".into(), false);
        op.finished = true;
        op.failed = true;
        st.git_ops.insert("acme/api".into(), op);

        // Opened out of the pause to fix the failure: `back` there must not
        // dismiss the output, because the batch view behind it is the thing
        // showing it — and it is the only account of why the run stopped.
        open_git_view_for_paused(&mut st, &mpsc::unbounded_channel().0);
        assert_eq!(st.view, View::GitStatus);
        assert!(batch_owns_current_op(&st));

        // A hand-started commit in a repo the batch is not on is still the
        // view's own to dismiss.
        st.git_view.as_mut().unwrap().spec = "acme/web".into();
        assert!(!batch_owns_current_op(&st));
    }

    #[test]
    fn a_batch_with_nothing_marked_says_how_to_mark() {
        let mut st = dashboard();
        start_batch_commit(&mut st);
        assert!(st.batch.is_none(), "nothing marked, nothing to do");
        assert_eq!(st.view, View::Repos);
        // Refusing without saying which key marks is how a feature stays unused.
        let msg = st.status_msg.clone().unwrap_or_default();
        assert!(msg.contains("nothing marked"), "got {msg:?}");
        assert!(msg.contains("␣"), "got {msg:?}");
    }

    #[test]
    fn a_batch_will_not_start_on_a_repo_that_is_already_mid_commit() {
        let mut st = dashboard();
        st.repo_marks.insert("acme/api".into());
        st.repo_marks.insert("acme/web".into());
        // A hand-started commit is still holding acme/web's index.
        st.git_ops
            .insert("acme/web".into(), GitOp::new("commit", None, 0));

        start_batch_commit(&mut st);
        assert!(st.batch.is_none(), "staging under a running hook corrupts the commit");
        assert!(
            st.status_msg.as_deref().is_some_and(|m| m.contains("acme/web")),
            "got {:?} — the refusal has to name the repo",
            st.status_msg
        );

        // Once it finishes, the batch starts and takes both repos in row order.
        st.git_ops.clear();
        start_batch_commit(&mut st);
        let batch = st.batch.as_ref().expect("batch started");
        let specs: Vec<&str> = batch.items.iter().map(|i| i.spec.as_str()).collect();
        assert_eq!(specs, vec!["acme/api", "acme/web"]);
        assert_eq!(st.view, View::BatchCommit);
        // Nothing has run yet: the message comes first.
        assert_eq!(batch.phase, BatchPhase::Compose);
        assert!(batch.input.is_some());
    }

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

    /// What the dashboard's Changes column reports: staged and unstaged are
    /// counted apart, and a file that is both shows up in each.
    #[test]
    fn dashboard_counts_staged_and_unstaged_apart() {
        let mut card = RepoCard::local("/tmp/x".into(), None);
        assert!(card.git.is_none());
        card.git = Some(crate::git::parse_status("## main\0M  a.rs\0 M b.rs\0MM c.rs\0?? d.rs\0"));
        let g = card.git.as_ref().unwrap();
        assert!(!g.is_clean());
        assert_eq!(g.staged_count(), 2);
        assert_eq!(g.unstaged_count(), 3);
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

    fn quiet_config() -> Config {
        let mut cfg = Config::default();
        // Keep tests silent and headless.
        cfg.ui.notify_sound = false;
        cfg.ui.ask_sound_enabled = false;
        cfg.ui.notify_desktop = false;
        cfg
    }

    #[test]
    fn a_question_chimes_when_it_appears_and_not_once_a_frame_after() {
        let mut st = open_working_tree("## main...origin/main\0");
        let cfg = quiet_config();

        // Nothing asked, nothing owed.
        announce_prompt(&mut st, &cfg);
        assert!(!st.prompt_sounded);

        offer_push(&mut st, "acme/api");
        announce_prompt(&mut st, &cfg);
        assert!(st.prompt_sounded, "the question that just appeared is announced");

        // The dialog stays up for as many frames as it takes to answer, and
        // ←/→ walks the buttons. Neither is a new question.
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_push_prompt(&mut st, press(KeyCode::Right), &tx);
        announce_prompt(&mut st, &cfg);
        announce_prompt(&mut st, &cfg);
        assert!(st.prompt_sounded);

        // Answered, the ledger clears — the next question gets its own chime.
        handle_push_prompt(&mut st, press(KeyCode::Esc), &tx);
        announce_prompt(&mut st, &cfg);
        assert!(!st.prompt_sounded, "a closed dialog rearms the chime");
        offer_push(&mut st, "acme/api");
        announce_prompt(&mut st, &cfg);
        assert!(st.prompt_sounded);
    }

    #[test]
    fn a_push_is_followed_into_ci_until_the_run_it_spawned_settles() {
        let mut st = dashboard();
        let cfg = quiet_config();
        start_push_watch(&mut st, "acme/api");
        let w = st.push_watches.first().expect("a push starts a watch");
        // The branch comes off the card's working tree, so only this branch's
        // runs can be adopted.
        assert_eq!(w.branch.as_deref(), Some("main"));
        assert!(w.run.is_none());

        // A run created after the push, on the pushed branch: adopted.
        settle_push_watch(&mut st, "acme/api", &[run_with(7, Status::Running)], &cfg);
        assert_eq!(
            st.push_watches[0].run.as_ref().map(|r| r.id),
            Some(7),
            "the spawned run is attached"
        );

        // The same run settling ends the watch.
        settle_push_watch(&mut st, "acme/api", &[run_with(7, Status::Failure)], &cfg);
        assert!(st.push_watches.is_empty(), "a settled run ends the watch");
    }

    #[test]
    fn runs_that_predate_the_push_or_ride_another_branch_are_not_adopted() {
        let mut st = dashboard();
        let cfg = quiet_config();
        start_push_watch(&mut st, "acme/api");

        let mut old = run_with(3, Status::Running);
        old.created_at = chrono::Utc::now() - chrono::Duration::minutes(10);
        let mut elsewhere = run_with(4, Status::Running);
        elsewhere.head_branch = "feature".into();

        settle_push_watch(&mut st, "acme/api", &[old, elsewhere], &cfg);
        assert!(
            st.push_watches[0].run.is_none(),
            "neither an old run nor another branch's is the push's offspring"
        );
    }

    // async: even constructing an octocrab client wants a tokio reactor.
    #[tokio::test]
    async fn a_watch_that_never_finds_a_run_gives_up() {
        let mut st = dashboard();
        start_push_watch(&mut st, "acme/api");
        st.tick_count += PUSH_WATCH_GIVE_UP_TICKS;
        let (tx, _rx) = mpsc::unbounded_channel();
        let provider = Arc::new(
            GitHubProvider::new(RepoSpec::parse("acme/api").unwrap(), "test-token".into())
                .unwrap(),
        );
        poll_push_watches(&mut st, &provider, &tx);
        assert!(st.push_watches.is_empty());
    }

    #[tokio::test]
    async fn one_key_re_asks_for_whatever_the_screen_is_showing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let provider = Arc::new(
            GitHubProvider::new(RepoSpec::parse("acme/api").unwrap(), "test-token".into())
                .unwrap(),
        );
        let mut st = dashboard();
        st.view = View::Runs;
        st.workflow_for_runs = Some("ci.yml".into());
        st.runs = vec![run_with(7, Status::Success)];

        refresh_current(&mut st, &provider, &tx);
        assert!(st.pending >= 1, "the run list was not asked for again");
        // The key says so: several of these change nothing visible when the
        // answer matches, and silence reads as an unbound key.
        assert_eq!(st.status_msg.as_deref(), Some("refreshing…"));

        // Turned away for pace: asking again is what extends the wait, so the
        // refusal is reported instead of argued with.
        st.pending = 0;
        st.hold_api(None);
        refresh_current(&mut st, &provider, &tx);
        assert_eq!(st.pending, 0, "a rate-limit hold was spent through");
        assert!(
            st.status_msg.as_deref().is_some_and(|m| m.contains("rate limited")),
            "got {:?}",
            st.status_msg
        );
    }

    #[test]
    fn refresh_owns_r_everywhere_and_the_reruns_moved_off_it() {
        let km = resolve_keymap(&KeymapConfig::default()).unwrap();
        assert_eq!(km.refresh, (KeyCode::Char('r'), KeyModifiers::NONE));
        // A hand that has learned `r` means refresh must not be able to start
        // CI with it, nor resume a batch commit.
        assert_ne!(km.rerun, km.refresh);
        assert_ne!(km.rerun_failed, km.refresh);
        assert_ne!(km.batch_retry, km.refresh);

        // Configs written when the key only ever re-read a working tree keep
        // working, under the name they used.
        let cfg: KeymapConfig = toml::from_str(r#"git_refresh = "R""#).unwrap();
        assert_eq!(cfg.refresh, "R");
        assert_eq!(resolve_keymap(&cfg).unwrap().refresh.0, KeyCode::Char('R'));
    }

    #[test]
    fn a_failed_run_lands_the_cursor_on_the_step_that_broke() {
        let step = |name: &str, status| Step {
            name: name.into(),
            status,
            started_at: None,
            completed_at: None,
        };
        let detail = RunDetail {
            run: run_with(1, Status::Failure),
            jobs: vec![
                crate::provider::Job {
                    id: 1,
                    name: "build".into(),
                    status: Status::Success,
                    steps: vec![step("compile", Status::Success)],
                },
                crate::provider::Job {
                    id: 2,
                    name: "test".into(),
                    status: Status::Failure,
                    steps: vec![
                        step("checkout", Status::Success),
                        step("pytest", Status::Failure),
                    ],
                },
            ],
        };
        // Rows flatten as job, its steps, next job, its steps — the failing
        // `pytest` step is row 4, and that is where a red run should open.
        assert_eq!(first_failed_item(&detail), Some(4));
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

    fn step(name: &str, status: Status, hms: &str) -> Step {
        let (h, m, s) = (
            hms[0..2].parse().unwrap(),
            hms[3..5].parse().unwrap(),
            hms[6..8].parse().unwrap(),
        );
        Step {
            name: name.into(),
            status,
            started_at: {
                use chrono::TimeZone;
                chrono::Utc.with_ymd_and_hms(2026, 8, 3, h, m, s).single()
            },
            completed_at: None,
        }
    }

    /// The shape of a real failing Windows `cargo test` job: the step's whole
    /// tail — test results, panic, `##[error]Process completed` — lands inside
    /// the same wall-clock second in which the next steps are reported to start.
    fn failing_test_job() -> (Vec<String>, Vec<Step>) {
        let mut raw = vec![
            "09:57:21 Current runner version: '2.331.0'".to_string(),
            "09:57:22 ##[group]Run actions/checkout@v4".to_string(),
            "09:57:29 ##[endgroup]".to_string(),
            "09:57:32 ##[group]Run cargo test --verbose".to_string(),
        ];
        raw.extend((0..20).map(|i| format!("09:57:5{} Downloaded crate {i}", i % 10)));
        // Everything from here shares one second with the next step's start.
        raw.push("09:59:58    Finished `test` profile in 2m 22s".into());
        raw.push("09:59:58 test git::tests::x ... FAILED".into());
        raw.push("09:59:58 failures:".into());
        raw.push("09:59:58 assertion `left == right` failed".into());
        raw.push("09:59:58 test result: FAILED. 75 passed; 1 failed".into());
        raw.push("09:59:58 error: test failed, to rerun pass `--bin jog`".into());
        raw.push("09:59:58 ##[error]Process completed with exit code 1.".into());
        raw.push("09:59:58 Post job cleanup.".into());
        raw.push("10:00:00 Cleaning up orphan processes".into());
        let steps = vec![
            step("Set up job", Status::Success, "09:57:21"),
            step("Run actions/checkout@v4", Status::Success, "09:57:22"),
            step("Run tests", Status::Failure, "09:57:32"),
            // Skipped because the tests failed — it produced no output at all.
            step("Build release", Status::Skipped, "09:59:58"),
            step("Post Run actions/checkout@v4", Status::Success, "09:59:58"),
            step("Complete job", Status::Success, "10:00:00"),
        ];
        (raw, steps)
    }

    #[test]
    fn a_failing_step_keeps_its_own_failure_output() {
        let (raw, steps) = failing_test_job();
        let (starts, names) = compute_step_line_starts(&raw, &steps);
        let idx = names.iter().position(|n| n == "Run tests").unwrap();
        let slice = extract_step_by_line_range(&raw, idx, &starts);
        let text = slice.join("\n");
        // Second-granularity timestamps used to hand this tail to the *skipped*
        // step that followed, truncating the failing step right before the part
        // that says what failed.
        assert!(text.contains("test result: FAILED"), "got:\n{text}");
        assert!(text.contains("assertion"), "got:\n{text}");
        assert!(text.contains("Process completed with exit code"), "got:\n{text}");
        // And not the next step's business.
        assert!(!text.contains("Post job cleanup"), "got:\n{text}");
    }

    #[test]
    fn a_skipped_step_claims_no_output() {
        let (raw, steps) = failing_test_job();
        let (starts, names) = compute_step_line_starts(&raw, &steps);
        let idx = names.iter().position(|n| n == "Build release").unwrap();
        assert!(extract_step_by_line_range(&raw, idx, &starts).is_empty());
    }

    #[test]
    fn a_skipped_step_does_not_steal_the_next_step_s_header() {
        // `Install musl tools` is skipped on Windows, and its start time equals
        // the *next* step's. Matching by timestamp alone hands it the
        // `##[group]Run rustup target add …` header that belongs to `Add Rust
        // target`, which is then left with nothing to show.
        let raw = vec![
            "09:57:21 Current runner version: '2.331.0'".to_string(),
            "09:57:29 ##[group]Run rustup target add x86_64-pc-windows-msvc".to_string(),
            "09:57:30   info: downloading component".to_string(),
            "09:57:32 ##[endgroup]".to_string(),
        ];
        let steps = vec![
            step("Set up job", Status::Success, "09:57:21"),
            step("Install musl tools", Status::Skipped, "09:57:29"),
            step("Add Rust target", Status::Success, "09:57:29"),
        ];
        let (starts, names) = compute_step_line_starts(&raw, &steps);
        let idx = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(extract_step_by_line_range(&raw, idx("Install musl tools"), &starts).is_empty());
        let added = extract_step_by_line_range(&raw, idx("Add Rust target"), &starts).join("\n");
        assert!(added.contains("rustup target add"), "got:\n{added}");
        assert!(added.contains("downloading component"), "got:\n{added}");
    }

    #[test]
    fn steps_after_the_marker_still_get_their_lines() {
        let (raw, steps) = failing_test_job();
        let (starts, names) = compute_step_line_starts(&raw, &steps);
        let post = names.iter().position(|n| n.starts_with("Post Run")).unwrap();
        let text = extract_step_by_line_range(&raw, post, &starts).join("\n");
        // Pushing boundaries past the end-of-step marker must not swallow what
        // legitimately comes after it.
        assert!(text.contains("Post job cleanup"), "got:\n{text}");
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

    // ── Regression: refresh, watch lifetime, and announcing ───────────────

    /// A runs list under a workflow, as the Runs view holds it.
    fn runs_view(ids: &[u64]) -> AppState {
        let mut st = empty_state();
        st.view = View::Runs;
        st.workflow_for_runs = Some("ci.yml".into());
        st.runs = ids.iter().map(|&i| run_with(i, Status::Running)).collect();
        st
    }

    /// What the `RunsLoaded` arm does to the cursor, extracted so the rule can
    /// be tested without standing up the whole event loop.
    fn apply_runs_loaded(st: &mut AppState, runs: Vec<Run>) {
        let held = (st.view != View::Watch)
            .then(|| st.selected_run().map(|r| r.id))
            .flatten();
        st.runs = runs;
        st.run_cursor = held
            .and_then(|id| st.runs.iter().position(|r| r.id == id))
            .unwrap_or(0);
    }

    #[test]
    fn a_poll_refresh_keeps_the_run_the_user_selected() {
        let mut st = runs_view(&[10, 9, 8, 7, 6]);
        st.run_cursor = 3; // run #7

        // The poll re-fetches while anything is in flight. Same list back.
        apply_runs_loaded(&mut st, (0..5).map(|i| run_with(10 - i, Status::Running)).collect());
        assert_eq!(st.selected_run().map(|r| r.id), Some(7), "cursor followed the run");

        // A newer run arrives at the head: the selection shifts down a row to
        // stay on the same run, rather than staying put and pointing at another.
        let grown: Vec<Run> = (0..6).map(|i| run_with(11 - i, Status::Running)).collect();
        apply_runs_loaded(&mut st, grown);
        assert_eq!(st.run_cursor, 4);
        assert_eq!(st.selected_run().map(|r| r.id), Some(7));

        // The selected run falls off the end of the window: back to the top,
        // which is the only honest answer left.
        apply_runs_loaded(&mut st, vec![run_with(20, Status::Running)]);
        assert_eq!(st.run_cursor, 0);
    }

    #[test]
    fn watch_still_follows_the_newest_run() {
        // Watch is about the run at the head of the list, by definition — the
        // hold-the-cursor rule must not leak into it.
        let mut st = runs_view(&[10, 9, 8]);
        st.view = View::Watch;
        st.run_cursor = 2;
        apply_runs_loaded(&mut st, vec![
            run_with(11, Status::Running),
            run_with(10, Status::Running),
        ]);
        assert_eq!(st.run_cursor, 0);
    }

    #[tokio::test]
    async fn a_watch_whose_run_ages_out_of_the_recent_list_stops_polling() {
        let mut st = dashboard();
        let cfg = quiet_config();
        start_push_watch(&mut st, "acme/api");
        settle_push_watch(&mut st, "acme/api", &[run_with(7, Status::Running)], &cfg);
        assert_eq!(st.push_watches[0].run.as_ref().map(|r| r.id), Some(7));

        // `list_repo_runs` is a fixed window over every workflow, so on a busy
        // repo the tracked run drops out of it and never returns. Kept, the
        // watch would spend a request per poll on it for the rest of the
        // session — and pin the header chip on a run that never resolves.
        let newer: Vec<Run> = (100..110).map(|i| run_with(i, Status::Running)).collect();
        settle_push_watch(&mut st, "acme/api", &newer, &cfg);
        assert!(st.push_watches.is_empty(), "the stale watch is dropped");
    }

    #[tokio::test]
    async fn a_watch_still_waiting_for_its_run_is_not_dropped_for_not_finding_one() {
        // The same "not found" answer means something different before a run has
        // been adopted: CI simply has not stamped it yet. That case has its own
        // deadline and must survive the check above.
        let mut st = dashboard();
        let cfg = quiet_config();
        start_push_watch(&mut st, "acme/api");
        let old = {
            let mut r = run_with(3, Status::Running);
            r.created_at = chrono::Utc::now() - chrono::Duration::minutes(10);
            r
        };
        settle_push_watch(&mut st, "acme/api", &[old], &cfg);
        assert_eq!(st.push_watches.len(), 1, "still waiting, not given up on");
        assert!(st.push_watches[0].run.is_none());
    }

    #[tokio::test]
    async fn a_watch_that_never_ends_is_still_bounded() {
        let mut st = dashboard();
        let cfg = quiet_config();
        start_push_watch(&mut st, "acme/api");
        // Adopted, and then queued forever because no runner is free.
        settle_push_watch(&mut st, "acme/api", &[run_with(7, Status::Queued)], &cfg);
        assert_eq!(st.push_watches.len(), 1);

        st.tick_count += PUSH_WATCH_MAX_TICKS;
        let (tx, _rx) = mpsc::unbounded_channel();
        let provider = Arc::new(
            GitHubProvider::new(RepoSpec::parse("acme/api").unwrap(), "test-token".into())
                .unwrap(),
        );
        poll_push_watches(&mut st, &provider, &tx);
        assert!(st.push_watches.is_empty(), "the hard ceiling ends it");
    }

    #[tokio::test]
    async fn a_run_two_watchers_see_settle_is_announced_once() {
        let mut st = dashboard();
        let cfg = quiet_config();
        let finished = run_with(7, Status::Failure);

        // Your own push, followed into CI, with the dashboard polling the same
        // repo. Both of them see run 7 in flight…
        start_push_watch(&mut st, "acme/api");
        settle_push_watch(&mut st, "acme/api", &[run_with(7, Status::Running)], &cfg);
        announce_if_finished(&mut st, &run_with(7, Status::Running), "acme/api", &cfg);

        // …and then both see it settle. The dashboard's poll lands first and
        // makes the noise.
        announce_if_finished(&mut st, &finished, "acme/api", &cfg);
        assert!(st.announced_runs.contains(&7));

        // The push watch arrives with the same run a moment later. It still ends
        // its watch — but it must not announce a second time.
        settle_push_watch(&mut st, "acme/api", std::slice::from_ref(&finished), &cfg);
        assert!(st.push_watches.is_empty(), "the watch is still finished off");
        assert!(
            !notify_run_finished(&mut st, &finished, "acme/api", &cfg),
            "the run was already announced — a second sound and popup is the bug"
        );
    }

    #[tokio::test]
    async fn a_push_watch_announces_a_run_the_dashboard_never_saw_running() {
        // The other direction: CI so fast that the first poll after the push
        // already shows the run finished. Nothing put it in `watch_seen_running`,
        // so the dedupe must not swallow the announcement outright.
        let mut st = dashboard();
        let cfg = quiet_config();
        start_push_watch(&mut st, "acme/api");
        settle_push_watch(&mut st, "acme/api", &[run_with(7, Status::Success)], &cfg);
        assert!(st.announced_runs.contains(&7), "the push's own run is announced");
        assert!(st.push_watches.is_empty());
    }
}
