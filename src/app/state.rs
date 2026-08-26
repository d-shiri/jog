use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::style::{Color, Modifier, Style, Stylize};

use rayon::prelude::*;

use crate::config::KeymapConfig;
use crate::git::RepoStatus;
use crate::history::History;
use crate::provider::github::{ApiError, Quota};
use crate::provider::{Job, PrInfo, Run, RunDetail, Status, Workflow};


#[derive(Debug, Clone, Copy)]
pub enum DetailItem {
    Job(usize),
    Step { job: usize, step: usize },
}

/// What a mouse click at some screen position would land on.
///
/// Each list view registers one of these per *visible* row while rendering, so
/// the click handler never re-derives table offsets — it asks the frame that
/// was actually drawn. The index is into the view's own list (repos, runs, …),
/// not into table rows, so headings and scrolling are already accounted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Repo(usize),
    Workflow(usize),
    Run(usize),
    DetailItem(usize),
    GitEntry(usize),
}

/// The part of a failed run's log worth reading before opening the log at all:
/// a window around the first error of the first failed step. Fetched once per
/// failed run when its detail view opens, and kept until another run's digest
/// replaces it.
#[derive(Debug, Clone)]
pub struct FailureDigest {
    /// Run this digest belongs to — the detail view shows it only while it is
    /// looking at the same run.
    pub run_id: u64,
    pub job_name: String,
    /// The failed step, when the job's step list named one.
    pub step_name: Option<String>,
    /// The extracted window, time prefixes stripped.
    pub lines: Vec<String>,
    /// Indices into `lines` that classified as errors, for highlighting.
    pub error_rows: Vec<usize>,
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
    /// One message committed across several repos, one repo at a time.
    BatchCommit,
}

/// GitHub's mark, as a Nerd Font glyph. Overridable, because a terminal without
/// a patched font draws it as a box — see `ui.github_icon`.
pub const DEFAULT_FORGE_ICON: &str = "\u{f09b}";

/// The header's bell — notifications are live — and its slashed twin for a
/// snooze. Nerd Font (Font Awesome bell / bell-slash), same tradeoff and
/// override story as the forge mark: see `ui.bell_icon` / `ui.bell_off_icon`.
pub const DEFAULT_BELL_ICON: &str = "\u{f0f3}";
pub const DEFAULT_BELL_OFF_ICON: &str = "\u{f1f6}";

/// The question a landed commit raises: push it?
///
/// A commit nobody pushed is invisible to CI and to everyone else, so the answer
/// is almost always yes — which is why it should cost one keystroke you are
/// already reaching for rather than one you have to remember.
#[derive(Debug, Clone)]
pub struct PushPrompt {
    /// Repo the commit landed in.
    pub spec: String,
    /// Branch the push would go to, captured when the commit landed.
    ///
    /// The prompt carries this rather than re-reading the working tree because
    /// the status refresh a commit triggers is still in flight when the question
    /// appears: asking then would find the pre-commit ahead count and conclude
    /// there is nothing to push, one keystroke after there demonstrably is.
    pub branch: String,
    /// False when this branch has never been pushed — the push would create it,
    /// which is worth saying out loud before it happens.
    pub has_upstream: bool,
    /// The highlighted answer. Starts on yes; Enter takes it.
    pub yes: bool,
    /// `Some(n)` when the question is a finished batch's — "push all n?" —
    /// rather than one branch's. `spec`/`branch` are unused then: the batch
    /// itself knows which repos are ready to push.
    pub batch_count: Option<usize>,
}

impl PushPrompt {
    /// The question a finished batch raises: one dialog for all its commits,
    /// the same shape as the single-repo one, so the answer costs the same
    /// keystroke everywhere.
    pub fn for_batch(count: usize) -> Self {
        Self {
            spec: String::new(),
            branch: String::new(),
            has_upstream: true,
            yes: true,
            batch_count: Some(count),
        }
    }
}

/// A push being followed into CI: first watching for the run it spawns, then
/// for that run to settle.
///
/// This exists because polling is per-view: push from the working tree and walk
/// off to read logs, and nothing would otherwise fetch that repo again — the
/// run your push started could fail unannounced. The watch polls on its own,
/// whatever is on screen, and ends itself when the run lands (or none appears).
#[derive(Debug, Clone)]
pub struct PushWatch {
    /// Repo the push went to (a `RepoCard` key).
    pub spec: String,
    /// Branch that was pushed, when it was knowable at push time. `None` makes
    /// the match fall back to "any run created after the push".
    pub branch: Option<String>,
    /// When the push landed — runs created before it are not its offspring.
    pub pushed_at: chrono::DateTime<chrono::Utc>,
    /// Tick the watch started on, for giving up when no run ever appears
    /// (plenty of pushes trigger no workflow at all).
    pub started_tick: u64,
    /// The run the push spawned, once one has been spotted.
    pub run: Option<Run>,
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
    /// The open PR riding the branch, and which branch that answer was fetched
    /// for. `None` until asked; the branch is kept so switching branches (or a
    /// push, which clears this) asks again while ordinary status refreshes
    /// don't.
    pub pr: Option<(String, Option<PrInfo>)>,
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
            pr: None,
        }
    }

    /// The PR shown to the user, if the branch has one.
    pub fn open_pr(&self) -> Option<&PrInfo> {
        self.pr.as_ref().and_then(|(_, p)| p.as_ref())
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
    /// The last line is unterminated — `pre-commit` printing `pytest....` with
    /// the verdict still to come. The next pushed line replaces it rather than
    /// stacking under it, the way a terminal would overwrite the same row.
    last_partial: bool,
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
            last_partial: false,
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

    pub fn push_line(&mut self, text: String, partial: bool) {
        let (errors, warns) = classify_log_severity(std::slice::from_ref(&text));
        let line = OpLine {
            text,
            error: !errors.is_empty(),
            warn: !warns.is_empty(),
        };
        // A partial line's successor is the same line — grown, or finished.
        // Replacing keeps `pytest....` and `pytest....Passed` from both
        // appearing, one above the other.
        if self.last_partial
            && let Some(last) = self.lines.last_mut()
        {
            *last = line;
        } else {
            self.lines.push(line);
        }
        self.last_partial = partial;
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

/// One side of a side-by-side diff row: the text of the line with its marker
/// column dropped, the number it has in its own file, and where inside it the
/// edit actually happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSide {
    pub text: String,
    /// The line's number in its own version of the file — `None` only when the
    /// hunk header it belongs to could not be parsed.
    pub num: Option<usize>,
    /// Added or removed, as opposed to context carried by both sides.
    pub changed: bool,
    /// Byte range of the changed span within `text`.
    pub emph: Option<ByteSpan>,
}

/// One row of the side-by-side diff.
///
/// Rows, not lines: a `-`/`+` pair is one row with two sides, which is the
/// whole point of the layout — the old and the new sit at the same height and
/// the eye compares across rather than remembering down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    /// A staged/unstaged banner.
    Section(String),
    /// A line belonging to neither side: a hunk header, a file header, a
    /// `Binary files …` note, the blank between sections.
    Meta(String),
    /// A side each. `None` is the gap opposite a line the other side added or
    /// removed — there is nothing there, and the layout says so.
    Pair {
        old: Option<DiffSide>,
        new: Option<DiffSide>,
    },
}

/// A tab, in columns. Expanded here rather than left to the terminal: in a
/// two-column layout a raw tab jumps to the terminal's own stop, which is
/// measured from the edge of the screen and lands wherever it likes inside a
/// padded cell — one tab and the right-hand column stops lining up.
const TAB_WIDTH: usize = 4;

/// Where `i` lands once the tabs before it have been expanded.
fn expanded_at(s: &str, i: usize) -> usize {
    let Some(head) = s.get(..i) else { return i };
    head.chars()
        .map(|c| if c == '\t' { TAB_WIDTH } else { c.len_utf8() })
        .sum()
}

fn expand_tabs(s: &str) -> String {
    if s.contains('\t') {
        s.replace('\t', &" ".repeat(TAB_WIDTH))
    } else {
        s.to_string()
    }
}

/// One unified line as a side of a row: marker column dropped, tabs expanded,
/// and the emphasis span moved to match both.
fn diff_side(raw: &str, emph: Option<ByteSpan>, num: usize, changed: bool) -> DiffSide {
    let body = raw.get(1..).unwrap_or("");
    let emph = emph
        // Spans are measured against the line including its marker byte.
        .map(|(a, b)| (a.saturating_sub(1), b.saturating_sub(1)))
        .filter(|(a, b)| *b <= body.len() && a < b)
        .map(|(a, b)| (expanded_at(body, a), expanded_at(body, b)));
    DiffSide {
        text: expand_tabs(body),
        num: Some(num),
        changed,
        emph,
    }
}

/// `@@ -12,7 +30,9 @@` → the first line number on each side.
fn hunk_starts(t: &str) -> Option<(usize, usize)> {
    let rest = t.strip_prefix("@@ ")?;
    let mut parts = rest.split_whitespace();
    let (old, new) = (parts.next()?, parts.next()?);
    let first = |s: &str, mark: char| {
        s.strip_prefix(mark)?
            .split(',')
            .next()?
            .parse::<usize>()
            .ok()
    };
    Some((first(old, '-')?, first(new, '+')?))
}

/// Fold a unified diff into rows with a side each.
///
/// A run of removals followed by a run of additions is zipped position by
/// position — the same pairing the emphasis uses, extended to unequal runs,
/// where the longer side simply runs on opposite gaps. Anything else keeps a
/// row to itself: context appears on both sides, everything else on neither.
pub fn diff_rows(lines: &[DiffLine], emphasis: &[Option<ByteSpan>]) -> Vec<DiffRow> {
    let text_at = |i: usize| match &lines[i] {
        DiffLine::Text(t) => Some(t.as_str()),
        DiffLine::Section(_) => None,
    };
    let marked = |i: usize, mark: char, header: &str| {
        text_at(i).is_some_and(|t| t.starts_with(mark) && !t.starts_with(header))
    };
    let emph_at = |i: usize| emphasis.get(i).copied().flatten();

    let mut rows: Vec<DiffRow> = Vec::with_capacity(lines.len());
    let (mut old_no, mut new_no) = (0usize, 0usize);
    let mut i = 0;
    while i < lines.len() {
        if let DiffLine::Section(label) = &lines[i] {
            rows.push(DiffRow::Section(label.clone()));
            i += 1;
            continue;
        }
        if marked(i, '-', "---") || marked(i, '+', "+++") {
            let del = i;
            while i < lines.len() && marked(i, '-', "---") {
                i += 1;
            }
            let add = i;
            while i < lines.len() && marked(i, '+', "+++") {
                i += 1;
            }
            let (dn, an) = (add - del, i - add);
            for k in 0..dn.max(an) {
                let old = (k < dn).then(|| {
                    diff_side(text_at(del + k).unwrap_or(""), emph_at(del + k), old_no + k + 1, true)
                });
                let new = (k < an).then(|| {
                    diff_side(text_at(add + k).unwrap_or(""), emph_at(add + k), new_no + k + 1, true)
                });
                rows.push(DiffRow::Pair { old, new });
            }
            old_no += dn;
            new_no += an;
            continue;
        }
        let t = text_at(i).unwrap_or("");
        if let Some((o, n)) = hunk_starts(t) {
            // The header numbers the line *after* it, so the counters sit one
            // behind and are stepped as each line is taken.
            old_no = o.saturating_sub(1);
            new_no = n.saturating_sub(1);
            rows.push(DiffRow::Meta(t.to_string()));
        } else if t.starts_with(' ') {
            old_no += 1;
            new_no += 1;
            rows.push(DiffRow::Pair {
                old: Some(diff_side(t, None, old_no, false)),
                new: Some(diff_side(t, None, new_no, false)),
            });
        } else {
            rows.push(DiffRow::Meta(t.to_string()));
        }
        i += 1;
    }
    rows
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
    /// Per-line byte range of the changed span inside a paired `-`/`+` line,
    /// parallel to `lines`. Computed once here rather than per frame, for the
    /// same reason the render slices to the viewport: the pairing has to look
    /// at the whole diff, and a redraw should not.
    pub emphasis: Vec<Option<ByteSpan>>,
    /// The same diff folded into side-by-side rows. Built once here, with the
    /// emphasis and for the same reason: the pairing has to look at whole runs
    /// of the diff, and a redraw must not.
    pub rows: Vec<DiffRow>,
    pub scroll: usize,
    /// Scrollable units the last frame drew — rows where the terminal was wide
    /// enough for two columns, lines where it was not. `Cell` because the
    /// render is what knows which layout the width allowed, and the key
    /// handler has to stop at the same bottom the eye can see.
    pub units: Cell<usize>,
    pub loading: bool,
}

impl GitDiffView {
    pub fn new(spec: String, file: String) -> Self {
        Self {
            spec,
            file,
            lines: Vec::new(),
            emphasis: Vec::new(),
            rows: Vec::new(),
            scroll: 0,
            units: Cell::new(0),
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
        self.emphasis = diff_emphasis(&self.lines);
        self.rows = diff_rows(&self.lines, &self.emphasis);
        self.units.set(0);
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
        // Whatever the last frame counted; before the first one, the unified
        // line count is the only answer there is.
        let total = match self.units.get() {
            0 => self.lines.len(),
            n => n,
        };
        total.saturating_sub(viewport.max(1))
    }

    pub fn scroll_by(&mut self, delta: isize, viewport: usize) {
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, self.max_scroll(viewport) as isize) as usize;
    }
}

/// Byte range of the changed span within one diff line.
pub type ByteSpan = (usize, usize);

/// The changed span inside each diff line, as a byte range, for lines with
/// something finer than the line itself to say.
///
/// Only balanced runs — N `-` lines followed directly by N `+` lines — are
/// paired, the i-th with the i-th. Positional pairing on unequal runs is a
/// guess, and a wrong guess marks two unrelated lines as an edit of each
/// other. The `---`/`+++` file headers are never content, so they neither
/// join nor split a run they sit next to — in practice they only ever appear
/// outside hunks anyway.
fn diff_emphasis(lines: &[DiffLine]) -> Vec<Option<ByteSpan>> {
    let mut out = vec![None; lines.len()];
    let is = |i: usize, mark: char, header: &str| {
        matches!(&lines[i], DiffLine::Text(t) if t.starts_with(mark) && !t.starts_with(header))
    };
    let mut i = 0;
    while i < lines.len() {
        if !is(i, '-', "---") {
            i += 1;
            continue;
        }
        let del = i;
        while i < lines.len() && is(i, '-', "---") {
            i += 1;
        }
        let add = i;
        while i < lines.len() && is(i, '+', "+++") {
            i += 1;
        }
        if i - add != add - del {
            continue;
        }
        for k in 0..(add - del) {
            let (DiffLine::Text(old), DiffLine::Text(new)) = (&lines[del + k], &lines[add + k])
            else {
                continue;
            };
            // `+ 1` puts the range back past the `-`/`+` marker byte the
            // comparison skipped.
            if let Some((o, n)) = changed_spans(&old[1..], &new[1..]) {
                out[del + k] = o.map(|(s, e)| (s + 1, e + 1));
                out[add + k] = n.map(|(s, e)| (s + 1, e + 1));
            }
        }
    }
    out
}

/// Each side's differing byte span once the common prefix and suffix are
/// peeled off.
///
/// A side's span is `None` when it is empty — the other line grew or shrank at
/// that point, and there is nothing on this side to mark. The whole answer is
/// `None` when the lines share no edge at all: those are different lines, not
/// an edit of one, and marking all of both would only restate the line colour.
fn changed_spans(old: &str, new: &str) -> Option<(Option<ByteSpan>, Option<ByteSpan>)> {
    let prefix: usize = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    // Over the remainders, so the suffix can never reclaim prefix bytes.
    let suffix: usize = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    if prefix == 0 && suffix == 0 {
        return None;
    }
    let span = |len: usize| (prefix < len - suffix).then_some((prefix, len - suffix));
    Some((span(old.len()), span(new.len())))
}

/// One row of the multi-repo dashboard: a repo plus its most recent runs.
/// Where one repo of a batch commit has got to.
///
/// A single enum rather than a pile of flags: every repo is in exactly one of
/// these, which is what lets the summary be read straight off the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    /// Not attempted yet. Still says this after an abort — "not attempted" is a
    /// truthful thing to report, and "skipped" would not be.
    Queued,
    Running,
    Committed,
    Pushed,
    /// Nothing here for the batch to do, and why.
    Nothing(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct BatchItem {
    /// Key of the `RepoCard`, and of any `GitOp` this repo's work produces.
    pub spec: String,
    pub path: PathBuf,
    pub state: ItemState,
    /// Set once this repo's commit lands. Kept apart from `state` so a failed
    /// *push* doesn't erase the fact that the commit is sitting on disk — the
    /// difference between "retry the push" and "retry the commit".
    pub sha: Option<String>,
}

impl BatchItem {
    pub fn new(spec: String, path: PathBuf) -> Self {
        Self { spec, path, state: ItemState::Queued, sha: None }
    }

    /// Has a commit from this batch that hasn't been pushed yet.
    pub fn ready_to_push(&self) -> bool {
        self.state == ItemState::Committed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPhase {
    /// Typing the one message that every repo will get.
    Compose,
    Committing,
    /// A repo failed. Nothing else starts until the user says what to do.
    Paused,
    /// Every commit is in. Pushing is a separate decision, deliberately.
    AskPush,
    Pushing,
    Done,
}

/// One commit message applied across several repos, one repo at a time.
///
/// Sequential rather than parallel on purpose: hooks produce a lot of output,
/// and four repos failing at once in four panes is not something anyone reads.
/// One at a time means a failure is on screen, alone, when it happens.
#[derive(Debug, Clone)]
pub struct BatchCommit {
    pub message: String,
    /// `Some` while the message is being typed; `None` once the run starts.
    pub input: Option<String>,
    /// Frozen at start, in dashboard order.
    pub items: Vec<BatchItem>,
    pub cursor: usize,
    pub phase: BatchPhase,
    /// The phase a pause interrupted, so retry and skip know what to go back to.
    pub resume: BatchPhase,
    /// Tick the current repo started on, for its elapsed clock.
    pub started_tick: u64,
}

/// What the batch has actually done, for the summary line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchTally {
    pub committed: usize,
    pub pushed: usize,
    pub failed: usize,
    pub nothing: usize,
    pub untouched: usize,
}

impl BatchCommit {
    pub fn new(items: Vec<BatchItem>, tick: u64) -> Self {
        Self {
            message: String::new(),
            input: Some(String::new()),
            items,
            cursor: 0,
            phase: BatchPhase::Compose,
            resume: BatchPhase::Committing,
            started_tick: tick,
        }
    }

    pub fn current(&self) -> Option<&BatchItem> {
        self.items.get(self.cursor)
    }

    /// Whether a git command the batch started is still out there.
    pub fn is_working(&self) -> bool {
        matches!(self.phase, BatchPhase::Committing | BatchPhase::Pushing)
    }

    /// Move to the next repo with work left in this phase, marking it running.
    ///
    /// Returns the index to start, or `None` when the phase is over — in which
    /// case `phase` has already moved on, so the caller only has to look.
    pub fn advance(&mut self, tick: u64) -> Option<usize> {
        // Only a phase that is *running* repos has a queue to advance. Asked in
        // any other phase the answer is "nothing to start" — and, crucially, the
        // phase stays put: winding a paused batch up to Done here would throw
        // away the retry/skip decision the pause exists to wait for.
        if !self.is_working() {
            return None;
        }
        let next = match self.phase {
            BatchPhase::Committing => {
                self.items.iter().position(|i| i.state == ItemState::Queued)
            }
            BatchPhase::Pushing => self.items.iter().position(|i| i.ready_to_push()),
            _ => None,
        };
        match next {
            Some(i) => {
                self.resume = self.phase;
                self.cursor = i;
                self.items[i].state = ItemState::Running;
                self.started_tick = tick;
                Some(i)
            }
            None => {
                // Committing ends at the push question — but only if there is
                // something to push. Asking about nothing is just a keystroke.
                self.phase = if self.phase == BatchPhase::Committing
                    && self.items.iter().any(|i| i.ready_to_push())
                {
                    BatchPhase::AskPush
                } else {
                    BatchPhase::Done
                };
                None
            }
        }
    }

    /// Fold in the result of whichever repo was running.
    ///
    /// A failure pauses the whole batch: the next repo starting would scroll the
    /// output that explains the failure off the screen.
    ///
    /// The outcome is recorded even after an abort — the repo's work happened,
    /// and a summary that omits it would be wrong about what is on disk. Only
    /// the *pause* is conditional, since there is nothing left to pause.
    pub fn record(&mut self, spec: &str, result: Result<String, String>) {
        let (working, pushing) = (self.is_working(), self.resume == BatchPhase::Pushing);
        let Some(item) = self.items.iter_mut().find(|i| i.spec == spec) else {
            return;
        };
        match result {
            Ok(sha) => {
                if pushing {
                    item.state = ItemState::Pushed;
                } else {
                    item.state = ItemState::Committed;
                    item.sha = Some(sha);
                }
            }
            Err(error) => {
                item.state = ItemState::Failed(error);
                if working {
                    self.phase = BatchPhase::Paused;
                }
            }
        }
    }

    /// Nothing to do here — a clean tree, or a branch already up to date.
    ///
    /// The state moves to `Nothing` even when a commit landed earlier (a push
    /// that found a detached HEAD, say), because the item must stop being
    /// `ready_to_push` or the push queue would hand it back forever. `sha`
    /// survives, and both the tally and the row read it, so the commit sitting
    /// on disk is still reported.
    pub fn record_nothing(&mut self, spec: &str, why: String) {
        if let Some(item) = self.items.iter_mut().find(|i| i.spec == spec) {
            item.state = ItemState::Nothing(why);
        }
    }

    /// Put the paused repo back in the queue and resume.
    ///
    /// Back to *committing* only if the commit is what failed; a repo whose
    /// commit landed and whose push failed goes back to the push queue, because
    /// re-committing it would find nothing to commit.
    pub fn retry(&mut self) {
        if let Some(item) = self.items.get_mut(self.cursor)
            && matches!(item.state, ItemState::Failed(_))
        {
            item.state = if item.sha.is_some() {
                ItemState::Committed
            } else {
                ItemState::Queued
            };
        }
        self.phase = self.resume;
    }

    /// Leave the failure on the record and carry on with the rest.
    pub fn skip(&mut self) {
        self.phase = self.resume;
    }

    /// Start no further repos. Whatever already committed stays committed —
    /// there is no honest way to undo a hook that has already run.
    pub fn abort(&mut self) {
        self.phase = BatchPhase::Done;
    }

    pub fn tally(&self) -> BatchTally {
        let mut t = BatchTally::default();
        for i in &self.items {
            match i.state {
                ItemState::Committed => t.committed += 1,
                ItemState::Pushed => {
                    t.committed += 1;
                    t.pushed += 1;
                }
                ItemState::Failed(_) => t.failed += 1,
                // A repo whose *push* had nothing to do still has the commit
                // this batch made, and is counted for it. Counting it under
                // "nothing" as well would inflate the summary past the number of
                // repos in the run — the row already says why it wasn't pushed.
                ItemState::Nothing(_) if i.sha.is_some() => t.committed += 1,
                ItemState::Nothing(_) => t.nothing += 1,
                ItemState::Queued | ItemState::Running => t.untouched += 1,
            }
        }
        t
    }
}

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
    pub error: Option<ApiError>,
    pub loaded: bool,
    /// Working-tree state, refreshed alongside the run list. Only ever `Some`
    /// for rows with a local checkout.
    pub git: Option<RepoStatus>,
    /// Tick this row's CI last changed under us, if it has.
    ///
    /// Drives the flash that draws the eye back to a row that moved while you
    /// were reading a different one. Poll intervals are measured in seconds, so
    /// without it the only evidence that anything happened is a glyph that is
    /// now a different shape than it was the last time you looked at it.
    pub changed_tick: Option<u64>,
    /// Tick a run on this row *landed* — running one poll, terminal the next —
    /// and how it ended.
    ///
    /// Separate from `changed_tick` because a landing is the change worth
    /// more: the row breathes in the run's verdict colour for a couple of
    /// seconds, long enough to catch from the corner of an eye with the sound
    /// off, where the generic flash is one second and gone.
    pub settled_tick: Option<(u64, Status)>,
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
            changed_tick: None,
            settled_tick: None,
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
            changed_tick: None,
            settled_tick: None,
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

    /// Every run that hasn't settled yet — what the activity strip follows.
    ///
    /// All of them, not just the newest: one push commonly starts several
    /// workflows (CI, a deploy, a review bot), and reporting only the first
    /// would have the strip say "one run" while the repo's Actions tab says
    /// three.
    pub fn active_runs(&self) -> impl Iterator<Item = &Run> {
        self.runs
            .iter()
            .filter(|r| matches!(r.status, Status::Running | Status::Queued))
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
    /// The value came from the last dispatch rather than the YAML default —
    /// worth a marker, because a prefilled value you did not notice is how a
    /// deploy goes to yesterday's target.
    pub recalled: bool,
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
    /// `recall` is what the user dispatched this workflow with last time; where
    /// present (and still a legal choice) it wins over the YAML default. The
    /// default is what the workflow author guessed; the recall is what this
    /// user actually wanted, demonstrated once already.
    pub fn from_workflow(
        workflow: &Workflow,
        return_view: View,
        recall: Option<&HashMap<String, String>>,
    ) -> Self {
        let fields = workflow
            .inputs
            .iter()
            .map(|i| {
                let default = i.default.clone().unwrap_or_default();
                let recalled = recall
                    .and_then(|m| m.get(&i.name))
                    .filter(|v| !v.is_empty())
                    .filter(|v| {
                        i.options
                            .as_ref()
                            .is_none_or(|opts| opts.iter().any(|o| o == *v))
                    })
                    .filter(|v| **v != default)
                    .cloned();
                TriggerField {
                    name: i.name.clone(),
                    recalled: recalled.is_some(),
                    value: recalled.unwrap_or(default),
                    required: i.required,
                    options: i.options.clone(),
                }
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

/// Every colour the UI draws with, named by the job it does rather than by its
/// hue.
///
/// Roles, not values: a view asks for `text_muted` instead of picking a grey.
/// The same idea used to be spelled four slightly different ways across one
/// file — `110,110,140` beside `120,120,145` beside `95,95,120` — and that drift
/// is most of what makes a screen look assembled rather than designed. It also
/// means a palette can be swapped whole, which is what `[ui] theme` does.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // ── Surfaces, back to front ──────────────────────────────────────────
    /// Chrome behind the header and footer.
    pub surface: Color,
    /// A row or pane lifted off the background: cursor lines, previews.
    pub surface_alt: Color,
    /// Behind a modal. Darker than `surface`, so an overlay reads as being in
    /// front of the screen rather than part of it.
    pub overlay: Color,

    // ── Structure ────────────────────────────────────────────────────────
    /// Panel borders and separators.
    pub border: Color,
    /// Rules that should be sensed rather than read.
    pub border_dim: Color,

    // ── Text, brightest to faintest ──────────────────────────────────────
    /// The one thing on the row you are meant to read first.
    pub text_bright: Color,
    /// Body text.
    pub text: Color,
    /// Labels, units, secondary facts.
    pub text_muted: Color,
    /// Present but not competing: stale timestamps, hints.
    pub text_faint: Color,
    /// Barely there: disabled steps, structural punctuation.
    pub text_ghost: Color,

    // ── Selection ────────────────────────────────────────────────────────
    pub select_bg: Color,
    /// The selection when the pane does not have the keyboard.
    pub select_bg_dim: Color,

    // ── Meaning ──────────────────────────────────────────────────────────
    pub primary: Color,
    pub accent: Color,
    /// Accent at rest — a branch name, a divergence marker.
    pub accent_dim: Color,
    pub info: Color,
    pub success: Color,
    /// A success already absorbed: a step that passed, a clean tree.
    pub success_dim: Color,
    pub failure: Color,
    pub failure_dim: Color,
    pub warning: Color,
    pub unknown: Color,

    // ── Dashboard row tints ──────────────────────────────────────────────
    ///
    /// Deliberately close to `surface`: the row's *glyph* carries the status,
    /// and a row bright enough to read as a warning on its own would make eight
    /// repos look like eight alarms.
    pub row_failure: Color,
    pub row_running: Color,
    pub row_queued: Color,
    pub row_idle: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::midnight()
    }
}

/// `#rrggbb` or `rrggbb`, case-insensitive.
fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Nearest xterm-256 colour: the 6×6×6 cube, or the 24-step grey ramp when the
/// channels are close enough that the cube would tint a grey.
fn rgb_to_xterm256(r: u8, g: u8, b: u8) -> u8 {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    if max - min < 12 {
        let level = ((r as u16 + g as u16 + b as u16) / 3) as u8;
        // The ramp runs 8, 18, … 238 at indices 232..=255, with the cube's own
        // black and white beyond either end of it.
        return match level {
            0..=4 => 16,
            245..=255 => 231,
            _ => 232 + ((level as u16 - 8) * 23 / 230).min(23) as u8,
        };
    }
    let axis = |v: u8| -> u16 {
        // Cube steps are 0, 95, 135, 175, 215, 255 — not evenly spaced, so the
        // boundaries are the midpoints between them rather than v/51.
        match v {
            0..=47 => 0,
            48..=114 => 1,
            115..=154 => 2,
            155..=194 => 3,
            195..=234 => 4,
            _ => 5,
        }
    };
    (16 + 36 * axis(r) + 6 * axis(g) + axis(b)) as u8
}

/// Whether this terminal can be trusted with 24-bit colour.
///
/// Conservative: only an explicit `COLORTERM` claim counts. Guessing wrong
/// upwards leaves the palette rounded by the terminal into mud, which is the
/// exact failure `degrade_to_256` exists to avoid.
pub fn terminal_has_truecolor() -> bool {
    matches!(
        std::env::var("COLORTERM").as_deref(),
        Ok("truecolor") | Ok("24bit")
    )
}

impl Theme {
    /// Look one up by name, for `[ui] theme` in config.
    pub fn by_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "midnight" | "default" => Some(Self::midnight()),
            "nord" => Some(Self::nord()),
            "gruvbox" => Some(Self::gruvbox()),
            "mono" => Some(Self::mono()),
            _ => None,
        }
    }

    pub const NAMES: [&'static str; 4] = ["midnight", "nord", "gruvbox", "mono"];

    /// Point a token name at its slot, so config can address one colour.
    fn slot(&mut self, token: &str) -> Option<&mut Color> {
        Some(match token {
            "surface" => &mut self.surface,
            "surface_alt" => &mut self.surface_alt,
            "overlay" => &mut self.overlay,
            "border" => &mut self.border,
            "border_dim" => &mut self.border_dim,
            "text_bright" => &mut self.text_bright,
            "text" => &mut self.text,
            "text_muted" => &mut self.text_muted,
            "text_faint" => &mut self.text_faint,
            "text_ghost" => &mut self.text_ghost,
            "select_bg" => &mut self.select_bg,
            "select_bg_dim" => &mut self.select_bg_dim,
            "primary" => &mut self.primary,
            "accent" => &mut self.accent,
            "accent_dim" => &mut self.accent_dim,
            "info" => &mut self.info,
            "success" => &mut self.success,
            "success_dim" => &mut self.success_dim,
            "failure" => &mut self.failure,
            "failure_dim" => &mut self.failure_dim,
            "warning" => &mut self.warning,
            "unknown" => &mut self.unknown,
            "row_failure" => &mut self.row_failure,
            "row_running" => &mut self.row_running,
            "row_queued" => &mut self.row_queued,
            "row_idle" => &mut self.row_idle,
            _ => return None,
        })
    }

    /// Apply `[ui.colors]` on top of the palette.
    ///
    /// Returns what it could not use. A mistyped token or a malformed hex looks
    /// exactly like the setting being ignored, so the caller reports it rather
    /// than leaving the user to wonder which of the two happened.
    pub fn apply_overrides(&mut self, colors: &HashMap<String, String>) -> Vec<String> {
        let mut rejected: Vec<String> = colors
            .iter()
            .filter_map(|(token, value)| match (parse_hex(value), token.as_str()) {
                (Some(c), t) => {
                    let known = self.slot(t).map(|slot| *slot = c).is_some();
                    (!known).then(|| format!("{token} (unknown colour)"))
                }
                (None, _) => Some(format!("{token} = {value:?} (not #rrggbb)")),
            })
            .collect();
        rejected.sort();
        rejected
    }

    /// Fold every colour down to the xterm-256 cube.
    ///
    /// A terminal without truecolor doesn't reject 24-bit colour, it *rounds* it
    /// — usually to something muddy and inconsistent between two shades that
    /// were meant to differ. Choosing the nearest indexed colour ourselves keeps
    /// the palette's relationships intact at lower fidelity.
    pub fn degrade_to_256(&mut self) {
        for token in Self::TOKENS {
            if let Some(slot) = self.slot(token)
                && let Color::Rgb(r, g, b) = *slot
            {
                *slot = Color::Indexed(rgb_to_xterm256(r, g, b));
            }
        }
    }

    const TOKENS: [&'static str; 26] = [
        "surface", "surface_alt", "overlay", "border", "border_dim", "text_bright", "text",
        "text_muted", "text_faint", "text_ghost", "select_bg", "select_bg_dim", "primary",
        "accent", "accent_dim", "info", "success", "success_dim", "failure", "failure_dim",
        "warning", "unknown", "row_failure", "row_running", "row_queued", "row_idle",
    ];

    /// The house palette: cool slate, cyan headings, amber for anything moving.
    pub fn midnight() -> Self {
        Self {
            surface: Color::Rgb(28, 30, 42),
            surface_alt: Color::Rgb(35, 42, 58),
            overlay: Color::Rgb(18, 20, 32),
            border: Color::Rgb(55, 55, 80),
            border_dim: Color::Rgb(42, 42, 60),
            text_bright: Color::Rgb(220, 240, 255),
            text: Color::Rgb(190, 190, 212),
            text_muted: Color::Rgb(120, 120, 145),
            text_faint: Color::Rgb(92, 92, 118),
            text_ghost: Color::Rgb(64, 64, 82),
            select_bg: Color::Rgb(35, 95, 120),
            select_bg_dim: Color::Rgb(25, 85, 110),
            primary: Color::Rgb(96, 205, 226),
            accent: Color::Rgb(226, 192, 90),
            accent_dim: Color::Rgb(150, 132, 88),
            info: Color::Rgb(128, 158, 210),
            success: Color::Rgb(126, 202, 130),
            success_dim: Color::Rgb(96, 132, 100),
            failure: Color::Rgb(228, 110, 110),
            failure_dim: Color::Rgb(160, 96, 96),
            warning: Color::Rgb(226, 178, 78),
            unknown: Color::Rgb(96, 96, 112),
            row_failure: Color::Rgb(45, 20, 20),
            row_running: Color::Rgb(40, 36, 12),
            row_queued: Color::Rgb(18, 20, 40),
            row_idle: Color::Rgb(28, 30, 42),
        }
    }

    /// Nord: lower contrast, colder, easier on a bright room.
    pub fn nord() -> Self {
        Self {
            surface: Color::Rgb(46, 52, 64),
            surface_alt: Color::Rgb(59, 66, 82),
            overlay: Color::Rgb(36, 41, 51),
            border: Color::Rgb(76, 86, 106),
            border_dim: Color::Rgb(59, 66, 82),
            text_bright: Color::Rgb(236, 239, 244),
            text: Color::Rgb(216, 222, 233),
            text_muted: Color::Rgb(143, 154, 174),
            text_faint: Color::Rgb(110, 121, 141),
            text_ghost: Color::Rgb(76, 86, 106),
            select_bg: Color::Rgb(67, 96, 118),
            select_bg_dim: Color::Rgb(59, 82, 100),
            primary: Color::Rgb(136, 192, 208),
            accent: Color::Rgb(235, 203, 139),
            accent_dim: Color::Rgb(163, 143, 104),
            info: Color::Rgb(129, 161, 193),
            success: Color::Rgb(163, 190, 140),
            success_dim: Color::Rgb(122, 142, 108),
            failure: Color::Rgb(191, 97, 106),
            failure_dim: Color::Rgb(145, 84, 90),
            warning: Color::Rgb(235, 203, 139),
            unknown: Color::Rgb(103, 112, 130),
            row_failure: Color::Rgb(59, 44, 48),
            row_running: Color::Rgb(56, 55, 44),
            row_queued: Color::Rgb(46, 54, 68),
            row_idle: Color::Rgb(46, 52, 64),
        }
    }

    /// Gruvbox dark: warm, high contrast, retro.
    pub fn gruvbox() -> Self {
        Self {
            surface: Color::Rgb(40, 40, 40),
            surface_alt: Color::Rgb(60, 56, 54),
            overlay: Color::Rgb(29, 32, 33),
            border: Color::Rgb(80, 73, 69),
            border_dim: Color::Rgb(60, 56, 54),
            text_bright: Color::Rgb(251, 241, 199),
            text: Color::Rgb(235, 219, 178),
            text_muted: Color::Rgb(168, 153, 132),
            text_faint: Color::Rgb(124, 111, 100),
            text_ghost: Color::Rgb(80, 73, 69),
            select_bg: Color::Rgb(69, 84, 85),
            select_bg_dim: Color::Rgb(60, 70, 71),
            primary: Color::Rgb(131, 165, 152),
            accent: Color::Rgb(250, 189, 47),
            accent_dim: Color::Rgb(168, 132, 56),
            info: Color::Rgb(131, 165, 152),
            success: Color::Rgb(184, 187, 38),
            success_dim: Color::Rgb(134, 138, 44),
            failure: Color::Rgb(251, 73, 52),
            failure_dim: Color::Rgb(175, 62, 48),
            warning: Color::Rgb(254, 128, 25),
            unknown: Color::Rgb(124, 111, 100),
            row_failure: Color::Rgb(60, 34, 30),
            row_running: Color::Rgb(58, 48, 22),
            row_queued: Color::Rgb(40, 46, 46),
            row_idle: Color::Rgb(40, 40, 40),
        }
    }

    /// No hue at all. For screenshots, e-ink, projectors, and anyone whose
    /// terminal already carries the colour scheme they want.
    pub fn mono() -> Self {
        let g = |v: u8| Color::Rgb(v, v, v);
        Self {
            surface: g(24),
            surface_alt: g(38),
            overlay: g(16),
            border: g(70),
            border_dim: g(50),
            text_bright: g(245),
            text: g(200),
            text_muted: g(140),
            text_faint: g(105),
            text_ghost: g(75),
            select_bg: g(60),
            select_bg_dim: g(46),
            primary: g(235),
            accent: g(225),
            accent_dim: g(150),
            info: g(180),
            success: g(215),
            success_dim: g(140),
            failure: g(250),
            failure_dim: g(160),
            warning: g(230),
            unknown: g(110),
            row_failure: g(48),
            row_running: g(40),
            row_queued: g(32),
            row_idle: g(24),
        }
    }
}

/// What the footer message is reporting, so the colour can say it before the
/// words do: an op that failed and "opened in browser" should not read the
/// same at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Success,
    Error,
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
    /// Rendered row the cursor sits on.
    ///
    /// `usize`, along with every other rendered-row index below. These counted
    /// rows in a `u16` while nothing caps how many lines a job log has: a
    /// verbose build or a matrix test job clears 65 535 without trying, and past
    /// that the counter wrapped — silently in release, where `[profile.release]`
    /// turns overflow checks off — so group headers, fold markers and the
    /// minimap were all keyed 65 536 rows away from where they were drawn.
    pub log_line_cursor: usize,
    pub log_group_header_rows: Vec<usize>,
    pub log_rendered_group_map: HashMap<usize, usize>,
    /// Focus mode: show only error/warning lines plus `log_focus_context` lines
    /// of surrounding context, ignoring group collapse state.
    pub log_focus: bool,
    pub log_focus_context: usize,
    /// Fold markers standing in for skipped lines while focused: rendered row ->
    /// how many source lines it hides.
    pub log_fold_rows: HashMap<usize, usize>,
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
    /// Index into `log_rendered` of the first row drawn. See `log_line_cursor`
    /// for why this is not a `u16`.
    pub log_scroll: usize,
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
    pub status_kind: StatusKind,
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
    /// Glyph marking which forge a repo lives on, drawn to the left of every
    /// row that has one. Empty hides the column's worth of width entirely —
    /// the default is a Nerd Font glyph, and a terminal without one would
    /// otherwise show a box where the logo should be.
    pub forge_icon: String,
    pub history: History,
    pub theme: Theme,
    /// Run IDs we have seen in a non-terminal state during this Watch session.
    /// Used to fire a sound only when a run we were actively watching finishes.
    pub watch_seen_running: HashSet<u64>,
    /// Run IDs already announced, so a run that two watchers spot settling on
    /// the same tick still only makes one noise. See `announce_once`.
    pub announced_runs: HashSet<u64>,
    /// Multi-repo dashboard rows, in configured order.
    pub repos: Vec<RepoCard>,
    pub repo_cursor: usize,
    /// What is left of the hour's API budget, re-read once per poll.
    ///
    /// Every view spends from the same bucket, so this is read on the poll tick
    /// rather than per view: the number is only a warning if it keeps counting
    /// while you are off reading logs.
    pub quota: Option<Quota>,
    /// A quota read is in flight. Every dashboard row fails in the same tick, so
    /// without this the one question gets asked once per repo.
    pub quota_pending: bool,
    /// The alarm has already sounded for this hour's budget. Cleared when the
    /// quota resets, so the next hour can raise it again — and only once.
    pub quota_alarmed: bool,
    /// Polling is held off until this moment, because GitHub turned the last
    /// poll away for asking too fast.
    ///
    /// Without it the dashboard answers a refusal by asking again five seconds
    /// later, which is what the refusal was about: the secondary limit is fed
    /// by rejected requests too, so an eight-row dashboard can hold itself in
    /// the limit indefinitely. The wait is visible in the header — a poll that
    /// silently stops looks like a hang.
    pub api_paused_until: Option<DateTime<Utc>>,
    /// How long the current hold is, doubling for each refusal in a row and
    /// reset by the first poll that gets through. A fixed wait either gives up
    /// the dashboard for too long or walks straight back into the wall.
    pub api_backoff_secs: i64,
    /// Repos marked on the dashboard, by spec. Only ever fed to the batch —
    /// marking is inert until a batch is started, so it can't surprise anyone.
    pub repo_marks: HashSet<String>,
    /// The batch commit in progress, if any.
    pub batch: Option<BatchCommit>,
    /// Open fuzzy finder, if any. Rendered as an overlay above the current view.
    pub finder: Option<Finder>,
    /// Working-tree view for the repo we drilled into from the dashboard.
    pub git_view: Option<GitView>,
    /// The push question a landed commit raised. Owns the keyboard while it is
    /// up, like the message box it follows.
    pub push_prompt: Option<PushPrompt>,
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
    /// High-water mark of in-flight fetches in the current burst, and the tick
    /// the burst began. The mark is what lets the header count replies down as
    /// "fetching 3/8" instead of spinning blind, and the tick is how it can say
    /// a fetch has been dragging. `Cell` because the header tracks the burst in
    /// the act of drawing it; reset to zero the moment the burst drains.
    pub fetch_hwm: Cell<usize>,
    pub fetch_started_tick: Cell<u64>,
    /// Keybinding reference overlay.
    pub show_help: bool,
    pub help_scroll: u16,
    /// The tick the help card went up on — the clock its rows reveal
    /// themselves against, exactly like the services card's. `None` while it
    /// is closed (or on screen without an entrance to play — a test).
    pub help_opened_tick: Option<u64>,
    /// The tick the multi-repo dashboard was last landed on — the clock its
    /// rows arrive against, the same sweep the help and services cards play.
    /// Set on startup when the dashboard is the landing view and on every
    /// return to it; `None` when the entrance is not to be played (a test).
    pub dash_opened_tick: Option<u64>,
    /// Help has been opened at least once this session. What retires the
    /// footer's beacon: an invitation to a place you have already been is
    /// just blinking.
    pub help_seen: bool,
    /// How far the help card could scroll at last render, so the key and
    /// wheel handlers can stop at the bottom instead of counting past it
    /// into a distance that has to be scrolled back through.
    pub last_help_max_scroll: Cell<u16>,
    /// Directory the workspace scan was rooted at, when running outside a repo.
    pub workspace_root: Option<PathBuf>,
    /// Ticks between polls, and the tick the last one went out on.
    ///
    /// Only the header reads these, to say when the next refresh lands. A
    /// dashboard that silently refreshes every few seconds gives you no way to
    /// tell "nothing has changed" from "nothing has been fetched yet".
    pub poll_ticks: u64,
    pub last_poll_tick: u64,
    /// Jobs and steps of the in-flight runs on each dashboard row, keyed by
    /// repo.
    ///
    /// The row itself can only say *that* CI is going; this is what lets the
    /// strip at the bottom say which workflow and which step. A repo can have
    /// several workflows going at once — a push fans out to CI, a deploy, and a
    /// review bot — so this is a list per repo, not a single run: collapsing
    /// them would leave the strip claiming one workflow is the whole story.
    /// Entries are dropped the moment a run settles, so the strip appears and
    /// clears itself.
    pub run_progress: HashMap<String, Vec<RunDetail>>,
    /// Pushes being followed into CI — see [`PushWatch`]. A vec, not an option:
    /// a batch push starts one per repo.
    pub push_watches: Vec<PushWatch>,
    /// Per repo: the `.git` fingerprint last seen by the dashboard poll, and
    /// the tick of the last unconditional `git status`. What lets the poll
    /// skip the subprocess when nothing under `.git` has moved — see
    /// `crate::git::fingerprint` for what that does and doesn't cover.
    pub git_poll_gate: HashMap<String, (u64, u64)>,
    /// The tick the workspace last went quiet with every row green — the last
    /// in-flight run landed and nothing is red. Drives the one sweep of light
    /// across the header; `None` once it has faded (or before it ever happens).
    pub all_green_tick: Option<u64>,
    /// Whether the last look across the dashboard had anything in flight.
    /// The all-green moment is a *transition* — busy, then quiet-and-green —
    /// and a plain predicate would fire on every poll of a green morning.
    pub ci_was_busy: bool,
    /// Live tail of the running job on the watched run, refreshed once per
    /// poll while Watch is open. GitHub serves a running job's log-so-far
    /// from the same endpoint that serves the archive; some moments it has
    /// nothing yet, which is what `available` reports.
    pub watch_tail: Option<WatchTail>,
    /// A tail fetch is in flight — one per poll, not one per tick.
    pub watch_tail_pending: bool,
    /// Service health from the configured Uptime Kuma status page, refreshed
    /// once per poll. Empty when Kuma is unconfigured or has never answered.
    pub services: Vec<crate::kuma::Service>,
    /// Monitor name → dashboard repo, resolved when services arrive: the
    /// explicit `[uptime_kuma.map]` entries plus every monitor whose name
    /// matches a repo's. What the Live column joins on.
    pub service_repos: HashMap<String, String>,
    /// A Kuma read is in flight — one per poll, however slow the answer.
    pub kuma_pending: bool,
    /// A misconfigured Kuma has already said so once. A URL typo should be
    /// one status line, not one per poll forever.
    pub kuma_error_shown: bool,
    /// The service-health overlay: every monitor by name, with its ping and
    /// day's uptime — including the ones no dashboard row claims.
    pub show_services: bool,
    /// The tick the services card went up on — the clock its rows reveal
    /// themselves against. `None` while it is closed, and while it is on
    /// screen without an entrance to play (a redraw, a test).
    pub services_opened_tick: Option<u64>,
    /// When the readings on screen were fetched — what the card's "updated
    /// Ns ago" is measured from. `None` until Kuma first answers.
    pub kuma_fetched_at: Option<DateTime<Utc>>,
    /// The tick the last Kuma read went out on. Kuma runs on its own, slower
    /// clock than the CI poll; this is what holds it to that clock.
    pub kuma_last_poll_tick: u64,
    /// The `[uptime_kuma]` config, copied in at startup so the card's manual
    /// refresh can fire a fetch without threading `Config` through every key.
    pub kuma: Option<crate::config::UptimeKumaConfig>,
    /// Clickable regions of the frame on screen, rebuilt on every render.
    /// `RefCell` because views write it through `&AppState`, like the viewport
    /// cells above.
    pub hits: RefCell<Vec<(Rect, Hit)>>,
    /// Notifications (sounds and desktop) are muted until this moment — the
    /// meeting-mode switch. `None` when not snoozed. Runs that settle while
    /// muted are still marked announced, so the end of a snooze does not
    /// release a backlog of stale dings.
    pub snooze_until: Option<DateTime<Utc>>,
    /// Whether config would announce anything at all. When it wouldn't
    /// (`notify = "never"`, or both channels off), the header wears no bell:
    /// a permanently slashed bell would nag about a choice already made.
    pub notify_enabled: bool,
    /// Header bell glyphs — live and snoozed. See `DEFAULT_BELL_ICON`.
    pub bell_icon: String,
    pub bell_off_icon: String,
    /// Width of the last frame drawn. What lets the event loop skip fetching a
    /// live tail for the dashboard's side pane when the terminal is too narrow
    /// to ever show one.
    pub last_frame_width: Cell<u16>,
    /// Why the failed run open in the detail view failed — see
    /// [`FailureDigest`].
    pub failure_digest: Option<FailureDigest>,
    /// A digest log fetch is in flight for this run, so a re-entered detail
    /// view doesn't fetch the same log twice.
    pub digest_pending: Option<u64>,
}

/// What the Watch view's live log pane holds: whose log it is and the last
/// lines of it. Replaced whole on every fetch — the endpoint returns the log
/// from the top, so there is no append to do.
#[derive(Debug, Clone)]
pub struct WatchTail {
    pub job_id: u64,
    pub job_name: String,
    pub lines: Vec<String>,
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
            status_kind: StatusKind::Info,
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
            forge_icon: DEFAULT_FORGE_ICON.to_string(),
            history,
            theme: Theme::default(),
            watch_seen_running: HashSet::new(),
            announced_runs: HashSet::new(),
            repos: Vec::new(),
            repo_cursor: 0,
            quota: None,
            quota_pending: false,
            quota_alarmed: false,
            api_paused_until: None,
            api_backoff_secs: 0,
            repo_marks: HashSet::new(),
            batch: None,
            finder: None,
            git_view: None,
            push_prompt: None,
            git_ops: HashMap::new(),
            git_diff: None,
            last_diff_viewport_height: Cell::new(0),
            last_op_viewport_height: Cell::new(0),
            fetch_hwm: Cell::new(0),
            fetch_started_tick: Cell::new(0),
            show_help: false,
            help_scroll: 0,
            help_opened_tick: None,
            dash_opened_tick: None,
            help_seen: false,
            last_help_max_scroll: Cell::new(0),
            workspace_root: None,
            poll_ticks: 50,
            last_poll_tick: 0,
            run_progress: HashMap::new(),
            push_watches: Vec::new(),
            git_poll_gate: HashMap::new(),
            all_green_tick: None,
            ci_was_busy: false,
            watch_tail: None,
            watch_tail_pending: false,
            services: Vec::new(),
            service_repos: HashMap::new(),
            kuma_pending: false,
            kuma_error_shown: false,
            show_services: false,
            services_opened_tick: None,
            kuma_fetched_at: None,
            kuma_last_poll_tick: 0,
            kuma: None,
            hits: RefCell::new(Vec::new()),
            snooze_until: None,
            notify_enabled: true,
            bell_icon: DEFAULT_BELL_ICON.to_string(),
            bell_off_icon: DEFAULT_BELL_OFF_ICON.to_string(),
            last_frame_width: Cell::new(0),
            failure_digest: None,
            digest_pending: None,
        }
    }

    /// Whether notifications are currently snoozed.
    pub fn notify_snoozed(&self) -> bool {
        self.snooze_until.is_some_and(|t| Utc::now() < t)
    }

    /// The run the dashboard's live pane should follow: the first in-flight run
    /// (in table order, so the target doesn't jump between polls) that has a
    /// job actually producing a log right now.
    pub fn dash_tail_target(&self) -> Option<(&str, &RunDetail, &Job)> {
        self.active_progress().into_iter().find_map(|(card, d)| {
            d.jobs
                .iter()
                .find(|j| j.status == Status::Running)
                .map(|j| (card.spec.as_str(), d, j))
        })
    }

    /// The monitors watching one dashboard repo, in status-page order.
    pub fn repo_services(&self, spec: &str) -> impl Iterator<Item = &crate::kuma::Service> {
        self.services
            .iter()
            .filter(move |s| self.service_repos.get(&s.name).is_some_and(|r| r == spec))
    }

    /// Runs in flight across the dashboard, in the order the table lists their
    /// rows — and, within a row, the order the repo's run list gives.
    ///
    /// Table order rather than "most recently started" so a row and its strip
    /// entries stay in the same relative place — a list that reshuffles itself
    /// every poll is unreadable at a glance. A repo running two workflows
    /// contributes two entries; that is the point.
    pub fn active_progress(&self) -> Vec<(&RepoCard, &RunDetail)> {
        self.repos
            .iter()
            .filter_map(|c| self.run_progress.get(&c.spec).map(|ds| (c, ds)))
            .flat_map(|(c, ds)| ds.iter().map(move |d| (c, d)))
            .filter(|(_, d)| !d.run.status.is_terminal())
            .collect()
    }

    /// Seconds still to sit out before jog talks to GitHub again, if it is
    /// waiting at all. `None` is the normal state.
    pub fn api_hold_left(&self) -> Option<i64> {
        let until = self.api_paused_until?;
        let left = (until - Utc::now()).num_seconds();
        (left > 0).then_some(left)
    }

    /// Whether this poll should stay off the wire.
    pub fn api_held(&self) -> bool {
        self.api_hold_left().is_some()
    }

    /// Sit out a poll or several: GitHub just turned us away for pace, and the
    /// only thing that clears that is not asking. Each refusal in a row doubles
    /// the wait; `clear_api_hold` puts it back once a poll gets through.
    ///
    /// `until` overrides the ladder when GitHub has told us the actual moment —
    /// the hourly quota's reset — because guessing shorter only spends the
    /// requests that prove it is still spent.
    pub fn hold_api(&mut self, until: Option<DateTime<Utc>>) {
        const FIRST: i64 = 15;
        const MAX: i64 = 120;
        if let Some(t) = until.filter(|t| *t > Utc::now()) {
            self.api_paused_until = Some(t);
            return;
        }
        // Already waiting on this refusal — every other row's copy of the same
        // failure must not push the deadline out again.
        if self.api_held() {
            return;
        }
        self.api_backoff_secs = if self.api_backoff_secs == 0 {
            FIRST
        } else {
            (self.api_backoff_secs * 2).min(MAX)
        };
        self.api_paused_until = Some(Utc::now() + chrono::Duration::seconds(self.api_backoff_secs));
    }

    /// A poll got through: stop waiting, and forget how long the last wait was.
    pub fn clear_api_hold(&mut self) {
        self.api_paused_until = None;
        self.api_backoff_secs = 0;
    }

    pub fn switch_view(&mut self, v: View) {
        if self.view != v {
            self.view = v;
            self.needs_clear = true;
            // Coming back to the dashboard replays its entrance, so the sweep
            // is a property of arriving at the view rather than of starting
            // the program — the same rule the help and services cards follow.
            if v == View::Repos {
                self.dash_opened_tick = Some(self.tick_count);
            }
        }
    }

    pub fn set_status(&mut self, msg: String) {
        self.set_status_of(msg, StatusKind::Info);
    }

    /// An op that completed — the footer says so in green.
    pub fn set_status_ok(&mut self, msg: String) {
        self.set_status_of(msg, StatusKind::Success);
    }

    /// An op that failed — red, kept for actual failures rather than guards
    /// and refusals, so the colour still means something.
    pub fn set_status_err(&mut self, msg: String) {
        self.set_status_of(msg, StatusKind::Error);
    }

    fn set_status_of(&mut self, msg: String, kind: StatusKind) {
        self.status_msg = Some(msg);
        self.status_kind = kind;
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
        self.log_group_header_rows = vec![0usize; self.log_groups.len()];
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
    pub fn expand_fold_at(&mut self, row: usize) -> bool {
        if !self.log_fold_rows.contains_key(&row) {
            return false;
        }
        match self.log_rendered_src.get(row).copied() {
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
    pub fn max_log_scroll(&self) -> usize {
        let viewport = self.last_logs_viewport_height.get().max(1) as usize;
        let mut used = 0usize;
        for row in (0..self.log_rendered.len()).rev() {
            used += self.visual_height_of_row(row) as usize;
            if used > viewport {
                return row + 1;
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
        let cursor = self.log_line_cursor;
        if cursor < self.log_scroll {
            self.log_scroll = cursor;
            return;
        }
        if self.last_visible_row(self.log_scroll) >= cursor {
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
        self.log_scroll = self.log_scroll.max(first);
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
        let cursor = self.log_line_cursor;
        let cursor_bg = Style::default().bg(self.theme.surface_alt);
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
                    out = highlight_line(out, needle, true, &self.theme);
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
        let mut first = self.log_line_cursor;
        while first > 0 {
            let h = self.visual_height_of_row(first - 1) as usize;
            if used + h > budget {
                break;
            }
            used += h;
            first -= 1;
        }
        self.log_scroll = first;
    }

    /// Cursor row that displays source line `src`, if it is currently visible.
    pub fn rendered_row_for_src(&self, src: usize) -> Option<usize> {
        self.log_rendered_src.iter().position(|&s| s == src)
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

        let time_style = Style::default().fg(self.theme.text_ghost);

        let header_to_group: HashMap<usize, usize> = self.log_groups.iter().enumerate()
            .map(|(gi, g)| (g.header_line, gi))
            .collect();

        let hidden = self.compute_hidden_lines();

        // Pass 1 (sequential, cheap): which source lines survive filtering, and
        // where each group header lands. Row positions have to be assigned in
        // order, so this can't be parallel — but it's just index bookkeeping.
        let mut rendered_src: Vec<usize> = Vec::with_capacity(self.log_lines.len());
        let mut group_header_rows = vec![0usize; self.log_groups.len()];
        let mut group_map: HashMap<usize, usize> = HashMap::new();
        let mut fold_rows: HashMap<usize, usize> = HashMap::new();
        let mut rendered_row: usize = 0;
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
                if let Some(&n) = folds.get(&row) {
                    let plural = if n == 1 { "line" } else { "lines" };
                    return Line::from(Span::styled(
                        format!("  ⋯ {n} {plural} hidden — ↵ to show ⋯"),
                        Style::default().fg(self.theme.text_faint).italic(),
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
                let title_style = Style::default().fg(self.theme.primary).bold();
                let arrow = if is_collapsed { "▶ " } else { "▾ " };
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled(arrow, title_style));
                spans.extend(ansi_line_to_spans(title, title_style));
                Line::from(spans)
            } else if let Some(cmd) = content.strip_prefix("##[command]") {
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("▶ ", Style::default().fg(self.theme.success).bold()));
                spans.extend(ansi_line_to_spans(cmd, Style::default().fg(self.theme.text_bright)));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[error]") {
                let s = Style::default().fg(self.theme.failure).bold();
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("✗ ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[warning]") {
                let s = Style::default().fg(self.theme.warning);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("⚠ ", s.bold()));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[debug]") {
                let s = Style::default().fg(self.theme.text_faint);
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.push(Span::styled("# ", s));
                spans.extend(ansi_line_to_spans(msg, s));
                Line::from(spans)
            } else if let Some(msg) = content.strip_prefix("##[notice]") {
                let s = Style::default().fg(self.theme.primary);
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
                    Style::default().fg(self.theme.failure)
                } else if trimmed_lower.starts_with("warn") {
                    Style::default().fg(self.theme.warning)
                } else if trimmed_lower.starts_with('=') && trimmed_lower.len() > 3
                    && trimmed_lower[..4].chars().all(|c| c == '=')
                {
                    Style::default().fg(self.theme.warning).bold()
                } else if trimmed_lower.starts_with('-') && trimmed_lower.len() > 3
                    && trimmed_lower[..4].chars().all(|c| c == '-')
                {
                    Style::default().fg(self.theme.text_faint)
                } else {
                    Style::default().fg(self.theme.text)
                };
                let mut spans = vec![];
                if let Some(ts) = mk_time() { spans.push(ts); }
                spans.extend(ansi_line_to_spans(content, base));
                Line::from(spans)
            };

                match needle_lower.as_deref() {
                    Some(needle) => highlight_line(line, needle, false, &self.theme),
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
            // Step past the terminator — but only if there was one. A line that
            // ends mid-sequence leaves `j` at the end, and `j + 1` would then
            // put both cursors one past it, so the tail slice below would run
            // off the end and take the whole TUI with it. `strip_ansi` has
            // always clamped here; this is the same clamp.
            i = if j < chars.len() { j + 1 } else { j };
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

pub fn highlight_line(
    line: Line<'static>,
    needle: &str,
    current: bool,
    theme: &Theme,
) -> Line<'static> {
    if needle.is_empty() {
        return line;
    }
    let hit_bg = if current { theme.accent } else { theme.accent_dim };
    let hit_fg = theme.surface;
    let need: Vec<char> = needle.chars().collect();

    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let text = span.content.into_owned();
        let style = span.style;
        // Lowercase the span one character at a time, remembering which byte
        // span of `text` each lowercase character came from.
        //
        // The obvious version — search `text.to_lowercase()` and slice `text`
        // at the offsets it reports — is wrong, because lowercasing is not
        // length-preserving in bytes: `K` (U+212A) shrinks from three bytes to
        // one, `İ` (U+0130) grows from two to three. The offsets then address a
        // string that no longer exists, so they landed mid-character or past
        // the end and took the whole process with them. Every index used to cut
        // `text` below came out of `text.char_indices()`.
        let mut hay: Vec<char> = Vec::with_capacity(text.len());
        let mut owner: Vec<(usize, usize)> = Vec::with_capacity(text.len());
        for (at, ch) in text.char_indices() {
            let source = (at, at + ch.len_utf8());
            for lc in ch.to_lowercase() {
                hay.push(lc);
                owner.push(source);
            }
        }

        let mut cursor = 0usize; // byte offset into `text`
        let mut k = 0usize; // char offset into `hay`
        let mut hit = false;
        while k + need.len() <= hay.len() {
            if hay[k..k + need.len()] != need[..] {
                k += 1;
                continue;
            }
            // Whole source characters, always. A needle can match part of one
            // character's expansion (`i` inside `İ`); there is no way to draw
            // half a character, so the character is highlighted entire — and a
            // match reaching back into one already emitted is passed over,
            // which is also what keeps `cursor` moving forward.
            let (start, _) = owner[k];
            let (_, end) = owner[k + need.len() - 1];
            if start < cursor {
                k += 1;
                continue;
            }
            if start > cursor {
                out.push(Span::styled(text[cursor..start].to_string(), style));
            }
            out.push(Span::styled(
                text[start..end].to_string(),
                style.bg(hit_bg).fg(hit_fg).add_modifier(Modifier::BOLD),
            ));
            cursor = end;
            hit = true;
            k += need.len();
        }
        if !hit {
            out.push(Span::styled(text, style));
        } else if cursor < text.len() {
            out.push(Span::styled(text[cursor..].to_string(), style));
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
    use crate::provider::WorkflowInput;

    #[test]
    fn the_trigger_prompt_prefills_what_was_dispatched_last_time() {
        let wf = Workflow {
            name: "Deploy".into(),
            file_name: "deploy.yml".into(),
            triggerable: true,
            last_status: None,
            last_run_at: None,
            inputs: vec![
                WorkflowInput {
                    name: "env".into(),
                    required: false,
                    default: Some("staging".into()),
                    options: Some(vec!["staging".into(), "prod".into()]),
                },
                WorkflowInput {
                    name: "tag".into(),
                    required: false,
                    default: None,
                    options: None,
                },
                WorkflowInput {
                    name: "mode".into(),
                    required: false,
                    default: Some("fast".into()),
                    options: Some(vec!["fast".into(), "slow".into()]),
                },
            ],
        };
        let recall: HashMap<String, String> = [
            ("env".to_string(), "prod".to_string()),
            ("tag".to_string(), "v3".to_string()),
            // A choice the workflow no longer offers must not be resurrected.
            ("mode".to_string(), "gone".to_string()),
        ]
        .into_iter()
        .collect();

        let p = TriggerPrompt::from_workflow(&wf, View::Workflows, Some(&recall));
        let field = |n: &str| p.fields.iter().find(|f| f.name == n).unwrap();
        assert_eq!(field("env").value, "prod");
        assert!(field("env").recalled, "a recall that changed the value is marked");
        assert_eq!(field("tag").value, "v3");
        assert!(field("tag").recalled);
        assert_eq!(field("mode").value, "fast", "an invalid recall falls back to the default");
        assert!(!field("mode").recalled);

        // Without history the defaults stand, unmarked.
        let bare = TriggerPrompt::from_workflow(&wf, View::Workflows, None);
        assert_eq!(bare.fields[0].value, "staging");
        assert!(bare.fields.iter().all(|f| !f.recalled));
    }

    fn lines(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    fn section(label: &'static str, text: &str) -> crate::git::DiffSection {
        crate::git::DiffSection { label, text: text.to_string() }
    }

    /// A batch over three repos, already past the message box.
    fn batch_of(n: usize) -> BatchCommit {
        let items = (0..n)
            .map(|i| BatchItem::new(format!("acme/r{i}"), PathBuf::from(format!("/tmp/r{i}"))))
            .collect();
        let mut b = BatchCommit::new(items, 0);
        b.input = None;
        b.message = "fix: bump deps".into();
        b.phase = BatchPhase::Committing;
        b
    }

    /// Run the queue to a standstill, answering each repo with `answer`.
    fn drive(b: &mut BatchCommit, answer: impl Fn(&str) -> Result<String, String>) {
        while let Some(i) = b.advance(0) {
            let spec = b.items[i].spec.clone();
            let a = answer(&spec);
            b.record(&spec, a);
        }
    }

    #[test]
    fn the_batch_commits_one_repo_at_a_time_then_asks_about_pushing() {
        let mut b = batch_of(3);
        // Only ever one repo in flight: that is what makes a failure readable.
        let first = b.advance(0).unwrap();
        assert_eq!(b.items[first].state, ItemState::Running);
        assert_eq!(
            b.items.iter().filter(|i| i.state == ItemState::Running).count(),
            1
        );
        b.record("acme/r0", Ok("abc1234".into()));
        drive(&mut b, |_| Ok("def5678".into()));

        assert_eq!(b.tally().committed, 3);
        assert_eq!(b.items[0].sha.as_deref(), Some("abc1234"));
        // Pushing is a separate decision, never a consequence of committing.
        assert_eq!(b.phase, BatchPhase::AskPush);
        assert_eq!(b.tally().pushed, 0);
    }

    #[test]
    fn a_failed_hook_pauses_the_batch_instead_of_moving_on() {
        let mut b = batch_of(3);
        b.advance(0);
        b.record("acme/r0", Err("pre-commit hook failed".into()));

        assert_eq!(b.phase, BatchPhase::Paused);
        // The two untouched repos must still be untouched: starting the next one
        // would scroll the output that explains the failure off the screen.
        assert_eq!(b.items[1].state, ItemState::Queued);
        assert_eq!(b.items[2].state, ItemState::Queued);
        assert_eq!(b.advance(0), None, "a paused batch starts nothing");
        // …and asking is not answering: winding the pause up to Done here would
        // throw away the retry/skip decision the pause exists to wait for.
        assert_eq!(b.phase, BatchPhase::Paused);
    }

    #[test]
    fn a_push_with_nothing_to_do_still_reports_the_commit_that_landed() {
        let mut b = batch_of(1);
        drive(&mut b, |_| Ok("abc1234".into()));
        b.phase = BatchPhase::Pushing;
        b.advance(0);
        // What the push worker says when it finds a detached HEAD: the commit is
        // real and on disk, there is just nowhere to push it to.
        b.record_nothing("acme/r0", "detached HEAD".into());

        // The repo must stop being pushable or the queue would hand it back
        // forever — but a summary saying "0 committed" would send the user away
        // believing nothing happened.
        assert!(!b.items[0].ready_to_push());
        assert_eq!(b.items[0].sha.as_deref(), Some("abc1234"));
        assert_eq!(b.tally().committed, 1);
        // …and counted once: one repo cannot be both "1 committed" and "1 had
        // nothing to do" in a summary the user reads as a headcount.
        assert_eq!(b.tally().nothing, 0);
        assert_eq!(b.advance(0), None);
        assert_eq!(b.phase, BatchPhase::Done);
    }

    #[test]
    fn skip_leaves_the_failure_on_the_record_and_carries_on() {
        let mut b = batch_of(3);
        b.advance(0);
        b.record("acme/r0", Err("pytest failed".into()));
        b.skip();

        assert_eq!(b.phase, BatchPhase::Committing);
        drive(&mut b, |_| Ok("abc1234".into()));
        // Skipping is not forgetting: the repo that failed still says so.
        assert!(matches!(b.items[0].state, ItemState::Failed(_)));
        assert_eq!(b.tally().failed, 1);
        assert_eq!(b.tally().committed, 2);
    }

    #[test]
    fn retry_reruns_the_commit_that_failed() {
        let mut b = batch_of(2);
        b.advance(0);
        b.record("acme/r0", Err("pytest failed".into()));
        b.retry();

        assert_eq!(b.phase, BatchPhase::Committing);
        assert_eq!(b.advance(0), Some(0), "the same repo goes again");
        b.record("acme/r0", Ok("abc1234".into()));
        assert_eq!(b.items[0].state, ItemState::Committed);
    }

    #[test]
    fn a_failed_push_retries_the_push_not_the_commit() {
        let mut b = batch_of(1);
        drive(&mut b, |_| Ok("abc1234".into()));
        b.phase = BatchPhase::Pushing;
        b.advance(0);
        b.record("acme/r0", Err("pre-push hook failed".into()));
        assert_eq!(b.phase, BatchPhase::Paused);

        b.retry();
        // Re-committing would find nothing to commit and look like a new bug;
        // the commit is already on disk and only the push is outstanding.
        assert_eq!(b.items[0].state, ItemState::Committed);
        assert_eq!(b.advance(0), Some(0));
        assert_eq!(b.phase, BatchPhase::Pushing);
    }

    #[test]
    fn a_repo_that_committed_before_a_push_failed_still_says_so() {
        let mut b = batch_of(1);
        drive(&mut b, |_| Ok("abc1234".into()));
        b.phase = BatchPhase::Pushing;
        b.advance(0);
        b.record("acme/r0", Err("no upstream".into()));
        // Losing the sha would report the repo as untouched when it has a commit
        // sitting on it — the one thing you must know before walking away.
        assert_eq!(b.items[0].sha.as_deref(), Some("abc1234"));
    }

    #[test]
    fn stopping_the_batch_leaves_earlier_commits_alone() {
        let mut b = batch_of(3);
        b.advance(0);
        b.record("acme/r0", Ok("abc1234".into()));
        b.advance(0);
        b.abort();
        // The repo in flight still reports what it did — a hook that has already
        // run cannot be undone, so pretending otherwise would be a lie.
        b.record("acme/r1", Ok("def5678".into()));

        assert_eq!(b.phase, BatchPhase::Done, "an abort is not a pause");
        assert_eq!(b.tally().committed, 2);
        assert_eq!(b.items[2].state, ItemState::Queued);
        assert_eq!(b.tally().untouched, 1);
        assert_eq!(b.advance(0), None);
    }

    #[test]
    fn a_batch_with_nothing_to_commit_never_asks_about_pushing() {
        let mut b = batch_of(2);
        while let Some(i) = b.advance(0) {
            let spec = b.items[i].spec.clone();
            b.record_nothing(&spec, "working tree is clean".into());
        }
        assert_eq!(b.phase, BatchPhase::Done);
        assert_eq!(b.tally().nothing, 2);
    }

    #[test]
    fn pushing_only_touches_what_the_batch_committed() {
        let mut b = batch_of(3);
        b.advance(0);
        b.record("acme/r0", Ok("abc1234".into()));
        b.advance(0);
        b.record("acme/r1", Err("pytest failed".into()));
        b.skip();
        b.advance(0);
        b.record_nothing("acme/r2", "working tree is clean".into());
        b.advance(0);
        assert_eq!(b.phase, BatchPhase::AskPush);

        b.phase = BatchPhase::Pushing;
        let mut pushed = Vec::new();
        while let Some(i) = b.advance(0) {
            let spec = b.items[i].spec.clone();
            pushed.push(spec.clone());
            b.record(&spec, Ok("pushed main".into()));
        }
        // The failed repo has no commit and the clean one has no change; pushing
        // either would be pushing something the batch did not make.
        assert_eq!(pushed, vec!["acme/r0"]);
        assert_eq!(b.phase, BatchPhase::Done);
        assert_eq!(b.tally().pushed, 1);
    }

    #[test]
    fn every_shipped_theme_defines_every_token() {
        // A palette that forgot a token would inherit whatever `Default` had —
        // one stray midnight blue in the middle of gruvbox, and no compiler
        // error, since they are all just `Color`.
        for name in Theme::NAMES {
            let mut t = Theme::by_name(name).unwrap_or_else(|| panic!("{name} missing"));
            for token in Theme::TOKENS {
                assert!(t.slot(token).is_some(), "{name} has no {token}");
            }
        }
        assert!(Theme::by_name("nope").is_none());
        // Case and stray whitespace come from a hand-edited config file.
        assert!(Theme::by_name(" GruvBox ").is_some());
    }

    #[test]
    fn a_colour_override_that_cannot_work_says_so() {
        let mut t = Theme::midnight();
        let cfg = HashMap::from([
            ("accent".to_string(), "#ff8800".to_string()),
            ("text".to_string(), "00ff00".to_string()),
            ("acsent".to_string(), "#ff8800".to_string()),
            ("failure".to_string(), "red".to_string()),
        ]);
        let rejected = t.apply_overrides(&cfg);

        assert_eq!(t.accent, Color::Rgb(255, 136, 0));
        assert_eq!(t.text, Color::Rgb(0, 255, 0), "the # is optional");
        // A typo and a bad value are both reported: silently ignoring either is
        // indistinguishable from the whole setting not working.
        assert_eq!(
            rejected,
            vec![
                "acsent (unknown colour)".to_string(),
                "failure = \"red\" (not #rrggbb)".to_string(),
            ]
        );
        assert_eq!(t.failure, Theme::midnight().failure, "left alone");
    }

    #[test]
    fn degrading_a_palette_keeps_its_colours_apart() {
        let mut t = Theme::midnight();
        t.degrade_to_256();
        // The point of choosing the indexed colour ourselves is that shades
        // meant to differ still differ; a terminal rounding them itself is what
        // collapses a five-step text ramp into two.
        let ramp = [t.text_bright, t.text, t.text_muted, t.text_faint, t.text_ghost];
        for (i, c) in ramp.iter().enumerate() {
            assert!(matches!(c, Color::Indexed(_)), "{i} not degraded");
            assert!(!ramp[i + 1..].contains(c), "step {i} collapsed into a later one");
        }
        // Status colours must not land on the same cell either.
        assert_ne!(t.success, t.failure);
        assert_ne!(t.warning, t.accent_dim);
    }

    #[test]
    fn a_grey_degrades_to_the_grey_ramp_not_the_colour_cube() {
        // The cube's nearest neighbour to a near-grey is usually tinted, which
        // is how a neutral border turns faintly purple on a 256-colour term.
        assert!((232..=255).contains(&rgb_to_xterm256(120, 120, 122)));
        assert_eq!(rgb_to_xterm256(0, 0, 0), 16);
        assert_eq!(rgb_to_xterm256(255, 255, 255), 231);
        // …and a colour that is actually coloured still goes to the cube.
        assert!(rgb_to_xterm256(228, 110, 110) < 232);
    }

    fn op_with(raw: &[&str]) -> GitOp {
        let mut op = GitOp::new("commit", Some("pre-commit".into()), 0);
        for l in raw {
            op.push_line((*l).to_string(), false);
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
    fn a_partial_line_is_replaced_by_its_own_completion() {
        let mut op = GitOp::new("commit", None, 0);
        op.push_line("ruff format...Passed".into(), false);
        // pre-commit names the running step before it finishes; the pane must
        // show it now, not after pytest is done.
        op.push_line("pytest".into(), true);
        op.push_line("pytest....".into(), true);
        assert_eq!(op.lines.len(), 2);
        assert_eq!(op.lines[1].text, "pytest....");
        // The verdict arrives as the completed line, overwriting the draft —
        // one row, like a terminal, not the draft with the verdict under it.
        op.push_line("pytest....Passed".into(), false);
        assert_eq!(op.lines.len(), 2);
        assert_eq!(op.lines[1].text, "pytest....Passed");
        // And the next line goes below it, not over it.
        op.push_line("done".into(), false);
        assert_eq!(op.lines.len(), 3);
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
        op.push_line("five".into(), false);
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
            op.push_line(format!("line {i}"), false);
        }
        op.scroll = Some(10);
        op.push_line("overflow".into(), false);
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
    fn the_changed_word_is_marked_on_both_sides_of_a_pair() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section("unstaged", "-let x = 1;\n+let x = 2;\n")]);
        // Everything but the digit is shared; the mark lands on the digit,
        // offset by one for the `-`/`+` marker.
        assert_eq!(dv.emphasis, vec![Some((9, 10)), Some((9, 10))]);
    }

    #[test]
    fn rows_pair_the_old_and_new_and_number_each_side() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section(
            "unstaged",
            "@@ -12,3 +12,4 @@\n ctx\n-old\n+new\n+extra\n ctx2\n",
        )]);
        let sides = |r: &DiffRow| match r {
            DiffRow::Pair { old, new } => (
                old.as_ref().map(|s| (s.text.clone(), s.num)),
                new.as_ref().map(|s| (s.text.clone(), s.num)),
            ),
            _ => panic!("not a pair: {r:?}"),
        };
        assert_eq!(dv.rows[0], DiffRow::Meta("@@ -12,3 +12,4 @@".into()));
        // Context counts on both sides and keeps its own number on each.
        assert_eq!(
            sides(&dv.rows[1]),
            (Some(("ctx".into(), Some(12))), Some(("ctx".into(), Some(12))))
        );
        // The replaced line is one row, read across.
        assert_eq!(
            sides(&dv.rows[2]),
            (Some(("old".into(), Some(13))), Some(("new".into(), Some(13))))
        );
        // The run is longer on one side, so the row opposite the extra line is
        // a gap rather than someone else's line pulled up into it.
        assert_eq!(sides(&dv.rows[3]), (None, Some(("extra".into(), Some(14)))));
        // And the numbering has diverged by exactly the line that was added.
        assert_eq!(
            sides(&dv.rows[4]),
            (Some(("ctx2".into(), Some(14))), Some(("ctx2".into(), Some(15))))
        );
    }

    #[test]
    fn tabs_are_expanded_and_the_mark_moves_with_them() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        dv.set_sections(vec![section("unstaged", "-\tlet x = 1;\n+\tlet x = 2;\n")]);
        let DiffRow::Pair { old: Some(old), new: Some(new) } = &dv.rows[0] else {
            panic!("not a pair: {:?}", dv.rows[0]);
        };
        // A raw tab jumps to the terminal's own stop, measured from the edge of
        // the screen — inside a padded column it lands anywhere.
        assert_eq!(old.text, "    let x = 1;");
        assert_eq!(new.text, "    let x = 2;");
        // And the emphasis still points at the digit it was measured against,
        // three bytes further along on each side.
        let at = |s: &DiffSide| s.emph.map(|(a, b)| s.text[a..b].to_string());
        assert_eq!(at(old).as_deref(), Some("1"));
        assert_eq!(at(new).as_deref(), Some("2"));
    }

    #[test]
    fn wholly_different_lines_are_left_to_the_line_colour() {
        // No common edge at all: these are different lines, not an edit of
        // one, and marking all of both would just restate the colour.
        assert_eq!(changed_spans("abc", "xyz"), None);
    }

    #[test]
    fn unbalanced_runs_are_not_guessed_at() {
        let mut dv = GitDiffView::new("acme/api".into(), "src/a.rs".into());
        // Two deletions, one addition: pairing the first `-` with the `+`
        // would be a guess, and a wrong one marks unrelated lines.
        dv.set_sections(vec![section("unstaged", "-let a = 1;\n-let b = 1;\n+let a = 2;\n")]);
        assert_eq!(dv.emphasis, vec![None, None, None]);
    }

    #[test]
    fn a_pure_insertion_marks_only_the_side_that_grew() {
        // "ab" → "axb": the old line has no span of its own to mark.
        assert_eq!(changed_spans("ab", "axb"), Some((None, Some((1, 2)))));
    }

    #[test]
    fn emphasis_lands_on_char_boundaries_in_multibyte_text() {
        // "héllo" → "hällo": the changed char is 2 bytes wide, and a range
        // that split it would panic at render time when sliced.
        assert_eq!(
            changed_spans("héllo", "hällo"),
            Some((Some((1, 3)), Some((1, 3))))
        );
    }

    #[test]
    fn the_footer_kind_follows_the_message() {
        let mut st = AppState::new(
            "acme/api".into(),
            "main".into(),
            Vec::new(),
            crate::config::KeymapConfig::default(),
            crate::history::History::default(),
        );
        st.set_status_err("push failed".into());
        assert_eq!(st.status_kind, StatusKind::Error);
        // A plain note afterwards must not stay dressed in red.
        st.set_status("opened in browser".into());
        assert_eq!(st.status_kind, StatusKind::Info);
        st.set_status_ok("pushed".into());
        assert_eq!(st.status_kind, StatusKind::Success);
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
        let cursor_bg = Some(Theme::default().surface_alt);
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
        let cursor_bg = Some(Theme::default().surface_alt);
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
        assert!(st.last_visible_row(st.log_scroll) >= 999);
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
            std::hint::black_box(st.decorate_visible(st.log_scroll, 40));
        });

        // Scrolling to the bottom, the worst case for keep_cursor_visible.
        st.log_scroll = 0;
        st.log_line_cursor = st.log_rendered.len() - 1;
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
        let mut rows: Vec<(usize, usize)> = st.log_fold_rows.iter().map(|(&r, &n)| (r, n)).collect();
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

    // ── Regression: text handling that used to take the process down ──────

    #[test]
    fn a_line_that_ends_mid_escape_sequence_does_not_take_the_tui_with_it() {
        // A CSI with no terminator — a truncated write, a killed process, a
        // progress bar cut off by the runner. The scanner used to step one past
        // the end of the line and slice the tail from there.
        let spans = ansi_line_to_spans("hello \x1b[31", Style::default());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello ", "the text before the stub survives");

        // The stub alone, and a bare ESC at the very end, are the same shape.
        assert!(ansi_line_to_spans("\x1b[", Style::default()).is_empty());
        let tail: String = ansi_line_to_spans("x\x1b", Style::default())
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(tail, "x\x1b", "a lone ESC is not a sequence, so it is content");

        // And a well-formed sequence still splits and styles as before.
        let ok = ansi_line_to_spans("a\x1b[31mb\x1b[0mc", Style::default());
        let joined: String = ok.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "abc");
        assert_eq!(ok.len(), 3);
    }

    #[test]
    fn highlighting_survives_characters_whose_lowercase_is_a_different_length() {
        let theme = Theme::midnight();
        let text_of = |l: &Line<'static>| -> String {
            l.spans.iter().map(|s| s.content.as_ref()).collect()
        };

        // U+212A KELVIN SIGN: three bytes in, one byte out. Offsets taken from
        // the lowercased copy used to land inside it.
        let out = highlight_line(
            Line::from(Span::raw("\u{212A} error here")),
            "error",
            false,
            &theme,
        );
        assert_eq!(text_of(&out), "\u{212A} error here", "no character is lost or doubled");

        // U+0130: two bytes in, three out — offsets ran off the end instead.
        let out = highlight_line(Line::from(Span::raw("\u{130}error")), "error", false, &theme);
        assert_eq!(text_of(&out), "\u{130}error");

        // A needle matching *inside* one character's expansion highlights that
        // character whole rather than half of it — and terminates.
        let out = highlight_line(Line::from(Span::raw("\u{130}x")), "i", false, &theme);
        assert_eq!(text_of(&out), "\u{130}x");
    }

    #[test]
    fn highlighting_still_marks_the_matches_it_is_there_to_mark() {
        let theme = Theme::midnight();
        let out = highlight_line(
            Line::from(Span::raw("Error: an ERROR and an error")),
            "error",
            false,
            &theme,
        );
        let joined: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "Error: an ERROR and an error", "text is preserved exactly");
        // Three hits, case-insensitively, each carrying the hit background.
        let hits: Vec<&str> = out
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(theme.accent_dim))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hits, vec!["Error", "ERROR", "error"]);

        // No match: one span back, unstyled.
        let miss = highlight_line(Line::from(Span::raw("all quiet")), "error", false, &theme);
        assert_eq!(miss.spans.len(), 1);
        assert!(miss.spans[0].style.bg.is_none());
    }

    #[test]
    fn a_log_longer_than_a_u16_can_count_still_renders() {
        // These indices were u16. Past 65_535 the counter wrapped — silently in
        // release, where overflow checks are off — and every group header, fold
        // marker and minimap band was keyed 65_536 rows from where it was drawn.
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        const N: usize = 70_000;
        let mut lines: Vec<String> = (0..N).map(|i| format!("line {i}")).collect();
        lines.push("##[group]late group".into());
        lines.push("##[error]boom".into());
        lines.push("##[endgroup]".into());
        st.log_lines = lines;
        st.last_logs_viewport_height.set(40);
        st.last_logs_viewport_width.set(100);
        st.init_log_groups();
        st.recompute_log_rendered();

        assert!(st.log_rendered.len() > u16::MAX as usize);
        assert_eq!(st.log_rendered.len(), st.log_rendered_src.len());

        // The group past the old ceiling is keyed where it is actually drawn.
        let header_row = st.log_group_header_rows[0];
        assert!(header_row > u16::MAX as usize, "got {header_row}");
        assert_eq!(st.log_rendered_group_map.get(&header_row), Some(&0));
        assert_eq!(st.log_rendered_src[header_row], N);

        // And the cursor can still reach the end of it.
        st.log_line_cursor = st.log_rendered.len() - 1;
        st.keep_cursor_visible();
        assert!(st.log_scroll > u16::MAX as usize);
        assert!(st.last_visible_row(st.log_scroll) >= st.log_line_cursor);
    }

    #[test]
    fn the_hold_after_a_refusal_doubles_and_forgets() {
        let mut st = AppState::new(
            "o/r".into(),
            "main".into(),
            Vec::new(),
            KeymapConfig::default(),
            History::default(),
        );
        assert!(!st.api_held(), "nothing has been refused yet");

        // Eight rows fail on the same tick and all eight say so — the wait is
        // one wait, not eight compounded.
        st.hold_api(None);
        let first = st.api_hold_left().unwrap();
        for _ in 0..7 {
            st.hold_api(None);
        }
        assert!((st.api_hold_left().unwrap() - first).abs() <= 1);

        // The next round of refusals is a longer wait: the first one plainly
        // was not enough.
        st.api_paused_until = None;
        st.hold_api(None);
        assert!(st.api_hold_left().unwrap() > first);

        // …and a poll that gets through puts it back to nothing, so an hour
        // later a single hiccup does not start at two minutes.
        st.clear_api_hold();
        assert!(!st.api_held());
        st.hold_api(None);
        assert!((st.api_hold_left().unwrap() - first).abs() <= 1);

        // When GitHub has named the moment — the hourly quota's reset — that
        // beats any guess the ladder could make.
        let reset = Utc::now() + chrono::Duration::minutes(20);
        st.hold_api(Some(reset));
        assert!(st.api_hold_left().unwrap() > 60 * 19);
        // A reset already in the past is no hold at all.
        st.clear_api_hold();
        st.hold_api(Some(Utc::now() - chrono::Duration::minutes(1)));
        assert!(st.api_held(), "an expired reset falls back to the ladder");
    }
}
