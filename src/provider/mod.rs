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
