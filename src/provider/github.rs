use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use octocrab::Octocrab;
use octocrab::models::workflows as gh_workflows;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use super::{Job, LogChunk, LogStream, Provider, Run, RunDetail, Status, Step};

#[derive(Debug, Clone)]
pub struct RepoSpec {
    pub owner: String,
    pub repo: String,
}

impl RepoSpec {
    pub fn parse(s: &str) -> Result<Self> {
        let (owner, repo) = s
            .split_once('/')
            .ok_or_else(|| anyhow!("repo must be owner/name, got `{s}`"))?;
        Ok(Self {
            owner: owner.trim().to_string(),
            repo: repo.trim().trim_end_matches(".git").to_string(),
        })
    }

    pub fn from_git_remote() -> Result<Self> {
        let out = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .output()
            .context("failed to invoke `git remote get-url origin`")?;
        if !out.status.success() {
            return Err(anyhow!("`git remote get-url origin` exited with non-zero"));
        }
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        parse_remote_url(&url)
    }
}

fn parse_remote_url(url: &str) -> Result<RepoSpec> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let path = if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        rest
    } else {
        return Err(anyhow!("unrecognized GitHub remote `{url}`"));
    };
    RepoSpec::parse(path)
}

pub fn resolve_token() -> Result<String> {
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let out = match Command::new("gh").args(["auth", "token"]).output() {
        Ok(o) => o,
        Err(_) => {
            return Err(anyhow!(
                "no GitHub credentials.\n  fix: set GITHUB_TOKEN, or install `gh` and run `gh auth login`"
            ));
        }
    };
    if !out.status.success() {
        return Err(anyhow!(
            "no GitHub credentials.\n  fix: run `gh auth login`, or export GITHUB_TOKEN=<your-token>"
        ));
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        return Err(anyhow!(
            "`gh auth token` returned empty.\n  fix: re-run `gh auth login`"
        ));
    }
    Ok(token)
}

pub fn current_branch() -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("failed to invoke git")?;
    if !out.status.success() {
        return Err(anyhow!("git rev-parse failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub struct GitHubProvider {
    crab: Arc<Octocrab>,
    repo: RepoSpec,
}

impl GitHubProvider {
    pub fn new(repo: RepoSpec, token: String) -> Result<Self> {
        let crab = Octocrab::builder()
            .personal_token(token)
            .build()
            .context("build octocrab client")?;
        Ok(Self {
            crab: Arc::new(crab),
            repo,
        })
    }

    pub fn repo(&self) -> &RepoSpec {
        &self.repo
    }
}

fn map_run(r: gh_workflows::Run) -> Run {
    let status = parse_run_status(&r.status, r.conclusion.as_deref());
    Run {
        id: r.id.0,
        display_title: r.name.clone(),
        head_branch: r.head_branch,
        status,
        created_at: r.created_at,
        updated_at: r.updated_at,
        url: r.html_url.to_string(),
    }
}

fn parse_run_status(status: &str, conclusion: Option<&str>) -> Status {
    match status {
        "queued" | "pending" | "waiting" | "requested" => Status::Queued,
        "in_progress" => Status::Running,
        "completed" => match conclusion.unwrap_or("") {
            "success" => Status::Success,
            "failure" | "timed_out" | "action_required" => Status::Failure,
            "cancelled" => Status::Cancelled,
            "skipped" | "neutral" => Status::Skipped,
            _ => Status::Unknown,
        },
        _ => Status::Unknown,
    }
}

fn map_job_status(s: &gh_workflows::Status, c: Option<&gh_workflows::Conclusion>) -> Status {
    match s {
        gh_workflows::Status::Queued | gh_workflows::Status::Pending => Status::Queued,
        gh_workflows::Status::InProgress => Status::Running,
        gh_workflows::Status::Failed => Status::Failure,
        gh_workflows::Status::Completed => match c {
            Some(gh_workflows::Conclusion::Success) => Status::Success,
            Some(gh_workflows::Conclusion::Failure)
            | Some(gh_workflows::Conclusion::TimedOut)
            | Some(gh_workflows::Conclusion::ActionRequired) => Status::Failure,
            Some(gh_workflows::Conclusion::Cancelled) => Status::Cancelled,
            Some(gh_workflows::Conclusion::Skipped) | Some(gh_workflows::Conclusion::Neutral) => {
                Status::Skipped
            }
            Some(_) | None => Status::Unknown,
        },
        _ => Status::Unknown,
    }
}

fn map_step(s: gh_workflows::Step) -> Step {
    Step {
        name: s.name,
        status: map_job_status(&s.status, s.conclusion.as_ref()),
        number: s.number,
    }
}

fn map_job(j: gh_workflows::Job) -> Job {
    let status = map_job_status(&j.status, j.conclusion.as_ref());
    Job {
        id: j.id.0,
        name: j.name,
        status,
        steps: j.steps.into_iter().map(map_step).collect(),
    }
}

#[async_trait]
impl Provider for GitHubProvider {
    async fn list_runs(&self, workflow_file: &str, limit: u8) -> Result<Vec<Run>> {
        let page = self
            .crab
            .workflows(&self.repo.owner, &self.repo.repo)
            .list_runs(workflow_file)
            .per_page(limit)
            .send()
            .await
            .context("list workflow runs")?;
        Ok(page.items.into_iter().map(map_run).collect())
    }

    async fn get_latest_run(&self, workflow_file: &str) -> Result<Option<Run>> {
        let mut runs = self.list_runs(workflow_file, 1).await?;
        Ok(runs.drain(..).next())
    }

    async fn get_run(&self, id: u64) -> Result<RunDetail> {
        let run_id = octocrab::models::RunId(id);
        let handler = self.crab.workflows(&self.repo.owner, &self.repo.repo);
        let (run, jobs_page) = tokio::try_join!(
            async { handler.get(run_id).await.context("get run") },
            async {
                handler
                    .list_jobs(run_id)
                    .per_page(50)
                    .send()
                    .await
                    .context("list jobs")
            },
        )?;
        let jobs = jobs_page.items.into_iter().map(map_job).collect();
        Ok(RunDetail {
            run: map_run(run),
            jobs,
        })
    }

    async fn stream_logs(&self, job_id: u64) -> Result<LogStream> {
        let route = format!(
            "/repos/{}/{}/actions/jobs/{}/logs",
            self.repo.owner, self.repo.repo, job_id
        );
        let resp = self.crab._get(route).await.context("fetch job logs")?;
        let resp = self
            .crab
            .follow_location_to_data(resp)
            .await
            .context("follow log redirect")?;
        let text = self
            .crab
            .body_to_string(resp)
            .await
            .context("read log body")?;
        let chunks: Vec<Result<LogChunk>> = text
            .lines()
            .map(clean_log_line)
            .map(|l| Ok(LogChunk { line: l }))
            .collect();
        Ok(stream::iter(chunks).boxed())
    }

    async fn trigger(
        &self,
        workflow_file: &str,
        reference: &str,
        inputs: HashMap<String, String>,
    ) -> Result<()> {
        let actions = self.crab.actions();
        let dispatch = actions.create_workflow_dispatch(
            &self.repo.owner,
            &self.repo.repo,
            workflow_file,
            reference,
        );
        let dispatch = if inputs.is_empty() {
            dispatch
        } else {
            let value = serde_json::to_value(inputs).context("serialize inputs")?;
            dispatch.inputs(value)
        };
        dispatch.send().await.context("dispatch workflow")?;
        Ok(())
    }

    async fn cancel(&self, run_id: u64) -> Result<()> {
        self.crab
            .actions()
            .cancel_workflow_run(&self.repo.owner, &self.repo.repo, run_id.into())
            .await
            .context("cancel workflow run")?;
        Ok(())
    }

    async fn rerun(&self, run_id: u64) -> Result<()> {
        let route = format!(
            "/repos/{}/{}/actions/runs/{}/rerun",
            self.repo.owner, self.repo.repo, run_id
        );
        let resp = self
            .crab
            ._post(route, None::<&()>)
            .await
            .context("rerun")?;
        if !resp.status().is_success() {
            return Err(octocrab::map_github_error(resp).await.unwrap_err().into());
        }
        Ok(())
    }

    async fn rerun_failed(&self, run_id: u64) -> Result<()> {
        let route = format!(
            "/repos/{}/{}/actions/runs/{}/rerun-failed-jobs",
            self.repo.owner, self.repo.repo, run_id
        );
        let resp = self
            .crab
            ._post(route, None::<&()>)
            .await
            .context("rerun-failed-jobs")?;
        if !resp.status().is_success() {
            return Err(octocrab::map_github_error(resp).await.unwrap_err().into());
        }
        Ok(())
    }
}

/// Clean a single GitHub Actions log line:
/// 1. Extract `HH:MM:SS` from the ISO timestamp and keep it as a short prefix
/// 2. Preserve ANSI escape sequences so the renderer can display colors
/// 3. Strip other control chars except `\t` (tab → 4 spaces) and `\x1b` (ESC for ANSI)
fn clean_log_line(raw: &str) -> String {
    let (time, content) = extract_time(raw);
    let mut out = String::new();
    if let Some(t) = time {
        out.push_str(t); // "HH:MM:SS"
        out.push(' ');
    }
    for c in content.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\n' | '\r' => {}
            '\x1b' => out.push(c), // keep ESC so ANSI color sequences reach the renderer
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// Returns `(Some("HH:MM:SS"), content)` when the line starts with a GitHub Actions
/// ISO timestamp (`2025-04-29T08:12:34.5678901Z `), otherwise `(None, whole_line)`.
fn extract_time(s: &str) -> (Option<&str>, &str) {
    if s.len() > 20
        && s.as_bytes().get(4) == Some(&b'-')
        && s.as_bytes().get(7) == Some(&b'-')
        && s.as_bytes().get(10) == Some(&b'T')
        && s.as_bytes().get(13) == Some(&b':')
        && s.as_bytes().get(16) == Some(&b':')
    {
        if let Some(idx) = s.find(' ') {
            return (Some(&s[11..19]), &s[idx + 1..]);
        }
    }
    (None, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_remote() {
        let r = parse_remote_url("https://github.com/foo/bar.git").unwrap();
        assert_eq!(r.owner, "foo");
        assert_eq!(r.repo, "bar");
    }

    #[test]
    fn parses_ssh_remote() {
        let r = parse_remote_url("git@github.com:foo/bar.git").unwrap();
        assert_eq!(r.owner, "foo");
        assert_eq!(r.repo, "bar");
    }

    #[test]
    fn extracts_time_and_keeps_ansi() {
        let raw = "2025-04-29T08:12:34.5678901Z \x1b[36;1mwith:\x1b[0m\r\n";
        // Time prefix kept; ANSI codes preserved; CR/LF stripped
        assert_eq!(clean_log_line(raw), "08:12:34 \x1b[36;1mwith:\x1b[0m");
    }

    #[test]
    fn keeps_plain_lines() {
        assert_eq!(clean_log_line("hello world"), "hello world");
    }

    #[test]
    fn extracts_time_plain() {
        let raw = "2025-04-29T08:12:34.5678901Z npm install";
        assert_eq!(clean_log_line(raw), "08:12:34 npm install");
    }
}
