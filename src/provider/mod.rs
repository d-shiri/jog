use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod discovery;
pub mod github;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
    Skipped,
    Unknown,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Status::Success | Status::Failure | Status::Cancelled | Status::Skipped
        )
    }

    /// Terminal states that indicate the run did not succeed.
    pub fn is_failure(self) -> bool {
        matches!(self, Status::Failure | Status::Cancelled)
    }
}

/// Names coming in from outside (workflow titles, job and step names, commit
/// messages) pass through here before anything measures or draws them.
///
/// The problem it solves: a pictograph like 🛠 (U+1F6E0) defaults to *text*
/// presentation, so unicode-width — the same crate ratatui budgets cells
/// with — counts it as one column, while every terminal font draws it two
/// wide. From that emoji to the end of the row, the screen is one cell right
/// of where ratatui believes it is, and incremental redraws then land one
/// cell off inside previously drawn text: colons overwritten by digits, stale
/// glyphs never cleared. Appending VS16 (U+FE0F) asks for emoji presentation,
/// which both unicode-width and the terminal agree is two columns.
///
/// Only pictographic-plane characters that measure one column get the
/// treatment, and only when the crate actually counts the VS16 pair as two —
/// checked by measuring, so this can never make the mismatch worse. A VS16
/// already present is kept; a VS15 (text presentation, deliberately narrow)
/// is respected by leaving its base character alone.
pub fn emoji_width_safe(s: &str) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        let followed_by_selector =
            matches!(chars.peek(), Some('\u{FE0E}') | Some('\u{FE0F}'));
        if !followed_by_selector
            && ('\u{1F000}'..='\u{1FAFF}').contains(&c)
            && UnicodeWidthChar::width(c) == Some(1)
        {
            let mut pair = c.to_string();
            pair.push('\u{FE0F}');
            if UnicodeWidthStr::width(pair.as_str()) == 2 {
                out.push('\u{FE0F}');
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: String,
    pub file_name: String,
    pub triggerable: bool,
    pub last_status: Option<Status>,
    pub last_run_at: Option<DateTime<Utc>>,
    /// Inputs declared under `on.workflow_dispatch.inputs`.
    pub inputs: Vec<WorkflowInput>,
}

#[derive(Debug, Clone)]
pub struct WorkflowInput {
    pub name: String,
    pub required: bool,
    pub default: Option<String>,
    /// Allowed values for `type: choice` inputs. None for free-form text.
    pub options: Option<Vec<String>>,
}

impl Workflow {
    /// Inputs that have to be supplied by the user (no default).
    pub fn missing_required_inputs(&self) -> Vec<&str> {
        self.inputs
            .iter()
            .filter(|i| i.required && i.default.is_none())
            .map(|i| i.name.as_str())
            .collect()
    }

    /// Apply YAML defaults on top of user-supplied inputs.
    pub fn merge_defaults(
        &self,
        user: HashMap<String, String>,
    ) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for inp in &self.inputs {
            if let Some(d) = &inp.default {
                out.insert(inp.name.clone(), d.clone());
            }
        }
        for (k, v) in user {
            out.insert(k, v);
        }
        out
    }
}

/// The open pull request for a branch — as much of it as the git view needs to
/// say "this branch is already on review" and to open it in a browser.
#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub draft: bool,
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: u64,
    pub display_title: String,
    pub head_branch: String,
    pub commit_msg: String,
    pub status: Status,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub url: String,
    /// The filename of the workflow (e.g. `ci.yml`). Optional because some
    /// API endpoints (like "list all runs for repo") might include it
    /// while others (list runs for a specific workflow) already imply it.
    pub workflow_file: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub status: Status,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub status: Status,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct RunDetail {
    pub run: Run,
    pub jobs: Vec<Job>,
}

impl RunDetail {
    pub fn current_step(&self) -> Option<&str> {
        self.jobs
            .iter()
            .filter(|j| j.status == Status::Running)
            .flat_map(|j| j.steps.iter())
            .find(|s| s.status == Status::Running)
            .map(|s| s.name.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct LogChunk {
    pub line: String,
}

pub type LogStream = BoxStream<'static, Result<LogChunk>>;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Workflows as the remote knows them. Used for repos we have no local
    /// checkout of (the multi-repo dashboard); the local repo prefers
    /// `discovery::discover_workflows`, which is a filesystem read.
    async fn list_workflows(&self) -> Result<Vec<Workflow>>;
    async fn list_runs(&self, workflow_file: &str, limit: u8) -> Result<Vec<Run>>;
    async fn list_repo_runs(&self, limit: u8) -> Result<Vec<Run>>;
    async fn get_latest_run(&self, workflow_file: &str) -> Result<Option<Run>>;
    async fn get_run(&self, id: u64) -> Result<RunDetail>;
    async fn stream_logs(&self, job_id: u64) -> Result<LogStream>;
    async fn trigger(
        &self,
        workflow_file: &str,
        reference: &str,
        inputs: HashMap<String, String>,
    ) -> Result<()>;
    async fn cancel(&self, run_id: u64) -> Result<()>;
    async fn rerun(&self, run_id: u64) -> Result<()>;
    async fn rerun_failed(&self, run_id: u64) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::emoji_width_safe;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn text_presentation_pictographs_get_widened() {
        // 🛠 without VS16 measures one column but every terminal draws two —
        // the live-strip row it sat on rendered its ETA and clock as garbage.
        let fixed = emoji_width_safe("\u{1F6E0} CI for Backend");
        assert_eq!(fixed, "\u{1F6E0}\u{FE0F} CI for Backend");
        assert_eq!(UnicodeWidthStr::width(fixed.as_str()), 2 + 15);
    }

    #[test]
    fn already_wide_or_selected_emoji_pass_through() {
        // 🚧 and 💻 are emoji-presentation by default: two columns as-is.
        assert_eq!(emoji_width_safe("🚧 Deploy"), "🚧 Deploy");
        assert_eq!(emoji_width_safe("💻 Deploy"), "💻 Deploy");
        // An explicit VS16 is not doubled; an explicit VS15 is respected.
        assert_eq!(
            emoji_width_safe("\u{1F6E0}\u{FE0F} CI"),
            "\u{1F6E0}\u{FE0F} CI"
        );
        assert_eq!(
            emoji_width_safe("\u{1F6E0}\u{FE0E} CI"),
            "\u{1F6E0}\u{FE0E} CI"
        );
    }

    #[test]
    fn plain_text_is_untouched() {
        let s = "Run tests with pytest · main ~3:48 left";
        assert_eq!(emoji_width_safe(s), s);
    }
}
