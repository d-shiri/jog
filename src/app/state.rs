use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use ratatui::text::{Line, Span};
use ratatui::style::{Color, Modifier, Style, Stylize};

use crate::config::KeymapConfig;
use crate::history::History;
use crate::provider::{Run, RunDetail, Workflow};


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
    Workflows,
    Runs,
    RunDetail,
    Logs,
    Watch,
    TriggerPrompt,
    Diff,
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
    /// (step_name, started_hms, completed_hms) stored when navigating into logs from a specific step.
    /// started/completed are "HH:MM:SS" strings derived from the GitHub API step timestamps.
    pub log_pending_section: Option<(String, Option<String>, Option<String>)>,
    pub log_scroll: u16,
    /// Inner viewport height of the Logs pane, captured at last render.
    /// Used to clamp `log_scroll` so users can't scroll past the bottom.
    /// Cell so render can write through `&AppState`.
    pub last_logs_viewport_height: Cell<u16>,
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
            log_pending_section: None,
            log_scroll: 0,
            last_logs_viewport_height: Cell::new(0),
            log_search_input: None,
            log_search_query: None,
            log_search_matches: Vec::new(),
            log_search_match_idx: None,
            status_msg: None,
            status_msg_tick: 0,
            repo_label,
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

    pub fn current_step_idx(&self) -> Option<usize> {
        if self.log_step_names.is_empty() { None } else { self.log_section_idx }
    }

    pub fn selected_workflow(&self) -> Option<&Workflow> {
        self.workflows.get(self.workflow_cursor)
    }

    pub fn selected_run(&self) -> Option<&Run> {
        self.runs.get(self.run_cursor)
    }

    pub fn init_log_groups(&mut self) {
        self.log_groups = parse_log_groups(&self.log_lines);
        self.log_collapsed = (0..self.log_groups.len()).collect();
        self.log_line_cursor = 0;
        self.log_group_header_rows = vec![0u16; self.log_groups.len()];
        self.log_rendered_group_map = HashMap::new();
    }

    pub fn compute_hidden_lines(&self) -> HashSet<usize> {
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
        for (i, line) in self.log_lines.iter().enumerate() {
            let hay = visible_text(line).to_lowercase();
            if hay.contains(&needle) {
                self.log_search_matches.push(i);
            }
        }
        if !self.log_search_matches.is_empty() {
            self.log_search_match_idx = Some(0);
        }
    }

    pub fn recompute_log_rendered(&mut self) {
        let needle_lower = self
            .log_search_query
            .as_deref()
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase());
        let current_match_line = self
            .log_search_match_idx
            .and_then(|i| self.log_search_matches.get(i).copied());

        let time_style = Style::default().fg(Color::Rgb(80, 80, 80));

        let header_to_group: HashMap<usize, usize> = self.log_groups.iter().enumerate()
            .map(|(gi, g)| (g.header_line, gi))
            .collect();

        let hidden = self.compute_hidden_lines();
        let cursor = self.log_line_cursor;
        let cursor_bg = Style::default().bg(Color::Rgb(35, 42, 58));

        let mut rendered: Vec<Line<'static>> = Vec::new();
        let mut group_header_rows = vec![0u16; self.log_groups.len()];
        let mut group_map: HashMap<u16, usize> = HashMap::new();
        let mut rendered_row: u16 = 0;

        for (src_idx, l) in self.log_lines.iter().enumerate() {
            if hidden.contains(&src_idx) {
                continue;
            }

            let (time, content) = split_time_prefix(l.as_str());
            let mk_time = || time.map(|t| Span::styled(format!("{t} "), time_style));

            if content.starts_with("##[endgroup]") {
                continue;
            }

            let mut line: Line = if let Some(&gi) = header_to_group.get(&src_idx) {
                group_header_rows[gi] = rendered_row;
                group_map.insert(rendered_row, gi);
                let is_collapsed = self.log_collapsed.contains(&gi);
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

            if rendered_row == cursor {
                line = line.style(cursor_bg);
            }

            if let Some(needle) = needle_lower.as_deref() {
                let is_current = current_match_line == Some(src_idx);
                rendered.push(highlight_line(line, needle, is_current));
            } else {
                rendered.push(line);
            }
            rendered_row += 1;
        }

        self.log_rendered = rendered;
        self.log_group_header_rows = group_header_rows;
        self.log_rendered_group_map = group_map;
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
