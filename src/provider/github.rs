use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::StreamExt;
use octocrab::Octocrab;
use octocrab::models::workflows as gh_workflows;
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

use super::{Job, LogChunk, LogStream, Provider, Run, RunDetail, Status, Step, Workflow};

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

pub fn parse_remote_url(url: &str) -> Result<RepoSpec> {
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
    if let Ok(t) = std::env::var("GITHUB_TOKEN")
        && !t.is_empty() {
            return Ok(t);
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

/// Why an API call failed, in the only terms a dashboard row has room for.
///
/// Sorting a failure into a kind first is what lets the row stay two words wide
/// while the panel header spells the same thing out with the fix attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFault {
    /// Quota spent. Nothing to fix — the clock fixes it.
    RateLimited,
    /// GitHub's *secondary* limit: too many requests too close together. Not
    /// the hourly budget — that can be barely touched while this one refuses
    /// every call — and it clears in a minute or two rather than at the hour.
    Throttled,
    /// Token missing, expired, or revoked.
    BadCredentials,
    /// Authenticated but not allowed: missing scope, or SSO not authorized.
    Denied,
    /// No such repo — or none this token is allowed to see.
    NotFound,
    /// GitHub is down, or we are.
    Unreachable,
    /// Anything else. Carries its own wording instead of a canned one.
    Other,
}

impl ApiFault {
    /// Two words: what fits in a table cell.
    pub fn label(self) -> &'static str {
        match self {
            Self::RateLimited => "rate limited",
            Self::Throttled => "throttled",
            Self::BadCredentials => "bad credentials",
            Self::Denied => "access denied",
            Self::NotFound => "not found",
            Self::Unreachable => "github unreachable",
            Self::Other => "failed",
        }
    }

    /// The same failure with the fix attached, for somewhere with room for it.
    pub fn detail(self) -> &'static str {
        match self {
            Self::RateLimited => "GitHub API quota spent",
            Self::Throttled => "asked too fast — backing off",
            Self::BadCredentials => "run `gh auth login`, or set GITHUB_TOKEN",
            Self::Denied => "token lacks access — check its scopes and SSO",
            Self::NotFound => "no such repo, or the token can't see it",
            Self::Unreachable => "GitHub or the network is not answering",
            Self::Other => "",
        }
    }
}

/// What is left of the token's hourly API budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    pub limit: u32,
    pub used: u32,
    /// When `used` goes back to zero.
    pub reset: DateTime<Utc>,
}

impl Quota {
    /// Share of the hour's budget already spent, 0.0–1.0.
    pub fn spent(&self) -> f64 {
        if self.limit == 0 {
            return 0.0;
        }
        (self.used as f64 / self.limit as f64).clamp(0.0, 1.0)
    }

    /// The same as whole percent, which is what the header shows.
    pub fn percent(&self) -> u32 {
        (self.spent() * 100.0).round() as u32
    }

    /// Close enough to the ceiling that the next few polls could hit it.
    ///
    /// The threshold exists to be acted on: at nine tenths spent the useful
    /// move is to quit jog and stop feeding the meter, which is only possible
    /// if you are told before the wall rather than at it.
    pub fn is_critical(&self) -> bool {
        self.percent() >= CRITICAL_PERCENT
    }
}

/// Where the quota readout turns red and the alarm sounds.
pub const CRITICAL_PERCENT: u32 = 90;

/// A failed API call, reduced to something a row can show.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub fault: ApiFault,
    /// What the row prints. Short by construction.
    pub text: String,
}

/// Sort an API failure into a kind and word it for a narrow column.
///
/// anyhow renders a chain outermost-first, so `{e:#}` in a table cell shows the
/// call we made — `list all repo runs: GitHub: API rate limit exceed…` — and
/// truncates away the answer. The row already names the repo and the column
/// already means "latest run", so the call is the one part worth dropping.
pub fn classify_error(err: &anyhow::Error) -> ApiError {
    if let Some(gh) = err.chain().find_map(|c| c.downcast_ref::<octocrab::Error>()) {
        return classify_octocrab(gh);
    }
    // Not an API failure at all (unparseable remote, git, io). The innermost
    // cause is the specific one; the layers above only say where it surfaced.
    let root = err.chain().last().map(|c| c.to_string()).unwrap_or_default();
    ApiError {
        fault: ApiFault::Other,
        text: first_line(&root, "failed"),
    }
}

fn classify_octocrab(err: &octocrab::Error) -> ApiError {
    let (fault, message) = match err {
        octocrab::Error::GitHub { source, .. } => (
            fault_for(source.status_code.as_u16(), &source.message),
            source.message.clone(),
        ),
        // Nothing came back at all: DNS, TLS, a dropped socket.
        octocrab::Error::Http { .. }
        | octocrab::Error::Hyper { .. }
        | octocrab::Error::Service { .. }
        | octocrab::Error::Uri { .. } => (ApiFault::Unreachable, String::new()),
        other => (ApiFault::Other, other.to_string()),
    };
    let text = match fault {
        ApiFault::Other => first_line(&message, "failed"),
        f => f.label().to_string(),
    };
    ApiError { fault, text }
}

/// What a status line and an error body add up to.
///
/// GitHub answers both quota kinds with 403 and only the prose tells them
/// apart: "API rate limit exceeded" for the hourly budget, "You have exceeded a
/// secondary rate limit" for bursts. Both are wait-it-out, and neither is the
/// permissions problem the bare status code would suggest.
///
/// 429 needs no prose to be read: it is the rate-limit status and nothing else.
/// Sorting it under `Denied` sent the user off to check token scopes and SSO for
/// something only the clock can fix — so it is decided by its number, while 403
/// still has to be read, because for 403 the number genuinely is ambiguous.
///
/// The two quota kinds are told apart rather than merged, because they lead
/// different places: the hourly budget is answered by the meter in the header
/// and waits out the hour, while the secondary one fires with the meter at 4%
/// and clears in a minute. Calling the second one "rate limited" next to a
/// header saying 4% spent reads as a bug in jog — it wasn't, it was two
/// different limits wearing one word.
fn fault_for(status: u16, message: &str) -> ApiFault {
    let lower = message.to_lowercase();
    // GitHub's own wording for the pace limit, in both of its phrasings.
    if lower.contains("secondary rate limit") || lower.contains("abuse detection") {
        return ApiFault::Throttled;
    }
    if lower.contains("rate limit") {
        return ApiFault::RateLimited;
    }
    match status {
        401 => ApiFault::BadCredentials,
        429 => ApiFault::RateLimited,
        403 => ApiFault::Denied,
        404 => ApiFault::NotFound,
        500..=599 => ApiFault::Unreachable,
        _ => ApiFault::Other,
    }
}

/// First line of `s`, or `fallback` when there isn't one worth showing.
///
/// GitHub errors carry a paragraph — the request id, a support address, a link
/// to the terms of service. None of it survives a table cell, and the first
/// sentence is the only part that ever said anything.
fn first_line(s: &str, fallback: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        fallback.to_string()
    } else {
        line.to_string()
    }
}

/// How many GitHub requests jog will have in the air at once.
///
/// A dashboard poll fans out over every repo at the same instant, and each row
/// with CI going pulls its jobs on top of that — eight repos is a burst of
/// seventy simultaneous requests, which is what trips the secondary limit while
/// the hourly meter still reads single digits. The cap turns the burst into a
/// trickle; it costs a fraction of a second on the poll and buys back a
/// dashboard that isn't refusing to load.
const MAX_INFLIGHT: usize = 4;

static API_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_INFLIGHT);

/// Run `f` with one of the in-flight slots held.
async fn gated<T>(f: impl std::future::Future<Output = T>) -> T {
    let _permit = API_GATE.acquire().await;
    f.await
}

/// The budget GitHub stamped on one answer, from its `X-RateLimit-*` headers.
///
/// Every response carries them, 304s included, and they describe the very
/// bucket the request was counted against — which `/rate_limit` does not
/// always do. Seen on live github.com: `GET /rate_limit` answering
/// `core: used 0, remaining 5000` with a reset an hour out, while calls made
/// on the same token seconds apart came back `x-ratelimit-used: 29` and `30`
/// against a window already running. A meter fed from the endpoint sat at 0%
/// all session; one fed from these headers tracks what was actually spent.
///
/// `None` unless the headers are all there and describe the core bucket — the
/// per-route buckets (`search`, `graphql`) are somebody else's budget, and a
/// partial set is not a reading.
fn quota_from_headers(headers: &http::HeaderMap) -> Option<Quota> {
    let text = |name: &str| headers.get(name)?.to_str().ok();
    if text("x-ratelimit-resource").is_some_and(|r| r != "core") {
        return None;
    }
    let num = |name: &str| text(name)?.trim().parse::<u64>().ok();
    Some(Quota {
        limit: num("x-ratelimit-limit")? as u32,
        used: num("x-ratelimit-used")? as u32,
        reset: DateTime::from_timestamp(num("x-ratelimit-reset")? as i64, 0)?,
    })
}

/// The ETag cache, poison notwithstanding: it holds nothing that can't be
/// re-fetched, so a panic elsewhere must not turn every later poll into an
/// unwrap panic of its own.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one conditional fetch left behind: the validator GitHub handed us and
/// the answer it validates. A 304 replays `runs`; anything else replaces the
/// whole entry.
struct CachedRuns {
    etag: String,
    runs: Vec<Run>,
}

pub struct GitHubProvider {
    crab: Arc<Octocrab>,
    repo: RepoSpec,
    /// Kept so we can mint a sibling provider for another repo without
    /// re-resolving credentials (see `for_repo`).
    token: String,
    /// The last budget GitHub stamped on an answer it actually gave us.
    ///
    /// Shared with `for_repo` siblings for the same reason the ETag cache is:
    /// the dashboard mints a provider per repo per poll, and a reading that
    /// died with the provider would never be read back.
    budget: Arc<std::sync::Mutex<Option<Quota>>>,
    /// ETags (and the run lists they stand for) per `owner/repo#limit`.
    ///
    /// The dashboard re-asks the same question every poll, and most polls the
    /// answer has not moved. GitHub says so with a 304 — which costs nothing
    /// against the hourly budget — but only if we kept the validator from last
    /// time. Shared across `for_repo` siblings because the dashboard mints a
    /// fresh provider per repo per poll; a cache that died with the provider
    /// would never see its second request.
    run_etags: Arc<std::sync::Mutex<HashMap<String, CachedRuns>>>,
}

impl GitHubProvider {
    pub fn new(repo: RepoSpec, token: String) -> Result<Self> {
        let crab = Octocrab::builder()
            .personal_token(token.clone())
            .build()
            .context("build octocrab client")?;
        Ok(Self {
            crab: Arc::new(crab),
            repo,
            token,
            budget: Arc::new(std::sync::Mutex::new(None)),
            run_etags: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn repo(&self) -> &RepoSpec {
        &self.repo
    }

    /// A provider for a different repo on the same credentials. The HTTP client
    /// is shared — only the repo coordinates differ.
    pub fn for_repo(&self, repo: RepoSpec) -> Self {
        Self {
            crab: self.crab.clone(),
            repo,
            token: self.token.clone(),
            budget: self.budget.clone(),
            run_etags: self.run_etags.clone(),
        }
    }

    /// The repo's default branch — the sane trigger ref for a repo we have no
    /// local checkout of.
    pub async fn default_branch(&self) -> Result<String> {
        let repo = self
            .crab
            .repos(&self.repo.owner, &self.repo.repo)
            .get()
            .await
            .context("get repo")?;
        repo.default_branch
            .ok_or_else(|| anyhow!("repo has no default branch"))
    }

    /// The open pull request this branch is riding, if any.
    ///
    /// `head` is qualified with the owner because GitHub matches it as a plain
    /// branch name otherwise, and someone else's `main` is not this repo's.
    /// Fork-based PRs therefore go unfound — jog works from the checkout's own
    /// remote, so that is the right blind spot to have.
    pub async fn pr_for_branch(&self, branch: &str) -> Result<Option<crate::provider::PrInfo>> {
        let page = self
            .crab
            .pulls(&self.repo.owner, &self.repo.repo)
            .list()
            .state(octocrab::params::State::Open)
            .head(format!("{}:{}", self.repo.owner, branch))
            .per_page(1)
            .send()
            .await
            .context("list pull requests")?;
        Ok(page.items.into_iter().next().map(|pr| crate::provider::PrInfo {
            number: pr.number,
            title: crate::provider::emoji_width_safe(&pr.title.unwrap_or_default()),
            url: pr
                .html_url
                .map(|u| u.to_string())
                .unwrap_or_else(|| format!(
                    "https://github.com/{}/{}/pull/{}",
                    self.repo.owner, self.repo.repo, pr.number
                )),
            draft: pr.draft.unwrap_or(false),
        }))
    }

    /// Bank the budget an answer arrived with, when it carried one.
    fn note_budget(&self, headers: &http::HeaderMap) {
        if let Some(q) = quota_from_headers(headers) {
            *lock(&self.budget) = Some(q);
        }
    }

    /// How much of this token's hourly API budget is left.
    ///
    /// Answered from the headers of the last real answer when there has been
    /// one. The dashboard asks for every repo's runs every poll, so that
    /// reading is a few seconds old at most, it costs no request of its own,
    /// and — unlike `/rate_limit` — it is by construction the bucket jog's own
    /// traffic is being counted against. See `quota_from_headers` for the
    /// github.com behaviour that makes the difference matter.
    ///
    /// `/rate_limit` is the fallback, for the poll before the first answer
    /// lands and for a workspace with no CI repo to ask about. It is exempt
    /// from the limit it reports, so asking costs nothing against the budget,
    /// and it is worth asking at all because a number you can watch fall is
    /// the only warning before every row goes red at once.
    pub async fn quota(&self) -> Result<Quota> {
        if let Some(q) = *lock(&self.budget) {
            return Ok(q);
        }
        let limits = gated(self.crab.ratelimit().get())
            .await
            .context("get rate limit")?;
        let core = limits.resources.core;
        let reset = DateTime::from_timestamp(core.reset as i64, 0)
            .ok_or_else(|| anyhow!("rate limit reset out of range"))?;
        Ok(Quota {
            limit: core.limit as u32,
            used: core.used as u32,
            reset,
        })
    }

    /// Just the jobs of a run, for a caller that already has the run itself.
    ///
    /// The dashboard's live strip is exactly that caller: the row's poll already
    /// listed the run a moment ago, so `get_run`'s second request would re-fetch
    /// something we hold. Across eight repos following four runs each that is
    /// half the poll's traffic spent on a known answer — and that traffic is
    /// what earns the secondary rate limit.
    pub async fn run_jobs(&self, id: u64) -> Result<Vec<Job>> {
        let page = gated(
            self.crab
                .workflows(&self.repo.owner, &self.repo.repo)
                .list_jobs(octocrab::models::RunId(id))
                .per_page(50)
                .send(),
        )
        .await
        .context("list jobs")?;
        Ok(page.items.into_iter().map(map_job).collect())
    }

    /// The repo-wide run list, asked conditionally.
    ///
    /// Sent with `If-None-Match` whenever a previous answer left an ETag
    /// behind. A 304 replays that answer from the cache — and, per GitHub's
    /// documentation, does not count against the hourly budget. A dashboard
    /// polling eight quiet repos every five seconds spends its whole hour on
    /// this one question; conditionally it spends almost nothing.
    ///
    /// Octocrab's typed runs endpoint has no slot for request headers or for
    /// reading response ones, so this speaks the route raw and parses the same
    /// model the typed call would have.
    async fn list_repo_runs_conditional(&self, limit: u8) -> Result<Vec<Run>> {
        let key = format!("{}/{}#{}", self.repo.owner, self.repo.repo, limit);
        let route = format!(
            "/repos/{}/{}/actions/runs?per_page={}",
            self.repo.owner, self.repo.repo, limit
        );
        let prev = lock(&self.run_etags).get(&key).map(|c| c.etag.clone());
        let mut headers = http::HeaderMap::new();
        if let Some(etag) = prev.as_deref().and_then(|e| http::HeaderValue::from_str(e).ok()) {
            headers.insert(http::header::IF_NONE_MATCH, etag);
        }
        let resp = gated(self.crab._get_with_headers(route.as_str(), Some(headers)))
            .await
            .context("list all repo runs")?;
        self.note_budget(resp.headers());
        if resp.status() == http::StatusCode::NOT_MODIFIED {
            if let Some(cached) = lock(&self.run_etags).get(&key) {
                return Ok(cached.runs.clone());
            }
            // A 304 with nothing to replay should be impossible (nothing
            // evicts), but answering it with an empty dashboard would be
            // worse than one uncached request: drop the validator and ask
            // plainly.
            let plain = gated(self.crab._get_with_headers(route.as_str(), None))
                .await
                .context("list all repo runs")?;
            self.note_budget(plain.headers());
            return self.digest_runs_page(&key, plain).await;
        }
        self.digest_runs_page(&key, resp).await
    }

    /// Turn a raw runs-page response into `Run`s, banking the ETag on the way.
    async fn digest_runs_page(
        &self,
        key: &str,
        resp: http::Response<http_body_util::combinators::BoxBody<bytes::Bytes, octocrab::Error>>,
    ) -> Result<Vec<Run>> {
        let etag = resp
            .headers()
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // Through octocrab's own error mapping, so a refusal here classifies
        // (rate limited, not found, …) exactly like one from the typed API.
        let resp = octocrab::map_github_error(resp)
            .await
            .map_err(anyhow::Error::from)
            .context("list all repo runs")?;
        let body = self
            .crab
            .body_to_string(resp)
            .await
            .context("read repo runs body")?;
        #[derive(serde::Deserialize)]
        struct RunsPage {
            workflow_runs: Vec<gh_workflows::Run>,
        }
        let page: RunsPage = serde_json::from_str(&body).context("parse repo runs")?;
        let runs: Vec<Run> = page.workflow_runs.into_iter().map(map_run).collect();
        match etag {
            Some(etag) => {
                lock(&self.run_etags).insert(key.to_string(), CachedRuns { etag, runs: runs.clone() });
            }
            // No validator means the next request cannot be conditional; a
            // stale entry left behind would pair last poll's runs with an
            // ETag GitHub no longer honours.
            None => {
                lock(&self.run_etags).remove(key);
            }
        }
        Ok(runs)
    }

    /// The repo-wide run list assembled from its parts: one request for the
    /// workflow files, one per workflow (capped) for its newest runs, merged
    /// newest-first. Strictly a fallback — it costs N+1 requests where the
    /// real endpoint costs one — for when that endpoint alone is refusing.
    async fn repo_runs_via_workflows(&self, limit: u8) -> Result<Vec<Run>> {
        // Enough for any repo a dashboard row summarises; a monorepo with
        // dozens of workflows must not turn one broken poll into dozens of
        // requests per interval.
        const MAX_WORKFLOWS: usize = 8;
        let page = gated(
            self.crab
                .workflows(&self.repo.owner, &self.repo.repo)
                .list()
                .per_page(50u8)
                .send(),
        )
        .await
        .context("list workflows")?;
        let files: Vec<String> = page
            .items
            .into_iter()
            // Legacy/removed workflows list with an empty path — no file to
            // ask about.
            .filter(|wf| !wf.path.trim().is_empty())
            .map(|wf| {
                wf.path
                    .rsplit('/')
                    .next()
                    .unwrap_or(wf.path.as_str())
                    .to_string()
            })
            .take(MAX_WORKFLOWS)
            .collect();
        if files.is_empty() {
            return Err(anyhow!("no workflows to fall back on"));
        }
        // Spread the budget: enough per workflow to reconstruct the window,
        // never more than the window itself.
        let per = (limit as usize / files.len()).clamp(3, limit.max(1) as usize) as u8;
        let fetches = files.iter().map(|f| async move {
            let mut runs = self.list_runs(f, per).await.unwrap_or_default();
            // The real endpoint doesn't name the workflow file; here we know
            // it, and downstream matching is better off for it.
            for r in &mut runs {
                r.workflow_file = Some(f.clone());
            }
            runs
        });
        let mut all: Vec<Run> = futures::future::join_all(fetches)
            .await
            .into_iter()
            .flatten()
            .collect();
        all.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        all.truncate(limit as usize);
        Ok(all)
    }

    /// Raw text of a file in the repo's default branch.
    async fn fetch_workflow_yaml(&self, path: &str) -> Result<String> {
        let content = gated(
            self.crab
                .repos(&self.repo.owner, &self.repo.repo)
                .get_content()
                .path(path)
                .send(),
        )
        .await
        .with_context(|| format!("get contents of {path}"))?;
        content
            .items
            .into_iter()
            .next()
            .and_then(|c| c.decoded_content())
            .ok_or_else(|| anyhow!("no decodable content at {path}"))
    }
}

fn map_run(r: gh_workflows::Run) -> Run {
    let status = parse_run_status(&r.status, r.conclusion.as_deref());
    let commit_msg = super::emoji_width_safe(
        r.head_commit.message.lines().next().unwrap_or(""),
    );
    Run {
        id: r.id.0,
        display_title: super::emoji_width_safe(&r.name),
        head_branch: r.head_branch,
        commit_msg,
        status,
        created_at: r.created_at,
        updated_at: r.updated_at,
        url: r.html_url.to_string(),
        workflow_file: None,
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
        name: super::emoji_width_safe(&s.name),
        status: map_job_status(&s.status, s.conclusion.as_ref()),
        started_at: s.started_at,
        completed_at: s.completed_at,
    }
}

fn map_job(j: gh_workflows::Job) -> Job {
    let status = map_job_status(&j.status, j.conclusion.as_ref());
    Job {
        id: j.id.0,
        name: super::emoji_width_safe(&j.name),
        status,
        steps: j.steps.into_iter().map(map_step).collect(),
    }
}

#[async_trait]
impl Provider for GitHubProvider {
    async fn list_workflows(&self) -> Result<Vec<Workflow>> {
        let page = self
            .crab
            .workflows(&self.repo.owner, &self.repo.repo)
            .list()
            .per_page(100u8)
            .send()
            .await
            .context("list workflows")?;

        // The list endpoint gives us name + path but not the `on:` block, so it
        // can't tell us whether a workflow is dispatchable or what inputs it
        // takes. Fetch each file's contents concurrently and reuse the same YAML
        // parser the local-checkout path uses. A workflow whose contents we
        // can't read still shows up, just without trigger metadata.
        let fetches = page
            .items
            .into_iter()
            // GitHub lists legacy/removed workflows with an empty path. There is
            // no file to read and no name to address them by, so they'd render
            // as a blank row that can't be opened or triggered.
            .filter(|wf| !wf.path.trim().is_empty())
            .map(|wf| async move {
            let file_name = wf
                .path
                .rsplit('/')
                .next()
                .unwrap_or(wf.path.as_str())
                .to_string();
            let parsed = self
                .fetch_workflow_yaml(&wf.path)
                .await
                .ok()
                .and_then(|raw| super::discovery::parse_workflow_str(&raw, &file_name).ok());
            parsed.unwrap_or(Workflow {
                name: super::emoji_width_safe(&wf.name),
                file_name,
                triggerable: false,
                last_status: None,
                last_run_at: None,
                inputs: Vec::new(),
            })
        });

        let mut out = futures::future::join_all(fetches).await;
        out.sort_by_key(|w| w.name.to_lowercase());
        Ok(out)
    }

    async fn list_runs(&self, workflow_file: &str, limit: u8) -> Result<Vec<Run>> {
        let page = gated(
            self.crab
                .workflows(&self.repo.owner, &self.repo.repo)
                .list_runs(workflow_file)
                .per_page(limit)
                .send(),
        )
        .await
        .context("list workflow runs")?;
        Ok(page.items.into_iter().map(map_run).collect())
    }

    async fn list_repo_runs(&self, limit: u8) -> Result<Vec<Run>> {
        match self.list_repo_runs_conditional(limit).await {
            Ok(runs) => Ok(runs),
            // GitHub's Actions backend has been seen 404-ing exactly this
            // route during incidents while the repo — and its per-workflow
            // runs — answered fine. Degrade to the long way round rather than
            // to eight rows of "not found"; a repo that is genuinely gone
            // fails the fallback too and keeps the original error.
            Err(err) => {
                if classify_error(&err).fault == ApiFault::NotFound
                    && let Ok(runs) = self.repo_runs_via_workflows(limit).await
                {
                    return Ok(runs);
                }
                Err(err)
            }
        }
    }

    async fn get_latest_run(&self, workflow_file: &str) -> Result<Option<Run>> {
        let mut runs = self.list_runs(workflow_file, 1).await?;
        Ok(runs.drain(..).next())
    }

    async fn get_run(&self, id: u64) -> Result<RunDetail> {
        let run_id = octocrab::models::RunId(id);
        let handler = self.crab.workflows(&self.repo.owner, &self.repo.repo);
        let (run, jobs) = tokio::try_join!(
            async { gated(handler.get(run_id)).await.context("get run") },
            self.run_jobs(id),
        )?;
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
        let status = resp.status();
        let text = self
            .crab
            .body_to_string(resp)
            .await
            .context("read log body")?;
        if !status.is_success() {
            if text.contains("BlobNotFound") {
                return Err(anyhow!(
                    "logs not available yet (job still running or archive not finalized)"
                ));
            }
            return Err(anyhow!("fetch job logs: HTTP {status}"));
        }
        let chunks: Vec<Result<LogChunk>> = text
            .lines()
            .map(|l| Ok(LogChunk { line: clean_log_line(l) }))
            .collect();
        Ok(futures::stream::iter(chunks).boxed())
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
///
/// The byte checks pin down the separators but say nothing about bytes 17 and
/// 18, so the `HH:MM:SS` window is taken with `get` rather than by slicing: a
/// multi-byte character starting at either of them straddles offset 19, and
/// slicing there would panic on a line that is merely shaped a bit like a
/// timestamp. A line that fails the check keeps its text whole, which is the
/// right answer for one that was never stamped in the first place.
fn extract_time(s: &str) -> (Option<&str>, &str) {
    if s.len() > 20
        && s.as_bytes().get(4) == Some(&b'-')
        && s.as_bytes().get(7) == Some(&b'-')
        && s.as_bytes().get(10) == Some(&b'T')
        && s.as_bytes().get(13) == Some(&b':')
        && s.as_bytes().get(16) == Some(&b':')
        && let Some(hms) = s.get(11..19)
        && let Some(idx) = s.find(' ') {
            return (Some(hms), &s[idx + 1..]);
        }
    (None, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "live API probe: cargo test etag_roundtrip -- --ignored --nocapture"]
    async fn etag_roundtrip() {
        // Two identical polls: the second should ride the ETag — same answer,
        // and (per GitHub's docs) no charge against the hourly budget. The
        // budget readings printed here are the evidence; the assertions only
        // pin what must hold either way.
        let token = resolve_token().unwrap();
        let p = GitHubProvider::new(RepoSpec::parse("cli/cli").unwrap(), token).unwrap();
        let before = p.quota().await.unwrap();
        let a = p.list_repo_runs(5).await.unwrap();
        let mid = p.quota().await.unwrap();
        let b = p.list_repo_runs(5).await.unwrap();
        let after = p.quota().await.unwrap();
        println!(
            "used: {} -> {} (uncached) -> {} (conditional)",
            before.used, mid.used, after.used
        );
        assert_eq!(
            a.iter().map(|r| r.id).collect::<Vec<_>>(),
            b.iter().map(|r| r.id).collect::<Vec<_>>(),
            "a 304 must replay exactly what the 200 said"
        );
        assert!(!a.is_empty(), "cli/cli runs CI; an empty answer is a parse bug");
    }

    fn rate_headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn the_budget_is_read_off_the_answer_github_actually_gave() {
        let q = quota_from_headers(&rate_headers(&[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-used", "1250"),
            ("x-ratelimit-remaining", "3750"),
            ("x-ratelimit-reset", "1787467570"),
            ("x-ratelimit-resource", "core"),
        ]))
        .expect("a full set of core headers is a reading");
        assert_eq!((q.limit, q.used), (5000, 1250));
        assert_eq!(q.percent(), 25);
        assert_eq!(q.reset.timestamp(), 1787467570);

        // Somebody else's budget. Search gets thirty requests a minute; folding
        // that into the meter would have it read 100% off one search.
        assert!(
            quota_from_headers(&rate_headers(&[
                ("x-ratelimit-limit", "30"),
                ("x-ratelimit-used", "30"),
                ("x-ratelimit-reset", "1787467570"),
                ("x-ratelimit-resource", "search"),
            ]))
            .is_none()
        );
        // Half a set is not a reading: a missing `used` defaulted to zero is
        // exactly the 0% this whole path exists to stop showing.
        assert!(
            quota_from_headers(&rate_headers(&[
                ("x-ratelimit-limit", "5000"),
                ("x-ratelimit-reset", "1787467570"),
            ]))
            .is_none()
        );
        assert!(quota_from_headers(&http::HeaderMap::new()).is_none());
    }

    #[tokio::test]
    async fn a_banked_reading_beats_the_rate_limit_endpoint() {
        // github.com has been seen answering `/rate_limit` with a core bucket
        // reading zero while real calls on the same token came back with a
        // window well underway. Whenever an answer has carried the headers,
        // they are what the meter shows — and asking costs no request, which
        // is why this test can make the claim without a network at all.
        let p = GitHubProvider::new(RepoSpec::parse("acme/api").unwrap(), "t".into()).unwrap();
        p.note_budget(&rate_headers(&[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-used", "40"),
            ("x-ratelimit-reset", "1787467570"),
            ("x-ratelimit-resource", "core"),
        ]));
        assert_eq!(p.quota().await.unwrap().used, 40);

        // And a sibling minted for another repo reads the same meter: the
        // dashboard makes one per repo per poll, so a per-provider reading
        // would be thrown away as fast as it was taken.
        let sibling = p.for_repo(RepoSpec::parse("acme/web").unwrap());
        assert_eq!(sibling.quota().await.unwrap().used, 40);

        // A later answer moves it — that movement is the whole point of a meter.
        sibling.note_budget(&rate_headers(&[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-used", "41"),
            ("x-ratelimit-reset", "1787467570"),
            ("x-ratelimit-resource", "core"),
        ]));
        assert_eq!(p.quota().await.unwrap().used, 41);
    }

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
    fn rate_limit_beats_the_status_code() {
        // Both quota kinds arrive as 403, which on its own reads as a
        // permissions problem and sends you off to re-check token scopes.
        let msg = "API rate limit exceeded for user ID 44789851. If you reach out …";
        assert_eq!(fault_for(403, msg), ApiFault::RateLimited);
        assert_eq!(fault_for(403, "Resource not accessible"), ApiFault::Denied);
        assert_eq!(fault_for(401, "Bad credentials"), ApiFault::BadCredentials);
        assert_eq!(fault_for(404, "Not Found"), ApiFault::NotFound);
        assert_eq!(fault_for(502, "Bad gateway"), ApiFault::Unreachable);
    }

    #[test]
    fn the_pace_limit_is_not_the_hourly_one() {
        // The hourly meter can read 4% while every row is being refused: the
        // secondary limit is about how fast we asked, not how much we spent.
        // Told apart here, or the header offers an hour-away reset time for a
        // wait that is over in a minute.
        for msg in [
            "You have exceeded a secondary rate limit. Please wait a few minutes before you try again.",
            "You have triggered an abuse detection mechanism.",
        ] {
            assert_eq!(fault_for(403, msg), ApiFault::Throttled, "{msg}");
            assert_eq!(fault_for(429, msg), ApiFault::Throttled, "{msg}");
        }
    }

    #[test]
    fn a_bare_429_is_a_rate_limit_without_having_to_say_so() {
        // 429 means one thing. Read as "access denied" it sent the user off to
        // audit token scopes and SSO for something the clock fixes on its own.
        assert_eq!(fault_for(429, "Too Many Requests"), ApiFault::RateLimited);
        assert_eq!(fault_for(429, ""), ApiFault::RateLimited);
        // 403 stays ambiguous by nature, so it still has to be read.
        assert_eq!(fault_for(403, "Resource not accessible"), ApiFault::Denied);
    }

    #[test]
    fn a_line_shaped_like_a_timestamp_does_not_split_a_character_in_half() {
        // Passes every separator check, then turns multi-byte across offset 19.
        // Slicing blind here took the whole TUI down from one malformed line.
        let raw = "2025-04-29T08:12:3\u{2764}\u{2764} rest";
        assert_eq!(clean_log_line(raw), raw, "no timestamp found, so nothing is stripped");

        // The genuine article still parses, including the tight variant where
        // the separating space lands exactly at the end of the HH:MM:SS window.
        assert_eq!(clean_log_line("2025-04-29T08:12:34.567Z hi"), "08:12:34 hi");
        assert_eq!(clean_log_line("2025-04-29T08:12:34 hi"), "08:12:34 hi");
    }

    #[test]
    fn keeps_the_cause_not_the_call() {
        // What the dashboard used to show: "list all repo runs: …", with the
        // part that named the cause truncated off the end.
        let err = anyhow!("no decodable content at .github/workflows/ci.yml")
            .context("get contents of ci.yml")
            .context("list all repo runs");
        let api = classify_error(&err);
        assert_eq!(api.fault, ApiFault::Other);
        assert_eq!(api.text, "no decodable content at .github/workflows/ci.yml");
    }

    #[test]
    fn drops_the_paragraph_github_attaches() {
        let err = anyhow!("Server Error\nDocumentation URL: https://docs.github.com/…");
        assert_eq!(classify_error(&err).text, "Server Error");
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
