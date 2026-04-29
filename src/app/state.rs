use crate::provider::{Run, RunDetail, Workflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Workflows,
    Runs,
    RunDetail,
    Logs,
    Watch,
}

#[derive(Debug)]
pub struct AppState {
    pub view: View,
    pub workflows: Vec<Workflow>,
    pub workflow_cursor: usize,
    pub runs: Vec<Run>,
    pub run_cursor: usize,
    pub run_detail: Option<RunDetail>,
    pub job_cursor: usize,
    pub log_lines: Vec<String>,
    pub log_scroll: u16,
    pub status_msg: Option<String>,
    pub repo_label: String,
    pub current_branch: String,
    pub workflow_for_runs: Option<String>,
    /// Pending async work indicator (count of in-flight tasks)
    pub pending: usize,
    /// Set true when transitioning views so the event loop can `terminal.clear()`.
    pub needs_clear: bool,
}

impl AppState {
    pub fn new(repo_label: String, current_branch: String, workflows: Vec<Workflow>) -> Self {
        Self {
            view: View::Workflows,
            workflows,
            workflow_cursor: 0,
            runs: Vec::new(),
            run_cursor: 0,
            run_detail: None,
            job_cursor: 0,
            log_lines: Vec::new(),
            log_scroll: 0,
            status_msg: None,
            repo_label,
            current_branch,
            workflow_for_runs: None,
            pending: 0,
            needs_clear: false,
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
}
