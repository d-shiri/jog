use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use ratatui::text::{Line, Span};
use ratatui::style::{Color, Modifier, Style, Stylize};

use rayon::prelude::*;

use crate::config::KeymapConfig;
use crate::git::RepoStatus;
use crate::history::History;
use crate::provider::{Run, RunDetail, Status, Workflow};


#[derive(Debug, Clone, Copy)]
pub enum DetailItem {
    Job(usize),
    Step { job: usize, step: usize },
}

#[derive(Debug, Clone)]
pub struct LogGroup {
    pub header_line: usize,
    pub end_line: usize,
}

pub fn build_detail_items(detail: &RunDetail) -> Vec<DetailItem> {
    let mut items = Vec::new();
    for (ji, job) in detail.jobs.iter().enumerate() {
        items.push(DetailItem::Job(ji));
        for (si, _) in job.steps.iter().enumerate() {
            items.push(DetailItem::Step { job: ji, step: si });
        }
    }
    items
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Repos,
    GitStatus,
    GitDiff,
    Workflows,
    Runs,
    RunDetail,
    Logs,
    Watch,
    TriggerPrompt,
    Diff,
}

/// The working-tree view for one local checkout: stage, commit, push, then hand
/// off to CI.
#[derive(Debug, Clone)]
pub struct GitView {
    /// Key of the `RepoCard` this belongs to.
    pub spec: String,
    pub path: PathBuf,
    pub status: Option<RepoStatus>,
    pub cursor: usize,
    /// `Some` while typing a commit message; the inner string is the draft.
    pub commit_input: Option<String>,
    /// A git command is in flight — block further ones so two commits can't race.
    pub busy: bool,
    /// Whether this repo has a GitHub remote, i.e. whether CI can be triggered
    /// after committing.
    pub has_ci: bool,
}

impl GitView {
    pub fn new(spec: String, path: PathBuf, has_ci: bool) -> Self {
        Self {
            spec,
            path,
            status: None,
            cursor: 0,
            commit_input: None,
            busy: false,
            has_ci,
        }
    }

    pub fn entries(&self) -> &[crate::git::StatusEntry] {
        self.status.as_ref().map(|s| s.entries.as_slice()).unwrap_or(&[])
    }

    pub fn selected(&self) -> Option<&crate::git::StatusEntry> {
        self.entries().get(self.cursor)
    }

    pub fn staged_count(&self) -> usize {
        self.status.as_ref().map(|s| s.staged_count()).unwrap_or(0)
    }
}

/// How many lines of hook output to keep. A `pre-commit` running a full pytest
/// suite can print tens of thousands; the interesting ones are at the end, so
/// the front is what gets dropped.
const MAX_OP_LINES: usize = 4000;

/// One line of a running command's output, classified as it arrives.
///
/// Severity is computed once, on arrival, rather than on every frame: the pane
/// redraws at 10fps while a hook runs and re-scanning thousands of lines of
/// pytest output each time would be wasted work.
#[derive(Debug, Clone)]
pub struct OpLine {
    pub text: String,
    pub error: bool,
    pub warn: bool,
}

/// A streaming git command — `commit` or `push` — and everything it has printed.
///
/// This outlives the process on purpose. When a `pre-commit` hook fails, the
/// pytest or pyright output *is* the reason, and it is the only place that
/// reason exists: git exits non-zero and says nothing further. So a failed op
/// keeps its pane on screen, scrollable, until dismissed.
#[derive(Debug, Clone)]
pub struct GitOp {
    /// What the user pressed a key to do: `commit`, `push`.
    pub verb: String,
    /// The hook this command is expected to run, when the repo has one enabled.
    /// Names the pane, so a stalled-looking screen says *what* is stalling.
    pub hook: Option<String>,
    pub lines: Vec<OpLine>,
    /// Set when the process exits. Until then the pane follows the tail.
    pub finished: bool,
    pub failed: bool,
    /// Tick the command started on, for the elapsed-seconds readout.
    pub started_tick: u64,
    /// Index of the top visible line. `None` follows the tail, which is where a
    /// running command should sit; any scroll key pins it.
    pub scroll: Option<usize>,
    /// Lines dropped off the front to stay under `MAX_OP_LINES`.
    pub dropped: usize,
}

impl GitOp {
    pub fn new(verb: &str, hook: Option<String>, started_tick: u64) -> Self {
        Self {
            verb: verb.to_string(),
            hook,
            lines: Vec::new(),
            finished: false,
            failed: false,
            started_tick,
            scroll: None,
            dropped: 0,
        }
    }

    /// What to call this in the pane title: the hook when there is one, since
    /// "pre-commit hook" is the honest description of what the 40 seconds are
    /// being spent on, and the bare verb otherwise.
    pub fn label(&self) -> String {
        match &self.hook {
            Some(h) => format!("{h} hook"),
            None => self.verb.clone(),
        }
    }

    pub fn push_line(&mut self, text: String) {
        let (errors, warns) = classify_log_severity(std::slice::from_ref(&text));
        self.lines.push(OpLine {
            text,
            error: !errors.is_empty(),
            warn: !warns.is_empty(),
        });
        if self.lines.len() > MAX_OP_LINES {
            let excess = self.lines.len() - MAX_OP_LINES;
            self.lines.drain(..excess);
            self.dropped += excess;
            // Pinned scroll offsets index into the vector that just shifted.
            if let Some(s) = self.scroll.as_mut() {
                *s = s.saturating_sub(excess);
            }
        }
    }

    pub fn error_count(&self) -> usize {
        self.lines.iter().filter(|l| l.error).count()
    }

    /// Whole seconds since the command started. Ticks are 100ms.
    pub fn elapsed_secs(&self, now_tick: u64) -> u64 {
        now_tick.saturating_sub(self.started_tick) / 10
    }

    fn max_scroll(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport)
    }

    /// Index of the first visible line for a pane `viewport` rows tall.
    pub fn scroll_offset(&self, viewport: usize) -> usize {
        match self.scroll {
            Some(s) => s.min(self.max_scroll(viewport)),
            None => self.max_scroll(viewport),
        }
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let cur = self.scroll_offset(viewport) as isize;
        let next = (cur + delta).clamp(0, self.max_scroll(viewport) as isize);
        self.scroll = Some(next as usize);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = Some(0);
    }

    /// Return to following the tail — where a still-running command belongs.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = None;
    }

    /// Put the next (or previous) error line at the top of the pane, wrapping at
    /// the ends. Mirrors `e`/`E` in the log viewer, on the same classification.
    pub fn jump_error(&mut self, forward: bool, viewport: usize) -> bool {
        let from = self.scroll_offset(viewport);
        let hits: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.error)
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            return false;
        }
        let target = if forward {
            hits.iter().find(|&&i| i > from).copied().unwrap_or(hits[0])
        } else {
            hits.iter()
                .rev()
                .find(|&&i| i < from)
                .copied()
                .unwrap_or(*hits.last().unwrap())
        };
        self.scroll = Some(target);
        true
    }

    /// The whole output as text, for yanking into an editor or a bug report.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One line of a file diff. Section banners are kept apart from the diff text so
/// styling never has to guess which is which from the characters alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Section(String),
    Text(String),
}

/// The diff for a single file, opened from the working-tree view.
#[derive(Debug, Clone)]
pub struct GitDiffView {
    /// Repo the diff was requested for. A response that arrives after the user
    /// has moved on to another repo is dropped rather than shown under the
    /// wrong heading.
    pub spec: String,
    pub file: String,
    pub lines: Vec<DiffLine>,
    pub scroll: usize,
    pub loading: bool,
}

impl GitDiffView {
    pub fn new(spec: String, file: String) -> Self {
        Self {
            spec,
            file,
            lines: Vec::new(),
            scroll: 0,
            loading: true,
        }
    }

    /// Flatten git's output into display lines, banner-per-section.
    ///
    /// The banner is dropped when there is only one section: with nothing to
    /// tell apart, it is a row of screen space that says nothing.
    pub fn set_sections(&mut self, sections: Vec<crate::git::DiffSection>) {
        self.loading = false;
        self.scroll = 0;
        self.lines.clear();
        let labelled = sections.len() > 1;
        for (i, s) in sections.iter().enumerate() {
            if labelled {
                if i > 0 {
                    self.lines.push(DiffLine::Text(String::new()));
                }
                self.lines.push(DiffLine::Section(s.label.to_string()));
            }
            for line in s.text.lines() {
                self.lines.push(DiffLine::Text(line.to_string()));
            }
        }
    }

    /// Added and removed line counts, for the title.
    ///
    /// `+++`/`---` are file headers, not content, and would otherwise add a
    /// phantom insertion and deletion to every file.
    pub fn stats(&self) -> (usize, usize) {
        self.lines.iter().fold((0, 0), |(add, del), l| match l {
            DiffLine::Text(t) if t.starts_with("+++") || t.starts_with("---") => (add, del),
            DiffLine::Text(t) if t.starts_with('+') => (add + 1, del),
            DiffLine::Text(t) if t.starts_with('-') => (add, del + 1),
            _ => (add, del),
        })
    }

    /// Largest useful scroll offset: stop when the last line reaches the top of
    /// the viewport, so scrolling never runs off into empty space.
    pub fn max_scroll(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport.max(1))
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, self.max_scroll(viewport) as isize) as usize;
    }
}

/// One row of the multi-repo dashboard: a repo plus its most recent runs.
#[derive(Debug, Clone)]
pub struct RepoCard {
    /// Unique key and display label. `owner/name` when the GitHub remote is
    /// known, otherwise the local directory name.
    pub spec: String,
    /// `owner/name` when this repo has a GitHub remote we can query. `None` for
    /// a local checkout with no (or a non-GitHub) origin — still listable and
    /// committable, just without CI.
    pub remote: Option<String>,
    /// Local checkout, when this row came from a workspace scan.
    pub path: Option<PathBuf>,
    pub runs: Vec<Run>,
    /// Set when the last fetch for this repo failed (bad name, no access, …).
    pub error: Option<String>,
    pub loaded: bool,
    /// Working-tree state, refreshed alongside the run list. Only ever `Some`
    /// for rows with a local checkout.
    pub git: Option<RepoStatus>,
}

impl RepoCard {
    /// A row from `[provider] repos` — remote only, no checkout.
    pub fn new(spec: String) -> Self {
        Self {
            remote: Some(spec.clone()),
            spec,
            path: None,
            runs: Vec::new(),
            error: None,
            loaded: false,
            git: None,
        }
    }

    /// A row discovered by scanning a workspace directory.
    pub fn local(path: PathBuf, remote: Option<String>) -> Self {
        let dir_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            spec: remote.clone().unwrap_or(dir_name),
            remote,
            path: Some(path),
            runs: Vec::new(),
            error: None,
            loaded: false,
            git: None,
        }
    }

    /// Whether CI actions (runs, workflows, triggers) are possible for this row.
    pub fn has_ci(&self) -> bool {
        self.remote.is_some()
    }

    /// Status of the most recent run, which is what the dashboard row reports.
    pub fn latest_status(&self) -> Option<Status> {
        self.runs.first().map(|r| r.status)
    }

    /// Counts across the loaded runs: (success, failure, in-flight).
    pub fn counts(&self) -> (u32, u32, u32) {
        self.runs.iter().fold((0, 0, 0), |(o, f, r), run| match run.status {
            Status::Success => (o + 1, f, r),
            Status::Failure => (o, f + 1, r),
            Status::Running | Status::Queued => (o, f, r + 1),
            _ => (o, f, r),
        })
    }
}

/// What the fuzzy finder is currently picking from. Each variant maps a match
/// back onto a cursor in the underlying list, so committing a choice is just
/// "move the cursor there".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderKind {
    Workflows,
    Runs,
    DetailItems,
    Repos,
    GitEntries,
}

#[derive(Debug, Clone)]
pub struct Finder {
    pub kind: FinderKind,
    pub query: String,
    /// (index into the underlying list, searchable label).
    pub items: Vec<(usize, String)>,
    /// Indices into `items`, best match first. Equals all of `items` when the
    /// query is empty.
    pub matches: Vec<usize>,
    pub cursor: usize,
}

impl Finder {
    pub fn new(kind: FinderKind, items: Vec<(usize, String)>) -> Self {
        let matches = (0..items.len()).collect();
        Self {
            kind,
            query: String::new(),
            items,
            matches,
            cursor: 0,
        }
    }

    /// Re-rank `items` against the current query. Ties keep the original list
    /// order, so an empty query shows the list exactly as it appears behind the
    /// overlay.
    pub fn recompute(&mut self) {
        if self.query.is_empty() {
            self.matches = (0..self.items.len()).collect();
        } else {
            let mut scored: Vec<(i32, usize)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, (_, label))| fuzzy_score(label, &self.query).map(|s| (s, i)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            self.matches = scored.into_iter().map(|(_, i)| i).collect();
        }
        self.cursor = self.cursor.min(self.matches.len().saturating_sub(1));
    }

    /// The underlying-list index currently highlighted.
    pub fn selected_target(&self) -> Option<usize> {
        let item_idx = *self.matches.get(self.cursor)?;
        self.items.get(item_idx).map(|(target, _)| *target)
    }
}

/// Score `needle` as a subsequence of `haystack`, case-insensitively.
/// `None` means "not a match at all". Higher is better.
///
/// The weighting is deliberately simple: reward runs of adjacent characters and
/// matches that land at a word boundary, penalise leading gaps. That is enough
/// to make `dtp` rank `deploy_to_prod.yml` above `docker-test-pipeline.yml`
/// without pulling in a matcher crate.
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().flat_map(|c| c.to_lowercase()).collect();
    let need: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut last_match: Option<usize> = None;
    for &nc in &need {
        let found = hay[hi..].iter().position(|&hc| hc == nc)? + hi;
        // Adjacent to the previous match: strong signal the user typed a prefix.
        if last_match == Some(found.wrapping_sub(1)) {
            score += 15;
        }
        // Start of the string or just after a separator.
        let boundary = found == 0
            || matches!(hay[found - 1], '_' | '-' | '.' | '/' | ' ' | ':');
        if boundary {
            score += 10;
        }
        // Each skipped character costs a little, so earlier matches win.
        score -= (found.saturating_sub(last_match.map(|l| l + 1).unwrap_or(0))) as i32;
        last_match = Some(found);
        hi = found + 1;
    }
    // Prefer shorter candidates when the matched content is otherwise equal.
    score -= (hay.len() / 8) as i32;
    Some(score)
}

#[derive(Debug, Clone)]
pub struct TriggerField {
    pub name: String,
    pub value: String,
    pub required: bool,
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct TriggerPrompt {
    pub workflow_file: String,
    pub workflow_name: String,
    pub fields: Vec<TriggerField>,
    pub cursor: usize,
    pub editing: bool,
    /// View to return to on cancel or after submit.
    pub return_view: View,
}

impl TriggerPrompt {
    pub fn from_workflow(workflow: &Workflow, return_view: View) -> Self {
        let fields = workflow
            .inputs
            .iter()
            .map(|i| TriggerField {
                name: i.name.clone(),
                value: i.default.clone().unwrap_or_default(),
                required: i.required,
                options: i.options.clone(),
            })
            .collect();
        Self {
            workflow_file: workflow.file_name.clone(),
            workflow_name: workflow.name.clone(),
            fields,
            cursor: 0,
            editing: false,
            return_view,
        }
    }

    pub fn current_field(&self) -> Option<&TriggerField> {
        self.fields.get(self.cursor)
    }

    pub fn current_field_mut(&mut self) -> Option<&mut TriggerField> {
        self.fields.get_mut(self.cursor)
    }

    pub fn cycle_option(&mut self) {
        if let Some(f) = self.current_field_mut()
            && let Some(opts) = f.options.clone() {
                if opts.is_empty() {
                    return;
                }
                let idx = opts.iter().position(|o| o == &f.value).unwrap_or(0);
                f.value = opts[(idx + 1) % opts.len()].clone();
            }
    }

    pub fn missing_required(&self) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|f| f.required && f.value.is_empty())
            .map(|f| f.name.as_str())
            .collect()
    }

    pub fn collected(&self) -> std::collections::HashMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.name.clone(), f.value.clone()))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub header_bg: Color,
    pub footer_bg: Color,
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub success: Color,
    pub failure: Color,
    pub warning: Color,
    pub unknown: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            header_bg: Color::Rgb(28, 30, 42),
            footer_bg: Color::Rgb(28, 30, 42),
            primary: Color::Cyan,
            secondary: Color::Rgb(120, 120, 145),
            accent: Color::Yellow,
            success: Color::Green,
            failure: Color::Red,
            warning: Color::Yellow,
            unknown: Color::DarkGray,
        }
    }
}

#[derive(Debug)]
pub struct AppState {
    pub view: View,
    pub workflows: Vec<Workflow>,
    pub workflow_cursor: usize,
    pub runs: Vec<Run>,
    pub run_cursor: usize,
    pub run_detail: Option<RunDetail>,
    pub detail_cursor: usize,
    pub log_lines: Vec<String>,
    pub log_raw: Vec<String>,
    pub log_sections: Vec<String>,
    pub log_section_idx: Option<usize>,
    pub log_job_idx: Option<usize>,
    pub log_step_line_starts: Vec<usize>,
    pub log_step_names: Vec<String>,
    pub log_groups: Vec<LogGroup>,
    pub log_collapsed: HashSet<usize>,
    pub log_line_cursor: u16,
    pub log_group_header_rows: Vec<u16>,
    pub log_rendered_group_map: HashMap<u16, usize>,
    /// Focus mode: show only error/warning lines plus `log_focus_context` lines
    /// of surrounding context, ignoring group collapse state.
    pub log_focus: bool,
    pub log_focus_context: usize,
    /// Fold markers standing in for skipped lines while focused: rendered row ->
    /// how many source lines it hides.
    pub log_fold_rows: HashMap<u16, usize>,
    /// Source line each fold starts at, for folds the user has opened back up.
    /// Two lines of context is enough to spot an error and never enough to
    /// understand it, so the surrounding block has to be one keypress away.
    pub log_focus_expanded: HashSet<usize>,
    /// Indices into `log_lines` that look like errors — the jump targets for
    /// next/prev-error and the seeds for focus mode.
    pub log_error_lines: Vec<usize>,
    /// Indices into `log_lines` for warnings. Focus mode keeps these too.
    pub log_warn_lines: Vec<usize>,
    /// Rendered row -> index into `log_lines`. Lets us translate a source line
    /// (an error, a search hit) into the cursor position that displays it.
    pub log_rendered_src: Vec<usize>,
    /// (step_name, started_hms, completed_hms) stored when navigating into logs from a specific step.
    /// started/completed are "HH:MM:SS" strings derived from the GitHub API step timestamps.
    pub log_pending_section: Option<(String, Option<String>, Option<String>)>,
    pub log_scroll: u16,
    /// Inner viewport height of the Logs pane, captured at last render.
    /// Used to clamp `log_scroll` so users can't scroll past the bottom.
    /// Cell so render can write through `&AppState`.
    pub last_logs_viewport_height: Cell<u16>,
    /// Inner viewport width of the Logs pane, captured at last render.
    /// Needed to predict how many rows a line occupies once wrapped.
    pub last_logs_viewport_width: Cell<u16>,
    /// `Some` while user is typing into the search prompt; the inner string is
    /// the in-progress query. Committed on Enter (moves to `log_search_query`).
    pub log_search_input: Option<String>,
    /// Active committed query. While set, `n`/`N` jump between matches.
    pub log_search_query: Option<String>,
    /// Indices into `log_lines` where the query matches (case-insensitive).
    pub log_search_matches: Vec<usize>,
    /// Index into `log_search_matches`.
    pub log_search_match_idx: Option<usize>,
    pub status_msg: Option<String>,
    pub status_msg_tick: u64,
    pub repo_label: String,
    /// True while `repo_label` is a bootstrap guess rather than a repo the user
    /// chose. In workspace mode there is no checkout to read a remote from, so
    /// startup picks the first discovered repo that has one — an arbitrary pick
    /// the dashboard must not advertise as "active". Cleared on the first real
    /// repo switch.
    pub repo_label_implicit: bool,
    pub current_branch: String,
    pub workflow_for_runs: Option<String>,
    /// Preview pane in Workflows view: recent runs for the highlighted workflow.
    pub workflow_preview_file: Option<String>,
    pub workflow_preview_runs: Vec<Run>,
    /// Preview pane in Runs view: detail for the highlighted run.
    pub runs_preview: Option<RunDetail>,
    pub runs_preview_id: Option<u64>,
    /// Pre-rendered log lines for TUI performance.
    pub log_rendered: Vec<Line<'static>>,
    /// Pending async work indicator (count of in-flight tasks)
    pub pending: usize,
    /// Counter incremented on every UI tick (used for animations)
    pub tick_count: u64,
    /// Set true when transitioning views so the event loop can `terminal.clear()`.
    pub needs_clear: bool,
    pub trigger_prompt: Option<TriggerPrompt>,
    pub keymap: KeymapConfig,
    pub history: History,
    pub theme: Theme,
    /// Run IDs we have seen in a non-terminal state during this Watch session.
    /// Used to fire a sound only when a run we were actively watching finishes.
    pub watch_seen_running: HashSet<u64>,
    /// Multi-repo dashboard rows, in configured order.
    pub repos: Vec<RepoCard>,
    pub repo_cursor: usize,
    /// Open fuzzy finder, if any. Rendered as an overlay above the current view.
    pub finder: Option<Finder>,
    /// Working-tree view for the repo we drilled into from the dashboard.
    pub git_view: Option<GitView>,
    /// Running and failed commits/pushes, keyed by repo.
    ///
    /// Held here rather than on `GitView` so a hook's output outlives the view
    /// it was started from: a 90-second `pre-commit` should not pin you to one
    /// screen, and walking away must not be the same as discarding the failure.
    /// Also what lets a dashboard row say that a repo is mid-commit.
    pub git_ops: HashMap<String, GitOp>,
    /// Diff for the file drilled into from the working-tree view.
    pub git_diff: Option<GitDiffView>,
    /// Rows the diff pane last drew, so paging and clamping match what is
    /// actually on screen rather than a guess.
    pub last_diff_viewport_height: Cell<u16>,
    /// Rows the hook-output pane last drew, for the same reason.
    pub last_op_viewport_height: Cell<u16>,
    /// Keybinding reference overlay.
    pub show_help: bool,
    pub help_scroll: u16,
    /// Directory the workspace scan was rooted at, when running outside a repo.
    pub workspace_root: Option<PathBuf>,
}

impl AppState {
    pub fn new(repo_label: String, current_branch: String, mut workflows: Vec<Workflow>, keymap: KeymapConfig, history: History) -> Self {
        for w in &mut workflows {
            if let Some(entry) = history.last_run(&w.file_name) {
                w.last_run_at = Some(entry.created_at);
                w.last_status = Some(entry.status);
            }
        }
        Self {
            view: View::Workflows,
            workflows,
            workflow_cursor: 0,
            runs: Vec::new(),
            run_cursor: 0,
            run_detail: None,
            detail_cursor: 0,
            log_lines: Vec::new(),
            log_raw: Vec::new(),
            log_sections: Vec::new(),
            log_section_idx: None,
            log_job_idx: None,
            log_step_line_starts: Vec::new(),
            log_step_names: Vec::new(),
            log_groups: Vec::new(),
            log_collapsed: HashSet::new(),
            log_line_cursor: 0,
            log_group_header_rows: Vec::new(),
            log_rendered_group_map: HashMap::new(),
            log_focus: false,
            log_focus_context: 2,
            log_fold_rows: HashMap::new(),
            log_focus_expanded: HashSet::new(),
            log_error_lines: Vec::new(),
            log_warn_lines: Vec::new(),
            log_rendered_src: Vec::new(),
            log_pending_section: None,
            log_scroll: 0,
            last_logs_viewport_height: Cell::new(0),
            last_logs_viewport_width: Cell::new(0),
            log_search_input: None,
            log_search_query: None,
            log_search_matches: Vec::new(),
            log_search_match_idx: None,
            status_msg: None,
            status_msg_tick: 0,
            repo_label,
            repo_label_implicit: false,
            current_branch,
            workflow_for_runs: None,
            workflow_preview_file: None,
            workflow_preview_runs: Vec::new(),
            runs_preview: None,
            runs_preview_id: None,
            log_rendered: Vec::new(),
            pending: 0,
            tick_count: 0,
            needs_clear: false,
            trigger_prompt: None,
            keymap,
            history,
            theme: Theme::default(),
            watch_seen_running: HashSet::new(),
            repos: Vec::new(),
            repo_cursor: 0,
            finder: None,
            git_view: None,
            git_ops: HashMap::new(),
            git_diff: None,
            last_diff_viewport_height: Cell::new(0),
            last_op_viewport_height: Cell::new(0),
            show_help: false,
            help_scroll: 0,
            workspace_root: None,
        }
    }

    pub fn switch_view(&mut self, v: View) {
        if self.view != v {
            self.view = v;
            self.needs_clear = true;
        }
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some(msg);
        self.status_msg_tick = self.tick_count;
    }

    /// The commit/push output belonging to the repo whose working tree is open.
    ///
    /// Anything the user can see or scroll is this one; the rest of the map is
    /// other repos' business.
    pub fn current_op(&self) -> Option<&GitOp> {
        let spec = &self.git_view.as_ref()?.spec;
        self.git_ops.get(spec)
    }

    pub fn current_op_mut(&mut self) -> Option<&mut GitOp> {
        let spec = self.git_view.as_ref()?.spec.clone();
        self.git_ops.get_mut(&spec)
    }

    /// Whether a commit or push is still running for `spec`.
    ///
    /// Checked against the map rather than `GitView::busy` because leaving the
    /// working-tree view drops the view but not the command: without this, going
    /// back in and pressing `c` again would start a second `git commit` on a
    /// repo that is already in one.
    pub fn op_running(&self, spec: &str) -> bool {
        self.git_ops.get(spec).is_some_and(|o| !o.finished)
    }

    pub fn current_step_idx(&self) -> Option<usize> {
        if self.log_step_names.is_empty() { None } else { self.log_section_idx }
    }

    pub fn selected_workflow(&self) -> Option<&Workflow> {
        self.workflows.get(self.workflow_cursor)
    }

    pub fn selected_run(&self) -> Option<&Run> {
        self.runs.get(self.run_cursor)
    }

    /// Rebuild every index derived from `log_lines`: groups, collapse state,
    /// and the error/warning line index used by focus mode and error jumps.
    /// Call this whenever `log_lines` is replaced.
    pub fn init_log_groups(&mut self) {
        self.log_groups = parse_log_groups(&self.log_lines);
        self.log_collapsed = (0..self.log_groups.len()).collect();
        self.log_line_cursor = 0;
        self.log_group_header_rows = vec![0u16; self.log_groups.len()];
        self.log_rendered_group_map = HashMap::new();
        let (errors, warnings) = classify_log_severity(&self.log_lines);
        self.log_error_lines = errors;
        self.log_warn_lines = warnings;
        // Folds are opened by source line, which means nothing once the source
        // is a different step's log.
        self.log_focus_expanded.clear();
        self.log_fold_rows.clear();
        // Focus mode survives a step change, but with nothing to focus on it
        // would hide every line and leave a blank pane with no explanation.
        if self.log_error_lines.is_empty() && self.log_warn_lines.is_empty() {
            self.log_focus = false;
        }
    }

    pub fn compute_hidden_lines(&self) -> HashSet<usize> {
        if self.log_focus {
            return self.compute_focus_hidden();
        }
        let mut hidden = HashSet::new();
        for (gi, group) in self.log_groups.iter().enumerate() {
            if self.log_collapsed.contains(&gi) {
                for li in (group.header_line + 1)..=group.end_line {
                    hidden.insert(li);
                }
            }
        }
        hidden
    }

    /// Everything that is *not* an error/warning or within `log_focus_context`
    /// lines of one. Group collapse is deliberately ignored while focused —
    /// hiding an error because its group happens to be folded would defeat the
    /// point of the mode.
    fn compute_focus_hidden(&self) -> HashSet<usize> {
        let ctx = self.log_focus_context;
        let mut keep = HashSet::new();
        for &i in self.log_error_lines.iter().chain(self.log_warn_lines.iter()) {
            let lo = i.saturating_sub(ctx);
            let hi = (i + ctx).min(self.log_lines.len().saturating_sub(1));
            for l in lo..=hi {
                keep.insert(l);
            }
        }
        let mut hidden: HashSet<usize> = (0..self.log_lines.len())
            .filter(|i| !keep.contains(i))
            .collect();
        // A fold the user opened stays open: walk each contiguous hidden run and
        // release the whole run if its first line is one they asked for.
        if !self.log_focus_expanded.is_empty() {
            for run in self.hidden_runs(&hidden) {
                if self.log_focus_expanded.contains(&run.0) {
                    for l in run.0..=run.1 {
                        hidden.remove(&l);
                    }
                }
            }
        }
        hidden
    }

    /// Contiguous `(first, last)` spans of hidden source lines, in order.
    fn hidden_runs(&self, hidden: &HashSet<usize>) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut start: Option<usize> = None;
        for i in 0..self.log_lines.len() {
            match (hidden.contains(&i), start) {
                (true, None) => start = Some(i),
                (false, Some(s)) => {
                    runs.push((s, i - 1));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            runs.push((s, self.log_lines.len() - 1));
        }
        runs
    }

    /// Open the fold under `row`, if that row is one. Returns whether anything
    /// changed, so the caller knows whether to re-render.
    pub fn expand_fold_at(&mut self, row: u16) -> bool {
        if !self.log_fold_rows.contains_key(&row) {
            return false;
        }
        match self.log_rendered_src.get(row as usize).copied() {
            Some(anchor) => self.log_focus_expanded.insert(anchor),
            None => false,
        }
    }

    /// How many screen rows rendered line `row` occupies once wrapped.
    ///
    /// `log_scroll` is fed to a wrapping `Paragraph`, which counts *visual* rows,
    /// while cursors index *logical* lines. A single `rustc` invocation can wrap
    /// to twenty rows, so the two diverge badly on real logs and any scroll
    /// computed in logical lines lands in the wrong place.
    ///
    /// Width is approximated by character count. That is exact for the ASCII
    /// that dominates CI logs and only under-counts for wide glyphs, where the
    /// worst case is scrolling slightly short of the target.
    pub fn visual_height_of_row(&self, row: usize) -> u16 {
        let width = self.last_logs_viewport_width.get().max(1) as usize;
        let Some(line) = self.log_rendered.get(row) else {
            return 1;
        };
        wrapped_rows(&line_text(line), width)
    }

    /// Index of the last rendered line that still fits when drawing starts at
    /// `first`. Always at least `first`, even if that one line overflows.
    pub fn last_visible_row(&self, first: usize) -> usize {
        let viewport = self.last_logs_viewport_height.get().max(1) as usize;
        let mut used = 0usize;
        let mut last = first;
        for row in first..self.log_rendered.len() {
            let h = self.visual_height_of_row(row) as usize;
            if used + h > viewport && row > first {
                break;
            }
            used += h;
            last = row;
        }
        last
    }

    /// Largest useful `log_scroll`: the first line such that everything after it
    /// still fills the viewport, so the last line can be reached but no further.
    pub fn max_log_scroll(&self) -> u16 {
        let viewport = self.last_logs_viewport_height.get().max(1) as usize;
        let mut used = 0usize;
        for row in (0..self.log_rendered.len()).rev() {
            used += self.visual_height_of_row(row) as usize;
            if used > viewport {
                return (row + 1).min(u16::MAX as usize) as u16;
            }
        }
        0
    }

    /// Scroll the minimum amount needed to bring the cursor line on screen.
    ///
    /// Scrolling down is computed by walking *backwards* from the cursor until
    /// the viewport is full, which is O(viewport). Advancing the first visible
    /// line one at a time instead would rescan forward on every step — O(n²),
    /// and a visible freeze when jumping to the end of a long log.
    pub fn keep_cursor_visible(&mut self) {
        let cursor = self.log_line_cursor as usize;
        if (cursor as u16) < self.log_scroll {
            self.log_scroll = cursor as u16;
            return;
        }
        if self.last_visible_row(self.log_scroll as usize) >= cursor {
            return; // already on screen
        }
        let viewport = self.last_logs_viewport_height.get().max(1) as usize;
        let mut used = self.visual_height_of_row(cursor) as usize;
        let mut first = cursor;
        while first > 0 {
            let h = self.visual_height_of_row(first - 1) as usize;
            if used + h > viewport {
                break;
            }
            used += h;
            first -= 1;
        }
        // Only ever scrolls down here; the cursor-above case returned earlier.
        self.log_scroll = self.log_scroll.max(first as u16);
    }

    /// Apply cursor and current-search-hit decoration to the rows about to be
    /// drawn. Kept off the rebuild path so moving the cursor stays O(viewport).
    ///
    /// `first` is the index of the first visible row; the returned lines are the
    /// slice starting there, already decorated.
    pub fn decorate_visible(&self, first: usize, count: usize) -> Vec<Line<'static>> {
        let end = first.saturating_add(count).min(self.log_rendered.len());
        if first >= end {
            return Vec::new();
        }
        let cursor = self.log_line_cursor as usize;
        let cursor_bg = Style::default().bg(Color::Rgb(35, 42, 58));
        let current_match_src = self
            .log_search_match_idx
            .and_then(|i| self.log_search_matches.get(i).copied());
        let needle = self
            .log_search_query
            .as_deref()
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase());

        self.log_rendered[first..end]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                let row = first + offset;
                let mut out = line.clone();
                // The current hit is drawn brighter than the others. Only this
                // one line needs re-splitting, so it is cheap here.
                if let (Some(needle), Some(src)) = (needle.as_deref(), current_match_src)
                    && self.log_rendered_src.get(row) == Some(&src)
                {
                    out = highlight_line(out, needle, true);
                }
                if row == cursor {
                    out = out.style(cursor_bg);
                }
                out
            })
            .collect()
    }

    /// Put the cursor line roughly a third of the way down the viewport — used
    /// when jumping somewhere (an error, a search hit) rather than stepping.
    ///
    /// The context above is measured by walking backwards from the cursor, so
    /// any wrap-estimate error is bounded by that short window instead of
    /// accumulating over the whole log.
    pub fn center_cursor(&mut self) {
        let viewport = self.last_logs_viewport_height.get().max(1) as usize;
        let budget = viewport / 3;
        let mut used = 0usize;
        let mut first = self.log_line_cursor as usize;
        while first > 0 {
            let h = self.visual_height_of_row(first - 1) as usize;
            if used + h > budget {
                break;
            }
            used += h;
            first -= 1;
        }
        self.log_scroll = first.min(u16::MAX as usize) as u16;
    }

    /// Cursor row that displays source line `src`, if it is currently visible.
    pub fn rendered_row_for_src(&self, src: usize) -> Option<u16> {
        self.log_rendered_src
            .iter()
            .position(|&s| s == src)
            .map(|r| r as u16)
    }

    /// Index of the group containing `src`, if any.
    pub fn group_containing(&self, src: usize) -> Option<usize> {
        self.log_groups
            .iter()
            .position(|g| src > g.header_line && src <= g.end_line)
    }

    /// Drop in-progress and committed search state. Called whenever
    /// `log_lines` is replaced (section change, fresh fetch).
    pub fn clear_log_search(&mut self) {
        self.log_search_input = None;
        self.log_search_query = None;
        self.log_search_matches.clear();
        self.log_search_match_idx = None;
    }

    /// Recompute match positions for the current query against `log_lines`.
    /// Matching uses the same "visible" form the renderer produces — ANSI,
    /// time prefix, and `##[…]` markup are stripped so the user searches
    /// what they actually see on screen.
    pub fn recompute_log_matches(&mut self) {
        self.log_search_matches.clear();
        self.log_search_match_idx = None;
        let Some(q) = self.log_search_query.as_deref() else { return };
        if q.is_empty() {
            return;
        }
        let needle = q.to_lowercase();
        // Each line is matched independently; `par_iter` keeps source order, so
        // the match list stays sorted and n/p still walk the log top to bottom.
        self.log_search_matches = self
            .log_lines
            .par_iter()
            .enumerate()
            .filter(|(_, line)| visible_text(line).to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if !self.log_search_matches.is_empty() {
            self.log_search_match_idx = Some(0);
        }
    }

    /// Rebuild the rendered log from `log_lines`.
    ///
    /// This is the expensive pass — ANSI parsing and span splitting for every
    /// visible line — so it must only run when the *content* changes: new logs,
    /// a group folded, focus toggled, a new search query. Cursor position and
    /// which search hit is "current" are pure decoration and are applied to the
    /// visible slice at draw time instead (see `decorate_visible`), because
    /// otherwise every `j` would re-render the whole buffer.
    pub fn recompute_log_rendered(&mut self) {
        let needle_lower = self
            .log_search_query
            .as_deref()
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase());

        let time_style = Style::default().fg(Color::Rgb(80, 80, 80));

        let header_to_group: HashMap<usize, usize> = self.log_groups.iter().enumerate()
            .map(|(gi, g)| (g.header_line, gi))
            .collect();

        let hidden = self.compute_hidden_lines();

        // Pass 1 (sequential, cheap): which source lines survive filtering, and
        // where each group header lands. Row positions have to be assigned in
        // order, so this can't be parallel — but it's just index bookkeeping.
        let mut rendered_src: Vec<usize> = Vec::with_capacity(self.log_lines.len());
        let mut group_header_rows = vec![0u16; self.log_groups.len()];
        let mut group_map: HashMap<u16, usize> = HashMap::new();
        let mut fold_rows: HashMap<u16, usize> = HashMap::new();
        let mut rendered_row: u16 = 0;
        // Run of consecutive dropped lines, tracked only while focused, which
        // becomes one "N lines hidden" row. Without it the filtered log is a
        // stack of fragments with no sign of the distance between them.
        //
        // Only *hidden* lines count and only a hidden line can anchor the fold:
        // an `##[endgroup]` is dropped in every mode, so it is neither something
        // the user is missing nor something opening the fold would bring back.
        let mut skipped: Option<(usize, usize)> = None;
        for (src_idx, l) in self.log_lines.iter().enumerate() {
            let is_endgroup = split_time_prefix(l.as_str()).1.starts_with("##[endgroup]");
            if hidden.contains(&src_idx) || is_endgroup {
                if self.log_focus && hidden.contains(&src_idx) {
                    skipped = match skipped {
                        Some((first, n)) => Some((first, n + 1)),
                        None => Some((src_idx, 1)),
                    };
                }
                continue;
            }
            if let Some((first, n)) = skipped.take() {
                fold_rows.insert(rendered_row, n);
                // The fold points at the first line it swallowed, so opening it
                // has an anchor and a jump into hidden text lands on the fold
                // that contains it rather than nowhere.
                rendered_src.push(first);
                rendered_row += 1;
            }
            if let Some(&gi) = header_to_group.get(&src_idx) {
                group_header_rows[gi] = rendered_row;
                group_map.insert(rendered_row, gi);
            }
            rendered_src.push(src_idx);
            rendered_row += 1;
        }
        // A log that ends in skipped lines still has to say so, or focus mode
        // looks like it stopped early.
        if let Some((first, n)) = skipped.take() {
            fold_rows.insert(rendered_row, n);
            rendered_src.push(first);
        }

        // Pass 2 (parallel): the expensive part — ANSI parsing and span
        // splitting per line, which is independent for every row. `par_iter`
        // preserves order, so row indices stay aligned with pass 1.
        let collapsed = &self.log_collapsed;
        let lines = &self.log_lines;
        let folds = &fold_rows;
        let rendered: Vec<Line<'static>> = rendered_src
            .par_iter()
            .enumerate()
            .map(|(row, &src_idx)| {
                if let Some(&n) = folds.get(&(row as u16)) {
                    let plural = if n == 1 { "line" } else { "lines" };
                    return Line::from(Span::styled(
                        format!("  ⋯ {n} {plural} hidden — ↵ to show ⋯"),
                        Style::default().fg(Color::Rgb(105, 105, 135)).italic(),
                    ));
                }
                let l = &lines[src_idx];
                let (time, content) = split_time_prefix(l.as_str());
                let mk_time = || time.map(|t| Span::styled(format!("{t} "), time_style));

            let line: Line = if let Some(&gi) = header_to_group.get(&src_idx) {
                let is_collapsed = collapsed.contains(&gi);
                let title = content.strip_prefix("##[group]")
                    .or_else(|| content.strip_prefix("##[section]"))
                    .unwrap_or(content);
                let title_style = Style::default().fg(Color::Cyan).bold();
                let arrow = if is_collapsed { "▶ " } else { "▾ " };
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled(arrow, title_style));
                spans.extend(ansi_line_to_spans(title, title_style));
                Line::from(spans)
            } else if let Some(cmd) = content.strip_prefix("##[command]") {
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("▶ ", Style::default().fg(Color::Green).bold()));
                spans.extend(ansi_line_to_spans(cmd, Style::default().fg(Color::White)));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[error]") {
                let s = Style::default().fg(Color::Red).bold();
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("✗ ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[warning]") {
                let s = Style::default().fg(Color::Yellow);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("⚠ ", s.bold()));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[debug]") {
                let s = Style::default().fg(Color::DarkGray);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("# ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[notice]") {
                let s = Style::default().fg(Color::Cyan);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("ℹ ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else {
                // Keyword detection runs on plain text regardless of ANSI presence.
                // For ANSI lines the detected style becomes the default that ANSI
                // resets (`\x1b[0m`) fall back to, so "FAILED" lines stay red even
                // after the escape sequence ends.
                let plain = if content.contains('\x1b') {
                    strip_ansi(content)
                } else {
                    content.to_string()
                };
                let trimmed_lower = plain.trim_start().to_lowercase();
                let base = if trimmed_lower.starts_with("error") || trimmed_lower.starts_with("failed") {
                    Style::default().fg(Color::Red)
                } else if trimmed_lower.starts_with("warn") {
                    Style::default().fg(Color::Yellow)
                } else if trimmed_lower.starts_with('=') && trimmed_lower.len() > 3
                    && trimmed_lower[..4].chars().all(|c| c == '=')
                {
                    Style::default().fg(Color::Yellow).bold()
                } else if trimmed_lower.starts_with('-') && trimmed_lower.len() > 3
                    && trimmed_lower[..4].chars().all(|c| c == '-')
                {
                    Style::default().fg(Color::Rgb(100, 100, 100))
                } else {
                    Style::default().fg(Color::Rgb(200, 200, 200))
                };
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.extend(ansi_line_to_spans(content, base));
                Line::from(spans)
            };

                match needle_lower.as_deref() {
                    Some(needle) => highlight_line(line, needle, false),
                    None => line,
                }
            })
            .collect();

        self.log_rendered = rendered;
        self.log_rendered_src = rendered_src;
        self.log_group_header_rows = group_header_rows;
        self.log_rendered_group_map = group_map;
        self.log_fold_rows = fold_rows;
    }
}

/// Strip ANSI, the `HH:MM:SS` time prefix and any GitHub Actions `##[...]`
/// markup so we operate on the same characters the renderer shows. Public
/// because both the search/match and highlight code use it.
pub fn visible_text(s: &str) -> String {
    let no_ansi = strip_ansi(s);
    let no_time = strip_time_prefix(&no_ansi).to_string();
    for prefix in [
        "##[group]",
        "##[section]",
        "##[endgroup]",
        "##[command]",
        "##[error]",
        "##[warning]",
        "##[debug]",
        "##[notice]",
    ] {
        if let Some(rest) = no_time.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    no_time
}

/// Flatten a rendered line back to plain text for width measurement.
fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// How many screen rows `text` occupies when word-wrapped to `width`.
///
/// Mirrors ratatui's `Wrap` closely enough for scroll math: greedy packing of
/// whitespace-separated words, with words longer than the line hard-split.
/// A plain `len / width` estimate is badly wrong for CI logs, where a single
/// `rustc` invocation is hundreds of long unbreakable tokens — and the error
/// accumulates across every line above the jump target.
pub fn wrapped_rows(text: &str, width: usize) -> u16 {
    if width == 0 {
        return 1;
    }
    let mut rows: usize = 1;
    let mut col: usize = 0;
    for word in text.split_inclusive(' ') {
        let w = word.chars().count();
        if w > width {
            // Too long to fit on any line: break to a fresh row, then hard-split.
            if col > 0 {
                rows += 1;
            }
            let extra = (w - 1) / width;
            rows += extra;
            col = w - extra * width;
        } else if col + w > width {
            rows += 1;
            col = w;
        } else {
            col += w;
        }
    }
    rows.min(u16::MAX as usize) as u16
}

/// Split log lines into (error indices, warning indices).
///
/// Uses the same two signals the renderer already colours on: explicit GitHub
/// Actions `##[error]` / `##[warning]` markup, and plain lines that *start* with
/// an error-ish word. Keeping the two in step means focus mode shows exactly the
/// lines that are painted red or yellow — no more, no less.
/// Lines a test harness prints when something fails, which the leading-keyword
/// rule misses entirely.
///
/// Without these, a failing `cargo test` step registers exactly one error — the
/// `error: test failed, to rerun pass …` line the tail — so jumping to "the
/// error" lands *below* the panic message and focus mode folds the assertion
/// away. The diagnosis is the part worth landing on.
fn is_test_failure(trimmed: &str) -> bool {
    // `test result: FAILED.`, `test some::case ... FAILED`, and pytest's
    // `FAILED tests/x.py::y` (already caught by the `failed` prefix rule).
    //
    // Matched case-insensitively at the end of the line for the `pre-commit`
    // framework, whose entire report is `ruff.................Failed` — the
    // verdict is the last word because the tool name is padded out to meet it.
    // That format shows up both in a local hook's output and in CI logs from
    // jobs that run `pre-commit run --all-files`.
    if trimmed.starts_with("test result: FAILED") {
        return true;
    }
    let verdict = trimmed.trim_end_matches(['.', ':', '!', ')']);
    if verdict
        .rsplit(['.', ' ', '\t'])
        .next()
        .is_some_and(|last| last.eq_ignore_ascii_case("failed"))
    {
        return true;
    }
    // Rust's panic header and the assertion line under it.
    if trimmed.contains("panicked at") {
        return true;
    }
    let lower = trimmed.to_lowercase();
    // `failures:` — the header over the list of what broke. Not caught by the
    // `failed` prefix: the word is "failures".
    lower.starts_with("failures:")
        || (lower.starts_with("assertion") && lower.contains("failed"))
}

pub fn classify_log_severity(lines: &[String]) -> (Vec<usize>, Vec<usize>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let content = split_time_prefix(line.as_str()).1;
        if content.starts_with("##[error]") {
            errors.push(i);
            continue;
        }
        if content.starts_with("##[warning]") {
            warnings.push(i);
            continue;
        }
        // Other `##[...]` markup (group/command/debug/notice) is structural, not
        // severity — skip it before the keyword check so `##[group]Error handling`
        // doesn't register as an error.
        if content.starts_with("##[") {
            continue;
        }
        let plain = if content.contains('\x1b') {
            strip_ansi(content)
        } else {
            content.to_string()
        };
        let trimmed = plain.trim();
        let lower = trimmed.to_lowercase();
        if lower.starts_with("error") || lower.starts_with("failed") || is_test_failure(trimmed) {
            errors.push(i);
        } else if lower.starts_with("warn") {
            warnings.push(i);
        }
    }
    (errors, warnings)
}

pub fn parse_log_groups(lines: &[String]) -> Vec<LogGroup> {
    let mut groups = Vec::new();
    let mut depth = 0usize;
    let mut current_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let content = split_time_prefix(line.as_str()).1;
        let is_group = content.starts_with("##[group]") || content.starts_with("##[section]");
        let is_endgroup = content.starts_with("##[endgroup]");
        if is_group {
            if depth == 0 {
                current_start = Some(i);
            }
            depth += 1;
        } else if is_endgroup {
            depth = depth.saturating_sub(1);
            if depth == 0
                && let Some(start) = current_start.take() {
                    groups.push(LogGroup { header_line: start, end_line: i });
                }
        }
    }
    if let Some(start) = current_start.take() {
        groups.push(LogGroup { header_line: start, end_line: lines.len().saturating_sub(1) });
    }
    groups
}

pub fn split_time_prefix(s: &str) -> (Option<&str>, &str) {
    if s.len() > 9
        && s.as_bytes().get(2) == Some(&b':')
        && s.as_bytes().get(5) == Some(&b':')
        && s.as_bytes().get(8) == Some(&b' ')
        && s[..2].bytes().all(|b| b.is_ascii_digit())
        && s[3..5].bytes().all(|b| b.is_ascii_digit())
        && s[6..8].bytes().all(|b| b.is_ascii_digit())
    {
        (Some(&s[..8]), &s[9..])
    } else {
        (None, s)
    }
}

pub fn ansi_line_to_spans(line: &str, default_style: Style) -> Vec<Span<'static>> {
    if !line.contains('\x1b') {
        return if line.is_empty() {
            vec![]
        } else {
            vec![Span::styled(line.to_string(), default_style)]
        };
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current = default_style;
    let chars: Vec<char> = line.chars().collect();
    let mut seg = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' && i + 1 < chars.len() && chars[i + 1] == '[' {
            let text: String = chars[seg..i].iter().collect();
            if !text.is_empty() {
                spans.push(Span::styled(text, current));
            }
            let seq_start = i + 2;
            let mut j = seq_start;
            while j < chars.len() && !chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j < chars.len() && chars[j] == 'm' {
                let params: String = chars[seq_start..j].iter().collect();
                current = apply_sgr(&params, current, default_style);
            }
            i = j + 1;
            seg = i;
        } else {
            i += 1;
        }
    }
    let tail: String = chars[seg..].iter().collect();
    if !tail.is_empty() {
        spans.push(Span::styled(tail, current));
    }
    spans
}

fn apply_sgr(params: &str, current: Style, default: Style) -> Style {
    if params.is_empty() {
        return default;
    }
    let nums: Vec<u32> = params.split(';').filter_map(|s| s.parse().ok()).collect();
    let mut s = current;
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => s = default,
            1 => s = s.add_modifier(Modifier::BOLD),
            2 => s = s.add_modifier(Modifier::DIM),
            3 => s = s.add_modifier(Modifier::ITALIC),
            4 => s = s.add_modifier(Modifier::UNDERLINED),
            22 => s = s.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => s = s.remove_modifier(Modifier::ITALIC),
            24 => s = s.remove_modifier(Modifier::UNDERLINED),
            30 => s = s.fg(Color::Black),
            31 => s = s.fg(Color::Red),
            32 => s = s.fg(Color::Green),
            33 => s = s.fg(Color::Yellow),
            34 => s = s.fg(Color::Blue),
            35 => s = s.fg(Color::Magenta),
            36 => s = s.fg(Color::Cyan),
            37 => s = s.fg(Color::Gray),
            38 if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() => {
                s = s.fg(Color::Rgb(
                    nums[i + 2] as u8,
                    nums[i + 3] as u8,
                    nums[i + 4] as u8,
                ));
                i += 4;
            }
            38 if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() => {
                s = s.fg(Color::Indexed(nums[i + 2] as u8));
                i += 2;
            }
            40 => s = s.bg(Color::Black),
            41 => s = s.bg(Color::Red),
            42 => s = s.bg(Color::Green),
            43 => s = s.bg(Color::Yellow),
            44 => s = s.bg(Color::Blue),
            45 => s = s.bg(Color::Magenta),
            46 => s = s.bg(Color::Cyan),
            47 => s = s.bg(Color::Gray),
            48 if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() => {
                s = s.bg(Color::Rgb(
                    nums[i + 2] as u8,
                    nums[i + 3] as u8,
                    nums[i + 4] as u8,
                ));
                i += 4;
            }
            48 if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() => {
                s = s.bg(Color::Indexed(nums[i + 2] as u8));
                i += 2;
            }
            90 => s = s.fg(Color::DarkGray),
            91 => s = s.fg(Color::LightRed),
            92 => s = s.fg(Color::LightGreen),
            93 => s = s.fg(Color::LightYellow),
            94 => s = s.fg(Color::LightBlue),
            95 => s = s.fg(Color::LightMagenta),
            96 => s = s.fg(Color::LightCyan),
            97 => s = s.fg(Color::White),
            _ => {}
        }
        i += 1;
    }
    s
}

pub fn highlight_line(line: Line<'static>, needle: &str, current: bool) -> Line<'static> {
    if needle.is_empty() {
        return line;
    }
    let hit_bg = if current {
        Color::Rgb(220, 200, 60)
    } else {
        Color::Rgb(120, 90, 30)
    };
    let hit_fg = Color::Black;

    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let text = span.content.into_owned();
        let style = span.style;
        let lower = text.to_lowercase();
        if !lower.contains(needle) {
            out.push(Span::styled(text, style));
            continue;
        }
        let bytes = text.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            match lower[cursor..].find(needle) {
                Some(rel) => {
                    let start = cursor + rel;
                    let end = start + needle.len();
                    if start > cursor {
                        out.push(Span::styled(text[cursor..start].to_string(), style));
                    }
                    out.push(Span::styled(
                        text[start..end].to_string(),
                        style.bg(hit_bg).fg(hit_fg).add_modifier(Modifier::BOLD),
                    ));
                    cursor = end;
                }
                None => {
                    out.push(Span::styled(text[cursor..].to_string(), style));
                    break;
                }
            }
        }
    }
    Line::from(out)
}

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
            i = if j < chars.len() { j + 1 } else { j };
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    fn section(label: &'static str, text: &str) -> crate::git::DiffSection {
        crate::git::DiffSection { label, text: text.to_string() }
    }

    fn op_with(raw: &[&str]) -> GitOp {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in raw {
            op.push_line((*l).to_string());
        }
        op
    }

    #[test]
    fn hook_output_is_classified_as_it_arrives() {
        let op = op_with(&[
            "ruff.....Passed",
            "FAILED tests/test_api.py::test_login",
            "warning: unused import",
        ]);
        assert!(!op.lines[0].error && !op.lines[0].warn);
        // The pytest line is the one worth colouring red and jumping to; it is
        // caught by the same rule the CI log viewer uses.
        assert!(op.lines[1].error);
        assert!(op.lines[2].warn);
        assert_eq!(op.error_count(), 1);
    }

    #[test]
    fn a_running_op_follows_the_tail_until_scrolled() {
        let mut op = op_with(&["one", "two", "three", "four"]);
        // No explicit scroll: show the newest lines, since that is where a
        // running hook is writing.
        assert_eq!(op.scroll_offset(2), 2);
        op.scroll_by(-1, 2);
        assert_eq!(op.scroll_offset(2), 1);
        // …and new output no longer drags the view along.
        op.push_line("five".into());
        assert_eq!(op.scroll_offset(2), 1);
        op.scroll_to_bottom();
        assert_eq!(op.scroll_offset(2), 3);
    }

    #[test]
    fn jump_error_walks_errors_and_wraps() {
        let mut op = op_with(&["a", "error: one", "b", "error: two", "c"]);
        op.scroll_to_top();
        assert!(op.jump_error(true, 2));
        assert_eq!(op.scroll, Some(1));
        assert!(op.jump_error(true, 2));
        assert_eq!(op.scroll, Some(3));
        // Past the last one, back to the first.
        assert!(op.jump_error(true, 2));
        assert_eq!(op.scroll, Some(1));
        assert!(op.jump_error(false, 2));
        assert_eq!(op.scroll, Some(3));
    }

    #[test]
    fn jump_error_reports_when_there_is_nothing_to_jump_to() {
        let mut op = op_with(&["all", "fine"]);
        assert!(!op.jump_error(true, 2));
    }

    #[test]
    fn dropping_old_lines_keeps_a_pinned_scroll_pointing_at_the_same_text() {
        let mut op = GitOp::new("commit", None, 0);
        for i in 0..MAX_OP_LINES {
            op.push_line(format!("line {i}"));
        }
        op.scroll = Some(10);
        op.push_line("overflow".into());
        assert_eq!(op.lines.len(), MAX_OP_LINES);
        assert_eq!(op.dropped, 1);
        // Line 10 shifted down to index 9, and the offset has to shift with it
        // or the pane silently scrolls itself while a hook is running.
        assert_eq!(op.scroll, Some(9));
        assert_eq!(op.lines[9].text, "line 10");
    }

    #[test]
    fn a_running_hook_survives_leaving_the_view_it_started_in() {
        let mut st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        let view = || GitView::new("acme/api".into(), PathBuf::from("/tmp/api"), true);
        st.git_ops
            .insert("acme/api".into(), GitOp::new("commit", Some("pre-commit".into()), 0));
        st.git_view = Some(view());
        assert!(st.current_op().is_some());

        // Esc out of the working tree while the hook is still going.
        st.git_view = None;
        assert!(st.current_op().is_none());
        // The command did not stop just because the screen changed — and
        // pressing `c` again on that repo must not start a second one.
        assert!(st.op_running("acme/api"));

        // Walking back in re-attaches the output rather than showing a blank.
        st.git_view = Some(view());
        assert!(st.current_op().is_some());

        // Another repo's working tree is not the place to show it.
        st.git_view = Some(GitView::new(
            "acme/web".into(),
            PathBuf::from("/tmp/web"),
            true,
        ));
        assert!(st.current_op().is_none());
        assert!(!st.op_running("acme/web"));
    }

    #[test]
    fn the_pane_is_named_after_the_hook_when_there_is_one() {
        assert_eq!(op_with(&[]).label(), "pre-commit hook");
        // No hook installed: don't invent one.
        assert_eq!(GitOp::new("push", None, 0).label(), "push");
    }

    #[test]
    fn elapsed_counts_whole_seconds_of_ticks() {
        let op = GitOp::new("commit", None, 100);
        assert_eq!(op.elapsed_secs(100), 0);
        assert_eq!(op.elapsed_secs(145), 4);
        // A tick count that went backwards must not panic.
        assert_eq!(op.elapsed_secs(50), 0);
    }

    #[test]
    fn a_single_diff_section_is_not_labelled() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section("unstaged", "@@ -1 +1 @@\n-a\n+b\n")]);
        assert!(!dv.loading);
        // With nothing to tell it apart from, a banner is a wasted row.
        assert!(!dv.lines.iter().any(|l| matches!(l, DiffLine::Section(_))));
        assert_eq!(dv.lines.len(), 3);
    }

    #[test]
    fn staged_and_unstaged_halves_are_kept_apart() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![
            section("staged", "+one\n"),
            section("unstaged", "+two\n"),
        ]);
        let banners: Vec<&String> = dv
            .lines
            .iter()
            .filter_map(|l| match l {
                DiffLine::Section(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(banners, vec!["staged", "unstaged"]);
        assert_eq!(dv.stats(), (2, 0));
    }

    #[test]
    fn stats_ignore_the_file_headers() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section(
            "unstaged",
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n more\n",
        )]);
        // `---`/`+++` name the files; counting them would add a phantom +1/-1
        // to every single file's summary.
        assert_eq!(dv.stats(), (1, 1));
    }

    #[test]
    fn scrolling_stops_at_the_last_line() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section(
            "unstaged",
            &(0..50).map(|i| format!("+{i}\n")).collect::<String>(),
        )]);
        dv.scroll_by(1000, 10);
        assert_eq!(dv.scroll, 40);
        dv.scroll_by(-1000, 10);
        assert_eq!(dv.scroll, 0);
        // A diff shorter than the viewport cannot scroll at all.
        assert_eq!(dv.max_scroll(100), 0);
    }

    #[test]
    fn reloading_resets_the_offset() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section("unstaged", "+a\n+b\n+c\n+d\n")]);
        dv.scroll = 3;
        // Stepping to another file must start at its top, not wherever the
        // previous file happened to be scrolled to.
        dv.set_sections(vec![section("unstaged", "+z\n")]);
        assert_eq!(dv.scroll, 0);
    }



    #[test]
    fn decorate_marks_only_the_cursor_row() {
        let mut st = state_with_rendered(20, "line", 10, 100);
        st.log_line_cursor = 3;
        let rows = st.decorate_visible(0, 6);
        assert_eq!(rows.len(), 6);
        let cursor_bg = Some(Color::Rgb(35, 42, 58));
        for (i, line) in rows.iter().enumerate() {
            assert_eq!(
                line.style.bg == cursor_bg,
                i == 3,
                "row {i} cursor highlight is wrong"
            );
        }
    }

    #[test]
    fn decorate_offsets_by_the_first_visible_row() {
        let mut st = state_with_rendered(20, "line", 10, 100);
        st.log_line_cursor = 12;
        // Viewport starts at 10, so the cursor is the third row drawn.
        let rows = st.decorate_visible(10, 5);
        let cursor_bg = Some(Color::Rgb(35, 42, 58));
        assert_eq!(rows[2].style.bg, cursor_bg);
        assert_ne!(rows[0].style.bg, cursor_bg);
    }

    #[test]
    fn decorate_clamps_past_the_end() {
        let st = state_with_rendered(5, "line", 10, 100);
        assert_eq!(st.decorate_visible(3, 40).len(), 2);
        assert!(st.decorate_visible(99, 10).is_empty());
    }

    #[test]
    fn moving_the_cursor_does_not_need_a_rebuild() {
        // The rendered buffer is content-only now, so it must be byte-identical
        // before and after a cursor move — that is what makes `j` cheap.
        let mut st = state_with_rendered(50, "line", 10, 100);
        st.log_line_cursor = 0;
        let before: Vec<String> = st.log_rendered.iter().map(|l| format!("{l:?}")).collect();
        st.log_line_cursor = 30;
        st.keep_cursor_visible();
        let after: Vec<String> = st.log_rendered.iter().map(|l| format!("{l:?}")).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn keep_cursor_visible_jumps_to_the_end_in_one_step() {
        let mut st = state_with_rendered(1000, "line", 10, 100);
        st.log_scroll = 0;
        st.log_line_cursor = 999;
        st.keep_cursor_visible();
        // 10 single-row lines fit, so the last screen starts at 990.
        assert_eq!(st.log_scroll, 990);
        assert!(st.last_visible_row(st.log_scroll as usize) >= 999);
    }

    #[test]
    fn parallel_render_preserves_line_order() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = (0..500).map(|i| format!("line {i}")).collect();
        st.last_logs_viewport_height.set(20);
        st.last_logs_viewport_width.set(200);
        st.init_log_groups();
        st.recompute_log_rendered();
        assert_eq!(st.log_rendered.len(), 500);
        assert_eq!(st.log_rendered_src, (0..500).collect::<Vec<_>>());
        for (i, line) in st.log_rendered.iter().enumerate() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(text, format!("line {i}"), "row {i} out of order");
        }
    }

    #[test]
    fn parallel_search_matches_stay_sorted() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = (0..1000)
            .map(|i| if i % 7 == 0 { format!("needle {i}") } else { format!("plain {i}") })
            .collect();
        st.log_search_query = Some("needle".into());
        st.recompute_log_matches();
        let expected: Vec<usize> = (0..1000).filter(|i| i % 7 == 0).collect();
        assert_eq!(st.log_search_matches, expected);
    }

    #[test]
    fn wrapped_rows_counts_word_wrap() {
        assert_eq!(wrapped_rows("short", 20), 1);
        assert_eq!(wrapped_rows("", 20), 1);
        // Exactly fills one row.
        assert_eq!(wrapped_rows("12345678901234567890", 20), 1);
        // Two words that don't fit together wrap to two rows.
        assert_eq!(wrapped_rows("aaaaaaaaaa bbbbbbbbbbbb", 20), 2);
        // A single unbreakable token longer than the line is hard-split.
        assert_eq!(wrapped_rows(&"x".repeat(45), 20), 3);
        // Width 0 is degenerate but must not divide by zero.
        assert_eq!(wrapped_rows("anything", 0), 1);
    }

    fn state_with_rendered(n: usize, text: &str, viewport: u16, width: u16) -> AppState {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = (0..n).map(|_| text.to_string()).collect();
        st.init_log_groups();
        st.last_logs_viewport_height.set(viewport);
        st.last_logs_viewport_width.set(width);
        st.recompute_log_rendered();
        st
    }

    #[test]
    fn max_log_scroll_leaves_a_full_viewport() {
        // 60 single-row lines, 36-row viewport -> last screen starts at line 24.
        let st = state_with_rendered(60, "short line", 36, 138);
        assert_eq!(st.log_rendered.len(), 60);
        assert_eq!(st.max_log_scroll(), 24);
    }

    #[test]
    fn max_log_scroll_is_zero_when_everything_fits() {
        let st = state_with_rendered(5, "short line", 36, 138);
        assert_eq!(st.max_log_scroll(), 0);
    }

    #[test]
    fn max_log_scroll_accounts_for_wrapping() {
        // Each line wraps to 2 rows, so a 36-row viewport holds 18 of them.
        let long = "w".repeat(60);
        let text: String = format!("{long} {long}");
        let st = state_with_rendered(60, &text, 36, 100);
        assert_eq!(st.visual_height_of_row(0), 2);
        assert_eq!(st.max_log_scroll(), 60 - 18);
    }

    #[test]
    fn last_visible_row_fills_but_does_not_overflow() {
        let st = state_with_rendered(60, "short line", 10, 138);
        assert_eq!(st.last_visible_row(0), 9);
        assert_eq!(st.last_visible_row(50), 59);
        // A line taller than the viewport is still reported as visible.
        let tall = state_with_rendered(3, &"x".repeat(2000), 5, 100);
        assert_eq!(tall.last_visible_row(0), 0);
    }

    #[test]
    fn center_cursor_puts_target_below_a_third_of_context() {
        let mut st = state_with_rendered(60, "short line", 36, 138);
        st.log_line_cursor = 40;
        st.center_cursor();
        // viewport/3 == 12 rows of context above the cursor.
        assert_eq!(st.log_scroll, 28);
        // Near the top there simply isn't that much context.
        st.log_line_cursor = 3;
        st.center_cursor();
        assert_eq!(st.log_scroll, 0);
    }

    #[test]
    fn keep_cursor_visible_scrolls_only_as_needed() {
        let mut st = state_with_rendered(60, "short line", 10, 138);
        st.log_scroll = 0;
        st.log_line_cursor = 5;
        st.keep_cursor_visible();
        assert_eq!(st.log_scroll, 0, "already on screen");
        st.log_line_cursor = 12;
        st.keep_cursor_visible();
        assert_eq!(st.log_scroll, 3, "scrolled just enough");
        st.log_line_cursor = 1;
        st.keep_cursor_visible();
        assert_eq!(st.log_scroll, 1, "scrolled back up");
    }


    // ── Performance probes ────────────────────────────────────────────────
    // Run with: cargo test --release perf_ -- --nocapture --test-threads=1

    fn big_log(n: usize) -> Vec<String> {
        // Shaped like a real CI log: groups, long unbreakable rustc lines,
        // ANSI colour, and a sprinkling of errors.
        let long = format!(
            "12:00:01 \x1b[32m     Running\x1b[0m `/home/runner/.rustup/toolchains/stable/bin/rustc --crate-name x {}`",
            "--extern dep=/very/long/path/to/lib-0123456789abcdef.rlib ".repeat(6)
        );
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            match i % 20 {
                0 => out.push(format!("12:00:0{} ##[group]step {}", i % 10, i)),
                19 => out.push("12:00:09 ##[endgroup]".to_string()),
                7 => out.push(long.clone()),
                13 if i % 200 == 13 => out.push(format!("12:00:03 ##[error]failure {i}")),
                _ => out.push(format!("12:00:0{} normal output line {}", i % 10, i)),
            }
        }
        out
    }

    fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
        let t = std::time::Instant::now();
        let r = f();
        println!("{label:38} {:>10.2?}", t.elapsed());
        r
    }

    fn perf_state(n: usize) -> AppState {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = big_log(n);
        st.last_logs_viewport_height.set(40);
        st.last_logs_viewport_width.set(140);
        st.init_log_groups();
        st.log_collapsed.clear(); // worst case: everything expanded
        st.recompute_log_rendered();
        st
    }

    #[test]
    #[ignore = "timing probe, not a pass/fail test; run with --ignored --nocapture"]
    fn perf_log_hot_paths() {
        const N: usize = 40_000;
        println!("\n--- {N} log lines, all groups expanded ---");
        let mut st = timed("build state (init + first render)", || perf_state(N));
        println!("rendered lines: {}", st.log_rendered.len());

        timed("recompute_log_rendered", || st.recompute_log_rendered());
        timed("compute_hidden_lines", || st.compute_hidden_lines());
        timed("max_log_scroll", || st.max_log_scroll());
        timed("visual_height_of_row x1000", || {
            for i in 0..1000 {
                std::hint::black_box(st.visual_height_of_row(i));
            }
        });

        // What one `j` keypress actually costs now: move, reclamp scroll, and
        // decorate the visible rows at draw time. No content rebuild.
        st.log_line_cursor = 20_000;
        st.log_scroll = 19_990;
        timed("ONE KEYPRESS (move+scroll+draw)", || {
            st.log_line_cursor += 1;
            st.keep_cursor_visible();
            std::hint::black_box(st.decorate_visible(st.log_scroll as usize, 40));
        });

        // Scrolling to the bottom, the worst case for keep_cursor_visible.
        st.log_scroll = 0;
        st.log_line_cursor = (st.log_rendered.len() - 1) as u16;
        timed("keep_cursor_visible (0 -> end)", || st.keep_cursor_visible());

        st.log_search_query = Some("failure".into());
        timed("recompute_log_matches", || st.recompute_log_matches());
        timed("rendered_row_for_src(last)", || {
            std::hint::black_box(st.rendered_row_for_src(N - 1))
        });
    }

    #[test]
    fn fuzzy_matches_subsequence_only() {
        assert!(fuzzy_score("deploy_to_prod.yml", "dtp").is_some());
        // Out-of-order characters are not a subsequence.
        assert!(fuzzy_score("deploy_to_prod.yml", "pdd").is_none());
        // A character that simply isn't there.
        assert!(fuzzy_score("deploy_to_prod.yml", "dq").is_none());
        assert!(fuzzy_score("deploy_to_prod.yml", "").is_some());
    }

    #[test]
    fn fuzzy_is_case_insensitive() {
        assert!(fuzzy_score("Deploy To Prod", "deploy").is_some());
        assert!(fuzzy_score("deploy", "DEPLOY").is_some());
    }

    #[test]
    fn fuzzy_prefers_word_boundaries() {
        // `dtp` hits three separator-led boundaries here...
        let boundary = fuzzy_score("deploy_to_prod.yml", "dtp").unwrap();
        // ...but is scattered mid-word here.
        let scattered = fuzzy_score("addendum-output-copy.yml", "dtp").unwrap();
        assert!(
            boundary > scattered,
            "boundary {boundary} should outrank scattered {scattered}"
        );
    }

    #[test]
    fn fuzzy_prefers_adjacent_runs() {
        let adjacent = fuzzy_score("cixyz.yml", "ci").unwrap();
        let split = fuzzy_score("czzzzi.yml", "ci").unwrap();
        assert!(adjacent > split, "adjacent {adjacent} vs split {split}");
    }

    #[test]
    fn finder_ranks_and_commits() {
        let items = vec![
            (0, "docker-test-pipeline.yml".to_string()),
            (1, "deploy_to_prod.yml".to_string()),
        ];
        let mut f = Finder::new(FinderKind::Workflows, items);
        assert_eq!(f.matches.len(), 2, "empty query matches everything");
        f.query = "dtp".into();
        f.recompute();
        // Both are subsequence matches; the boundary-heavy one must come first.
        assert_eq!(f.selected_target(), Some(1));
    }

    #[test]
    fn finder_drops_non_matches() {
        let items = vec![(0, "ci.yml".to_string()), (1, "release.yml".to_string())];
        let mut f = Finder::new(FinderKind::Workflows, items);
        f.query = "rel".into();
        f.recompute();
        assert_eq!(f.matches.len(), 1);
        assert_eq!(f.selected_target(), Some(1));
    }

    #[test]
    fn severity_uses_markup_and_keywords() {
        let ls = lines(&[
            "normal output",
            "##[error]boom",
            "ERROR: something broke",
            "##[warning]careful",
            "warning: deprecated",
            "##[group]Error handling setup",
            "   Error: leading whitespace is trimmed first",
        ]);
        let (errors, warnings) = classify_log_severity(&ls);
        // `##[group]Error handling` is structural markup, not an error.
        assert_eq!(errors, vec![1, 2, 6]);
        assert_eq!(warnings, vec![3, 4]);
    }

    #[test]
    fn severity_catches_a_failing_cargo_test_run() {
        // Verbatim from a real failing `cargo test` step. Only the last two
        // lines match the leading-keyword rule, which puts every jump target
        // below the panic — the one part that says what actually went wrong.
        let ls = lines(&[
            "test git::tests::discover_skips_nested_and_noise ... FAILED",
            "test tui::tests::notify_modes_parse ... ok",
            "failures:",
            "---- git::tests::discover_skips_nested_and_noise stdout ----",
            "thread 'git::tests::discover_skips_nested_and_noise' (6124) panicked at src\\git.rs:410:9:",
            "assertion `left == right` failed",
            "  left: [\"alpha\", \"group\\\\beta\"]",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
            "test result: FAILED. 75 passed; 1 failed; 1 ignored",
            "error: test failed, to rerun pass `--bin jog`",
        ]);
        let (errors, _) = classify_log_severity(&ls);
        assert_eq!(errors, vec![0, 2, 4, 5, 8, 9], "got {errors:?}");
        // The passing test above all of it stays quiet.
        assert!(!errors.contains(&1));
    }

    #[test]
    fn severity_reads_the_pre_commit_frameworks_verdict_column() {
        // What a `pre-commit` hook prints, locally and in CI. The verdict is at
        // the end of the line because the tool name is dot-padded out to it, so
        // no leading-keyword rule can see it.
        let ls = lines(&[
            "ruff.....................................................................Passed",
            "pyright..................................................................Failed",
            "- hook id: pyright",
            "- exit code: 1",
        ]);
        let (errors, _) = classify_log_severity(&ls);
        assert_eq!(errors, vec![1], "got {errors:?}");
    }

    #[test]
    fn severity_requires_leading_keyword() {
        // Mirrors the renderer, which only paints a line red when the keyword
        // leads. A mention mid-sentence is prose, not a failure.
        let ls = lines(&["step failed to error out", "the error was handled"]);
        let (errors, _) = classify_log_severity(&ls);
        assert!(errors.is_empty(), "got {errors:?}");
    }

    #[test]
    fn severity_ignores_mid_line_keywords() {
        let ls = lines(&["all good, no error here"]);
        let (errors, warnings) = classify_log_severity(&ls);
        assert!(errors.is_empty() && warnings.is_empty());
    }

    #[test]
    fn severity_sees_through_time_prefix_and_ansi() {
        let ls = lines(&["12:00:01 \x1b[31mERROR\x1b[0m: nope"]);
        let (errors, _) = classify_log_severity(&ls);
        assert_eq!(errors, vec![0]);
    }

    #[test]
    fn focus_keeps_errors_with_context() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = lines(&["a", "b", "c", "##[error]boom", "e", "f", "g"]);
        st.log_focus_context = 1;
        st.init_log_groups();
        st.log_focus = true;
        let hidden = st.compute_hidden_lines();
        // Error at 3 keeps 2..=4; everything else is hidden.
        assert_eq!(hidden, [0usize, 1, 5, 6].into_iter().collect());
    }

    /// A log with a single error buried in the middle of a lot of noise — the
    /// shape focus mode exists for.
    fn buried_error(before: usize, after: usize) -> AppState {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        let mut ls: Vec<String> = (0..before).map(|i| format!("noise {i}")).collect();
        ls.push("##[error]boom".into());
        ls.extend((0..after).map(|i| format!("tail {i}")));
        st.log_lines = ls;
        st.log_focus_context = 1;
        st.init_log_groups();
        st.log_focus = true;
        st.recompute_log_rendered();
        st
    }

    fn fold_counts(st: &AppState) -> Vec<usize> {
        let mut rows: Vec<(u16, usize)> = st.log_fold_rows.iter().map(|(&r, &n)| (r, n)).collect();
        rows.sort();
        rows.into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn focus_folds_report_what_they_swallowed() {
        let st = buried_error(100, 50);
        // Context 1 keeps lines 99..=101, so 99 lines fold above and 49 below.
        assert_eq!(fold_counts(&st), vec![99, 49]);
        // Three kept lines plus the two fold markers.
        assert_eq!(st.log_rendered.len(), 5);
        let first = line_text(&st.log_rendered[0]);
        assert!(first.contains("99 lines hidden"), "got {first:?}");
    }

    #[test]
    fn folds_appear_only_while_focused() {
        let mut st = buried_error(10, 10);
        st.log_focus = false;
        st.recompute_log_rendered();
        // Unfocused, nothing is elided, so a marker would be a lie.
        assert!(st.log_fold_rows.is_empty());
        assert_eq!(st.log_rendered.len(), 21);
    }

    #[test]
    fn opening_a_fold_reveals_exactly_that_run() {
        let mut st = buried_error(20, 20);
        // Row 0 is the fold above the error; row 2 (after fold + context) is
        // where the error sits, and the trailing fold is last.
        assert!(st.expand_fold_at(0));
        st.recompute_log_rendered();
        assert_eq!(fold_counts(&st), vec![19], "only the trailing fold is left");
        // The 19 lines above came back; the 19 below did not.
        assert_eq!(st.log_rendered.len(), 19 + 3 + 1);
    }

    #[test]
    fn a_fold_next_to_group_markup_still_opens() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        // `##[endgroup]` is dropped in every mode, so it must not become the
        // fold's anchor — anchored there, opening the fold would unhide nothing
        // and `↵` would look broken.
        let mut ls = vec!["##[group]build".to_string(), "##[error]boom".into()];
        ls.push("##[endgroup]".into());
        ls.extend((0..10).map(|i| format!("tail {i}")));
        st.log_lines = ls;
        st.log_focus_context = 1;
        st.init_log_groups();
        st.log_focus = true;
        st.recompute_log_rendered();

        let fold_row = *st.log_fold_rows.keys().next().expect("a fold exists");
        assert!(st.expand_fold_at(fold_row));
        st.recompute_log_rendered();
        assert!(st.log_fold_rows.is_empty(), "the fold opened");
    }

    #[test]
    fn a_normal_row_is_not_a_fold() {
        let mut st = buried_error(20, 20);
        // The error line itself: expanding it must not silently unhide a run.
        let err_row = st.rendered_row_for_src(20).unwrap();
        assert!(!st.expand_fold_at(err_row));
    }

    #[test]
    fn opened_folds_do_not_survive_a_new_log() {
        let mut st = buried_error(20, 20);
        st.expand_fold_at(0);
        st.log_lines = lines(&["##[error]other", "x"]);
        st.init_log_groups();
        // Fold anchors are source line numbers; against another step's log they
        // would unhide arbitrary lines.
        assert!(st.log_focus_expanded.is_empty());
        assert!(st.log_fold_rows.is_empty());
    }

    #[test]
    fn a_log_ending_in_noise_still_says_how_much() {
        // The trailing run has no following visible line to trigger the flush,
        // so without an explicit end-of-loop flush focus mode just stops.
        let st = buried_error(1, 40);
        assert_eq!(fold_counts(&st), vec![39]);
        let last = line_text(st.log_rendered.last().unwrap());
        assert!(last.contains("39 lines hidden"), "got {last:?}");
    }

    #[test]
    fn focus_ignores_collapsed_groups() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = lines(&["##[group]setup", "##[error]boom", "##[endgroup]"]);
        st.log_focus_context = 0;
        st.init_log_groups();
        assert!(!st.log_collapsed.is_empty(), "groups start collapsed");
        st.log_focus = true;
        // The error is inside a collapsed group but focus mode must still show it.
        assert!(!st.compute_hidden_lines().contains(&1));
    }

    #[test]
    fn rendered_src_maps_rows_back_to_lines() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        st.log_lines = lines(&["first", "##[error]boom", "third"]);
        st.init_log_groups();
        st.recompute_log_rendered();
        assert_eq!(st.log_rendered_src, vec![0, 1, 2]);
        assert_eq!(st.rendered_row_for_src(1), Some(1));
    }

    #[test]
    fn repo_card_counts_and_latest() {
        let mut card = RepoCard::new("o/r".into());
        assert_eq!(card.latest_status(), None);
        let mk = |id: u64, status: Status| Run {
            id,
            display_title: "ci".into(),
            head_branch: "main".into(),
            commit_msg: String::new(),
            status,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            url: String::new(),
            workflow_file: None,
        };
        card.runs = vec![
            mk(3, Status::Running),
            mk(2, Status::Failure),
            mk(1, Status::Success),
        ];
        assert_eq!(card.latest_status(), Some(Status::Running));
        assert_eq!(card.counts(), (1, 1, 1));
    }
}
