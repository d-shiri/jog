use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::provider::{RunDetail, Status};

const CAP: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub run_id: u64,
    pub workflow_file: String,
    pub status: Status,
    pub created_at: DateTime<Utc>,
    pub jobs: Vec<HistoryJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryJob {
    pub name: String,
    pub status: Status,
    pub steps: Vec<HistoryStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryStep {
    pub name: String,
    pub status: Status,
}

/// What actually sits in the history file: the run log, plus the inputs the
/// user last dispatched each workflow with. One file rather than two because
/// they live and die together — both are per-repo memory of what CI did.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    entries: Vec<HistoryEntry>,
    #[serde(default)]
    dispatch_inputs: HashMap<String, HashMap<String, String>>,
}

impl HistoryFile {
    /// Files written before dispatch inputs existed were a bare entry array.
    /// Read both shapes; write only the new one.
    fn parse(raw: &str) -> Self {
        serde_json::from_str::<HistoryFile>(raw)
            .or_else(|_| {
                serde_json::from_str::<Vec<HistoryEntry>>(raw).map(|entries| HistoryFile {
                    entries,
                    dispatch_inputs: HashMap::new(),
                })
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub struct History {
    path: Option<PathBuf>,
    entries: Vec<HistoryEntry>,
    dispatch_inputs: HashMap<String, HashMap<String, String>>,
}

impl History {
    pub fn load_for_repo(owner: &str, repo: &str) -> Self {
        let Some(path) = repo_history_path(owner, repo) else {
            return Self::default();
        };
        let file = std::fs::read_to_string(&path)
            .ok()
            .map(|raw| HistoryFile::parse(&raw))
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries: file.entries,
            dispatch_inputs: file.dispatch_inputs,
        }
    }

    /// Insert or replace by run_id, then trim to CAP newest entries (by created_at).
    pub fn record(&mut self, workflow_file: &str, detail: &RunDetail) {
        // Skip non-terminal runs — their step statuses are still moving.
        if !detail.run.status.is_terminal() {
            return;
        }
        let entry = HistoryEntry {
            run_id: detail.run.id,
            workflow_file: workflow_file.to_string(),
            status: detail.run.status,
            created_at: detail.run.created_at,
            jobs: detail
                .jobs
                .iter()
                .map(|j| HistoryJob {
                    name: j.name.clone(),
                    status: j.status,
                    steps: j
                        .steps
                        .iter()
                        .map(|s| HistoryStep {
                            name: s.name.clone(),
                            status: s.status,
                        })
                        .collect(),
                })
                .collect(),
        };
        if let Some(slot) = self.entries.iter_mut().find(|e| e.run_id == entry.run_id) {
            *slot = entry;
        } else {
            self.entries.push(entry);
        }
        self.entries
            .sort_by_key(|b| std::cmp::Reverse(b.created_at));
        if self.entries.len() > CAP {
            self.entries.truncate(CAP);
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = HistoryFile {
            entries: self.entries.clone(),
            dispatch_inputs: self.dispatch_inputs.clone(),
        };
        if let Ok(json) = serde_json::to_string(&file) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Remember what a workflow was dispatched with, so the next trigger prompt
    /// starts from the values that were typed last time instead of the YAML
    /// defaults. Overwrite rather than merge: the last dispatch is the whole
    /// answer to "what did I use", including fields cleared back to empty.
    pub fn record_dispatch_inputs(
        &mut self,
        workflow_file: &str,
        inputs: &HashMap<String, String>,
    ) {
        self.dispatch_inputs
            .insert(workflow_file.to_string(), inputs.clone());
        self.save();
    }

    pub fn last_dispatch_inputs(
        &self,
        workflow_file: &str,
    ) -> Option<&HashMap<String, String>> {
        self.dispatch_inputs.get(workflow_file)
    }

    /// (failed_count, terminal_count) per step name for the last `n` terminal runs
    /// of the given workflow. Steps are keyed by name; cancelled/skipped runs
    /// are excluded from the denominator since they don't reflect step health.
    pub fn step_failure_stats(
        &self,
        workflow_file: &str,
        n: usize,
    ) -> HashMap<String, (u32, u32)> {
        let mut out: HashMap<String, (u32, u32)> = HashMap::new();
        for entry in self
            .entries
            .iter()
            .filter(|e| e.workflow_file == workflow_file)
            .filter(|e| matches!(e.status, Status::Success | Status::Failure))
            .take(n)
        {
            for job in &entry.jobs {
                for step in &job.steps {
                    let slot = out.entry(step.name.clone()).or_insert((0, 0));
                    slot.1 += 1;
                    if step.status == Status::Failure {
                        slot.0 += 1;
                    }
                }
            }
        }
        out
    }

    /// Most recent run (any status) for the given workflow.
    pub fn last_run(&self, workflow_file: &str) -> Option<&HistoryEntry> {
        self.entries.iter().find(|e| e.workflow_file == workflow_file)
    }

    /// Most recent successful run for the given workflow.
    pub fn last_successful(&self, workflow_file: &str) -> Option<&HistoryEntry> {
        self.entries
            .iter()
            .find(|e| e.workflow_file == workflow_file && e.status == Status::Success)
    }
}

fn repo_history_path(owner: &str, repo: &str) -> Option<PathBuf> {
    let dir = dirs::cache_dir()?.join("jog").join("history");
    Some(dir.join(format!("{}__{}.json", sanitize(owner), sanitize(repo))))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Job, Run, Step};
    use chrono::TimeZone;

    fn run(id: u64, status: Status, h: u32) -> Run {
        Run {
            id,
            display_title: "x".into(),
            head_branch: "main".into(),
            commit_msg: String::new(),
            status,
            created_at: Utc.with_ymd_and_hms(2026, 5, 1, h, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 5, 1, h, 1, 0).unwrap(),
            url: String::new(),
            workflow_file: None,
        }
    }

    fn detail(id: u64, status: Status, h: u32, steps: &[(&str, Status)]) -> RunDetail {
        RunDetail {
            run: run(id, status, h),
            jobs: vec![Job {
                id: 1,
                name: "job".into(),
                status,
                steps: steps
                    .iter()
                    .enumerate()
                    .map(|(_, (n, s))| Step {
                        name: (*n).into(),
                        status: *s,
                        started_at: None,
                        completed_at: None,
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn files_from_before_dispatch_inputs_existed_still_parse() {
        let mut h = History::default();
        h.record("a.yml", &detail(1, Status::Success, 1, &[("build", Status::Success)]));
        // The old format was the bare entry array.
        let legacy = serde_json::to_string(&h.entries).unwrap();
        let parsed = HistoryFile::parse(&legacy);
        assert_eq!(parsed.entries.len(), 1);
        assert!(parsed.dispatch_inputs.is_empty());
    }

    #[test]
    fn dispatch_inputs_survive_the_round_trip() {
        let mut h = History::default();
        let inputs: HashMap<String, String> =
            [("env".to_string(), "prod".to_string())].into_iter().collect();
        h.record_dispatch_inputs("deploy.yml", &inputs);
        assert_eq!(
            h.last_dispatch_inputs("deploy.yml").and_then(|m| m.get("env")),
            Some(&"prod".to_string())
        );
        // And through the file format.
        let file = HistoryFile {
            entries: h.entries.clone(),
            dispatch_inputs: h.dispatch_inputs.clone(),
        };
        let parsed = HistoryFile::parse(&serde_json::to_string(&file).unwrap());
        assert_eq!(
            parsed.dispatch_inputs.get("deploy.yml").and_then(|m| m.get("env")),
            Some(&"prod".to_string())
        );
    }

    #[test]
    fn step_failure_stats_only_count_terminal_runs() {
        let mut h = History::default();
        // Running run is ignored.
        h.record("a.yml", &detail(1, Status::Running, 1, &[("test", Status::Running)]));
        // Cancelled is excluded from denominator (intentional aborts ≠ flakiness).
        h.record("a.yml", &detail(2, Status::Cancelled, 2, &[("test", Status::Cancelled)]));
        h.record("a.yml", &detail(3, Status::Success, 3, &[("test", Status::Success)]));
        h.record("a.yml", &detail(4, Status::Failure, 4, &[("test", Status::Failure)]));
        h.record("a.yml", &detail(5, Status::Failure, 5, &[("test", Status::Failure)]));
        let stats = h.step_failure_stats("a.yml", 10);
        assert_eq!(stats.get("test").copied(), Some((2, 3)));
    }

    #[test]
    fn last_successful_picks_most_recent() {
        let mut h = History::default();
        h.record("a.yml", &detail(1, Status::Success, 1, &[]));
        h.record("a.yml", &detail(2, Status::Failure, 2, &[]));
        h.record("a.yml", &detail(3, Status::Success, 3, &[]));
        assert_eq!(h.last_successful("a.yml").map(|e| e.run_id), Some(3));
    }

    #[test]
    fn other_workflow_isolated() {
        let mut h = History::default();
        h.record("a.yml", &detail(1, Status::Failure, 1, &[("test", Status::Failure)]));
        h.record("b.yml", &detail(2, Status::Success, 2, &[("test", Status::Success)]));
        let a = h.step_failure_stats("a.yml", 10);
        let b = h.step_failure_stats("b.yml", 10);
        assert_eq!(a.get("test").copied(), Some((1, 1)));
        assert_eq!(b.get("test").copied(), Some((0, 1)));
    }

    #[test]
    fn record_replaces_same_run_id() {
        let mut h = History::default();
        h.record("a.yml", &detail(1, Status::Failure, 1, &[("t", Status::Failure)]));
        h.record("a.yml", &detail(1, Status::Success, 1, &[("t", Status::Success)]));
        assert_eq!(h.entries.len(), 1);
        assert_eq!(h.entries[0].status, Status::Success);
    }
}

