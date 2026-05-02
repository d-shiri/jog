use std::cell::Cell;

use crate::config::KeymapConfig;
use crate::history::History;
use crate::provider::{Run, RunDetail, Workflow};


#[derive(Debug, Clone, Copy)]
pub enum DetailItem {
    Job(usize),
    Step { job: usize, step: usize },
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
        if let Some(f) = self.current_field_mut() {
            if let Some(opts) = f.options.clone() {
                if opts.is_empty() {
                    return;
                }
                let idx = opts.iter().position(|o| o == &f.value).unwrap_or(0);
                f.value = opts[(idx + 1) % opts.len()].clone();
            }
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
    /// (step_name, step_number) stored when navigating into logs from a specific step.
    /// step_number is the GitHub API number (1-based, may have gaps for internal sub-steps).
    pub log_pending_section: Option<(String, i64)>,
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
    pub repo_label: String,
    pub current_branch: String,
    pub workflow_for_runs: Option<String>,
    /// Preview pane in Workflows view: recent runs for the highlighted workflow.
    pub workflow_preview_file: Option<String>,
    pub workflow_preview_runs: Vec<Run>,
    /// Preview pane in Runs view: detail for the highlighted run.
    pub runs_preview: Option<RunDetail>,
    pub runs_preview_id: Option<u64>,
    /// Pending async work indicator (count of in-flight tasks)
    pub pending: usize,
    /// Set true when transitioning views so the event loop can `terminal.clear()`.
    pub needs_clear: bool,
    pub trigger_prompt: Option<TriggerPrompt>,
    pub keymap: KeymapConfig,
    pub history: History,
}

impl AppState {
    pub fn new(repo_label: String, current_branch: String, workflows: Vec<Workflow>, keymap: KeymapConfig, history: History) -> Self {
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
            log_pending_section: None,
            log_scroll: 0,
            last_logs_viewport_height: Cell::new(0),
            log_search_input: None,
            log_search_query: None,
            log_search_matches: Vec::new(),
            log_search_match_idx: None,
            status_msg: None,
            repo_label,
            current_branch,
            workflow_for_runs: None,
            workflow_preview_file: None,
            workflow_preview_runs: Vec::new(),
            runs_preview: None,
            runs_preview_id: None,
            pending: 0,
            needs_clear: false,
            trigger_prompt: None,
            keymap,
            history,
        }
    }

    pub fn switch_view(&mut self, v: View) {
        if self.view != v {
            self.view = v;
            self.needs_clear = true;
        }
    }

    pub fn selected_workflow(&self) -> Option<&Workflow> {
        self.workflows.get(self.workflow_cursor)
    }

    pub fn selected_run(&self) -> Option<&Run> {
        self.runs.get(self.run_cursor)
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
