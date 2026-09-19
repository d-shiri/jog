//! The shape of a run: which jobs are legs of the same matrix, and what order
//! the `needs:` edges put them in.
//!
//! GitHub's own run page draws exactly this — a box labelled `Matrix: build`
//! around the legs, everything else chained left to right in dependency order.
//! The REST jobs endpoint hands us none of it: a matrix leg arrives as an
//! ordinary job whose name has already been expanded (`build gojobi`), with no
//! hint of the YAML job it came from. So we read the shape back out of the
//! workflow file — on disk in any checkout — and match each run job to the YAML
//! job that produced it.
//!
//! Matching is by name, because the name is all the two sides share. A job's
//! `name:` is a template (`build ${{ matrix.service }}`); every `${{ … }}` in it
//! is a hole that the run filled with something we can't predict, so the
//! template compiles to literal segments around wildcards and a run job belongs
//! to whichever template it fits most specifically. Two suffixes GitHub adds on
//! its own are allowed for: `key (ubuntu, 3.11)` for a matrix job with no
//! `name:` of its own, and `caller / inner job` for a job that calls a reusable
//! workflow.

use std::collections::HashMap;

use serde_yml::Value;

use super::Job;

/// One piece of a compiled `name:` template.
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Lit(String),
    /// A `${{ … }}` expression: whatever the run put there.
    Wild,
}

#[derive(Debug, Clone)]
pub struct JobSpec {
    /// The key under `jobs:` — what GitHub calls the job when nothing else does.
    pub key: String,
    /// Does this job fan out over a `strategy.matrix`?
    pub matrix: bool,
    needs: Vec<String>,
    pattern: Vec<Seg>,
    /// Longest chain of `needs:` behind this job — its column in the graph.
    depth: usize,
    /// A reusable workflow in this repo that this job calls, as written.
    uses: Option<String>,
    /// That file, once it has been read. A call is a whole graph standing in
    /// one job's place: GitHub runs each of its jobs as a job of this run,
    /// named `caller / inner`, and their order is the called file's business.
    called: Option<Box<WorkflowGraph>>,
}

/// Where a run job sits: which column, and which matrix it is a leg of.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub depth: usize,
    /// `Some(key)` for a leg of a matrix — legs sharing a key are one box.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowGraph {
    jobs: Vec<JobSpec>,
}

/// A run's jobs, arranged the way the workflow file says they belong together.
#[derive(Debug, Clone, PartialEq)]
pub enum RunNode {
    /// A job that stands on its own. The index is into `RunDetail::jobs`.
    Job(usize),
    /// Legs of one matrix job, alphabetical like GitHub's box.
    Matrix { key: String, legs: Vec<usize> },
}

impl WorkflowGraph {
    /// Parse a workflow file. `None` when it has no `jobs:` mapping to read —
    /// the caller then falls back to name-shaped guessing.
    pub fn parse(raw: &str) -> Option<Self> {
        let doc: Value = serde_yml::from_str(raw).ok()?;
        let jobs = doc.get("jobs").and_then(|j| match j {
            Value::Mapping(m) => Some(m),
            _ => None,
        })?;
        let mut specs = Vec::new();
        for (key, body) in jobs {
            let Some(key) = key.as_str() else { continue };
            let name = body.get("name").and_then(|v| v.as_str());
            let matrix = body
                .get("strategy")
                .and_then(|s| s.get("matrix"))
                .is_some();
            specs.push(JobSpec {
                key: key.to_string(),
                matrix,
                needs: parse_needs(body.get("needs")),
                pattern: compile(name.unwrap_or(key)),
                depth: 0,
                // Only a path into this repo. `owner/repo/.github/…@ref` names
                // a file on some other checkout, which there is no reading
                // from here.
                uses: body
                    .get("uses")
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("./"))
                    .map(str::to_string),
                called: None,
            });
        }
        if specs.is_empty() {
            return None;
        }
        resolve_depths(&mut specs);
        Some(Self { jobs: specs })
    }

    /// Read the reusable workflows this one calls, so their chains become part
    /// of this one's.
    ///
    /// `budget` bounds the recursion: a workflow that calls itself, directly or
    /// round a ring, would otherwise be read until the stack gave out.
    pub fn resolve_calls(&mut self, repo_root: &std::path::Path, budget: u8) {
        if budget == 0 {
            return;
        }
        for spec in &mut self.jobs {
            let Some(uses) = spec.uses.as_deref() else {
                continue;
            };
            let path = repo_root.join(uses.trim_start_matches("./"));
            let Ok(raw) = std::fs::read_to_string(path) else {
                continue;
            };
            if let Some(mut inner) = Self::parse(&raw) {
                inner.resolve_calls(repo_root, budget - 1);
                spec.called = Some(Box::new(inner));
            }
        }
        // A job that stands for a two-column workflow occupies two columns, so
        // whatever waits on it starts two later, not one.
        resolve_depths(&mut self.jobs);
    }

    /// Columns this graph occupies — one more than its last job's depth.
    fn span(&self) -> usize {
        self.jobs.iter().map(|s| s.depth + 1).max().unwrap_or(1)
    }

    /// Where a run job belongs in the chain.
    ///
    /// A job that came out of a reusable workflow is named `caller / inner`,
    /// and its column is the caller's plus the inner job's own — which is what
    /// puts an arrow between two halves of one call, exactly where the run
    /// page draws one.
    pub fn place(&self, run_job_name: &str) -> Option<Placement> {
        let spec = self.spec_for(run_job_name)?;
        if let Some(inner) = &spec.called
            && let Some((_, tail)) = run_job_name.split_once(" / ")
            && let Some(within) = inner.place(tail)
        {
            return Some(Placement {
                depth: spec.depth + within.depth,
                group: within.group.map(|g| format!("{}/{g}", spec.key)),
            });
        }
        Some(Placement {
            depth: spec.depth,
            group: spec.matrix.then(|| spec.key.clone()),
        })
    }

    /// Does this file still describe that run?
    ///
    /// A job renamed since the run went out matches nothing here, and one that
    /// matches nothing gets no place in the chain — it inherits the depth of
    /// whatever came before it. A few of those are survivable; a file where
    /// *none* of the names land has told us nothing about the order, and a
    /// caller must not report its columns as fact.
    pub fn describes(&self, jobs: &[Job]) -> bool {
        !jobs.is_empty() && jobs.iter().all(|j| self.spec_for(&j.name).is_some())
    }

    /// The YAML job a run job's name came from, or `None` when nothing fits.
    ///
    /// Several templates can fit one name (`deploy` and `deploy ${{ … }}` both
    /// fit `deploy`), so the most specific wins: the one matching on the most
    /// literal text, and an exact fit ahead of one that needed a suffix.
    pub fn spec_for(&self, run_job_name: &str) -> Option<&JobSpec> {
        self.jobs
            .iter()
            .filter_map(|s| score(s, run_job_name).map(|sc| (sc, s)))
            .max_by_key(|(sc, _)| *sc)
            .map(|(_, s)| s)
    }
}

/// How well `name` fits this spec — higher is more specific, `None` is no fit.
fn score(spec: &JobSpec, name: &str) -> Option<usize> {
    let lit: usize = spec
        .pattern
        .iter()
        .map(|s| match s {
            Seg::Lit(l) => l.chars().count(),
            Seg::Wild => 0,
        })
        .sum();
    // An exact fit beats one that had to ignore a suffix, whatever the
    // literals say — hence the constant, which no literal count can reach
    // because a workflow job name is never that long.
    const EXACT: usize = 1 << 20;
    if glob_match(&spec.pattern, name) {
        // `name: ${{ matrix.name }}` compiles to a lone wildcard, which fits
        // every job there is. It is still a fit — those legs have no other
        // spec to belong to — but it must lose to anything that matched on
        // text, or it would swallow the jobs that did.
        return Some(if lit > 0 { EXACT + lit } else { 0 });
    }
    // `build (ubuntu-latest, 3.11)` — a matrix job that kept the default name.
    if spec.matrix
        && let Some(head) = name.strip_suffix(')').and_then(|n| n.rsplit_once(" (")).map(|(h, _)| h)
        && glob_match(&spec.pattern, head)
    {
        return Some(lit);
    }
    // `deploy-stage / v0.0.21 → stage` — a job that calls a reusable workflow
    // is named after the caller, then the job inside it.
    if let Some((head, _)) = name.split_once(" / ")
        && glob_match(&spec.pattern, head)
    {
        return Some(lit);
    }
    None
}

/// `needs: build` and `needs: [build, lint]` both mean a list.
fn parse_needs(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Longest path to each job along `needs:`. Bounded by the job count, so a
/// malformed file that points a cycle at itself stops instead of spinning.
fn resolve_depths(specs: &mut [JobSpec]) {
    let index: HashMap<&str, usize> = specs
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key.as_str(), i))
        .collect();
    let edges: Vec<Vec<usize>> = specs
        .iter()
        .map(|s| s.needs.iter().filter_map(|n| index.get(n.as_str()).copied()).collect())
        .collect();
    // How many columns each job takes: one, unless it stands for a whole
    // reusable workflow that has been read.
    let span: Vec<usize> = specs
        .iter()
        .map(|s| s.called.as_ref().map(|g| g.span()).unwrap_or(1))
        .collect();
    let mut depth = vec![0usize; specs.len()];
    for _ in 0..specs.len() {
        let mut moved = false;
        for (i, ups) in edges.iter().enumerate() {
            let want = ups.iter().map(|&u| depth[u] + span[u]).max().unwrap_or(0);
            if want > depth[i] {
                depth[i] = want;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    for (spec, d) in specs.iter_mut().zip(depth) {
        spec.depth = d;
    }
}

/// Split a `name:` template on its `${{ … }}` expressions.
fn compile(template: &str) -> Vec<Seg> {
    // Job names arrive from the API through the same widener, so a template
    // with an emoji in it has to be widened too or the literals won't line up.
    let template = super::emoji_width_safe(template);
    let mut segs = Vec::new();
    let mut rest = template.as_str();
    while let Some(start) = rest.find("${{") {
        if start > 0 {
            segs.push(Seg::Lit(rest[..start].to_string()));
        }
        match rest[start..].find("}}") {
            Some(end) => {
                segs.push(Seg::Wild);
                rest = &rest[start + end + 2..];
            }
            // An unclosed expression: the rest of the template is a hole.
            None => {
                segs.push(Seg::Wild);
                rest = "";
                break;
            }
        }
    }
    if !rest.is_empty() {
        segs.push(Seg::Lit(rest.to_string()));
    }
    segs
}

fn glob_match(segs: &[Seg], s: &str) -> bool {
    let mut pos = 0usize;
    // Set by a wildcard: the next literal may start anywhere, not just here.
    let mut floating = false;
    for seg in segs {
        match seg {
            Seg::Wild => floating = true,
            Seg::Lit(lit) => {
                if floating {
                    match s[pos..].find(lit.as_str()) {
                        Some(k) => pos += k + lit.len(),
                        None => return false,
                    }
                    floating = false;
                } else {
                    if !s[pos..].starts_with(lit.as_str()) {
                        return false;
                    }
                    pos += lit.len();
                }
            }
        }
    }
    // A trailing wildcard swallows whatever is left; a trailing literal has to
    // land on the end.
    floating || pos == s.len()
}

/// Arrange a run's jobs into single jobs and matrix boxes.
///
/// With a `graph` the grouping is what the workflow file says. Without one —
/// no checkout, or a file we couldn't read — legs are still recognisable when
/// the matrix job kept its default name, because GitHub writes the combination
/// in brackets: `test (ubuntu-latest, 3.11)`.
pub fn shape(jobs: &[Job], graph: Option<&WorkflowGraph>) -> Vec<RunNode> {
    laid_out(jobs, graph).into_iter().map(|(_, n)| n).collect()
}

/// The same arrangement, kept in the columns the `needs:` edges put it in:
/// one entry per stage, in order, each holding what that stage runs at once.
///
/// This is the run page's graph — everything in a stage starts when the stage
/// before it has finished. A workflow with no `needs:` anywhere is one stage
/// of everything, which is exactly what it is.
pub fn stages(jobs: &[Job], graph: Option<&WorkflowGraph>) -> Vec<Vec<RunNode>> {
    let mut out: Vec<(usize, Vec<RunNode>)> = Vec::new();
    for (depth, node) in laid_out(jobs, graph) {
        match out.last_mut() {
            Some((d, stage)) if *d == depth => stage.push(node),
            _ => out.push((depth, vec![node])),
        }
    }
    out.into_iter().map(|(_, stage)| stage).collect()
}

fn laid_out(jobs: &[Job], graph: Option<&WorkflowGraph>) -> Vec<(usize, RunNode)> {
    let mut group_of: Vec<Option<String>> = vec![None; jobs.len()];
    let mut depth_of: Vec<usize> = vec![0; jobs.len()];
    match graph {
        Some(g) => {
            // A job nothing in the file matches — renamed since the run, most
            // likely — keeps the company it arrived in rather than sorting to
            // the front as a depth of nothing.
            let mut last = 0usize;
            for (i, job) in jobs.iter().enumerate() {
                match g.place(&job.name) {
                    Some(at) => {
                        depth_of[i] = at.depth;
                        last = at.depth;
                        group_of[i] = at.group;
                    }
                    None => depth_of[i] = last,
                }
            }
        }
        None => {
            for (i, job) in jobs.iter().enumerate() {
                if let Some(head) = job
                    .name
                    .strip_suffix(')')
                    .and_then(|n| n.rsplit_once(" ("))
                    .map(|(h, _)| h)
                    .filter(|h| !h.is_empty())
                {
                    group_of[i] = Some(head.to_string());
                }
            }
        }
    }

    // One leg is not a matrix worth boxing — it reads as the job it is.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for key in group_of.iter().flatten() {
        *counts.entry(key.clone()).or_default() += 1;
    }
    for slot in group_of.iter_mut() {
        if slot.as_deref().map(|k| counts[k] < 2).unwrap_or(false) {
            *slot = None;
        }
    }

    // Emit each node where its first member sits, so a run whose YAML we
    // couldn't read keeps the order the API gave us.
    let mut nodes: Vec<(usize, usize, RunNode)> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (i, key) in group_of.iter().enumerate() {
        match key {
            None => nodes.push((depth_of[i], i, RunNode::Job(i))),
            Some(key) => {
                if seen.contains(&key.as_str()) {
                    continue;
                }
                seen.push(key);
                let mut legs: Vec<usize> = group_of
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| k.as_deref() == Some(key.as_str()))
                    .map(|(j, _)| j)
                    .collect();
                legs.sort_by(|&a, &b| jobs[a].name.cmp(&jobs[b].name));
                nodes.push((
                    depth_of[i],
                    i,
                    RunNode::Matrix {
                        key: key.clone(),
                        legs,
                    },
                ));
            }
        }
    }
    nodes.sort_by_key(|(depth, first, _)| (*depth, *first));
    nodes.into_iter().map(|(depth, _, n)| (depth, n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Status;

    const WF: &str = r#"
name: deploy to stage
on: workflow_dispatch
jobs:
  which-commit:
    name: which commit
    runs-on: ubuntu-latest
  build:
    needs: which-commit
    name: build ${{ matrix.service }}
    strategy:
      matrix:
        service: [db-backup, gojobi, ingestor, ollama, wecker]
    runs-on: ubuntu-latest
  deploy-stage:
    needs: [build]
    uses: ./.github/workflows/deploy.yml
"#;

    fn job(id: u64, name: &str) -> Job {
        Job {
            id,
            name: name.into(),
            status: Status::Success,
            started_at: None,
            completed_at: None,
            steps: Vec::new(),
        }
    }

    #[test]
    fn matrix_legs_group_under_their_yaml_job() {
        let g = WorkflowGraph::parse(WF).unwrap();
        // Jobs come back in start order, legs interleaved the way GitHub
        // scheduled them.
        let jobs = vec![
            job(1, "which commit"),
            job(2, "build ingestor"),
            job(3, "build wecker"),
            job(4, "build db-backup"),
            job(5, "build gojobi"),
            job(6, "build ollama"),
            job(7, "deploy-stage / v0.0.21 → stage"),
            job(8, "deploy-stage / v0.0.21 → stage (GPU box)"),
        ];
        let nodes = shape(&jobs, Some(&g));
        assert_eq!(
            nodes,
            vec![
                RunNode::Job(0),
                // Alphabetical inside the box, like the run page.
                RunNode::Matrix {
                    key: "build".into(),
                    legs: vec![3, 4, 1, 5, 2],
                },
                RunNode::Job(6),
                RunNode::Job(7),
            ]
        );
    }

    #[test]
    fn a_reusable_workflow_call_is_not_a_matrix() {
        let g = WorkflowGraph::parse(WF).unwrap();
        let spec = g.spec_for("deploy-stage / v0.0.21 → stage").unwrap();
        assert_eq!(spec.key, "deploy-stage");
        assert!(!spec.matrix);
    }

    #[test]
    fn needs_sets_the_order_even_when_the_api_does_not() {
        let g = WorkflowGraph::parse(WF).unwrap();
        // A leg that started before `which commit` reported in still sorts
        // behind it, because the workflow says it does.
        let jobs = vec![
            job(1, "build gojobi"),
            job(2, "build ollama"),
            job(3, "which commit"),
        ];
        let nodes = shape(&jobs, Some(&g));
        assert_eq!(
            nodes,
            vec![
                RunNode::Job(2),
                RunNode::Matrix {
                    key: "build".into(),
                    legs: vec![0, 1],
                },
            ]
        );
    }

    #[test]
    fn a_job_the_file_no_longer_knows_keeps_its_place() {
        let g = WorkflowGraph::parse(WF).unwrap();
        let jobs = vec![
            job(1, "which commit"),
            job(2, "build gojobi"),
            job(3, "build ollama"),
            // Renamed in the file since this run went out.
            job(4, "smoke test"),
        ];
        assert_eq!(
            shape(&jobs, Some(&g)),
            vec![
                RunNode::Job(0),
                RunNode::Matrix {
                    key: "build".into(),
                    legs: vec![1, 2],
                },
                RunNode::Job(3),
            ]
        );
    }

    #[test]
    fn a_matrix_that_kept_the_default_name_is_still_a_matrix() {
        // jog's own workflow: one `build` job over a matrix of includes, and
        // no `name:`, so GitHub writes the combination in brackets.
        let wf = r#"
jobs:
  build:
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-musl
          - os: macos-latest
            target: aarch64-apple-darwin
"#;
        let g = WorkflowGraph::parse(wf).unwrap();
        let jobs = vec![
            job(1, "build (ubuntu-latest, x86_64-unknown-linux-musl)"),
            job(2, "build (macos-latest, aarch64-apple-darwin)"),
        ];
        assert_eq!(
            shape(&jobs, Some(&g)),
            vec![RunNode::Matrix {
                key: "build".into(),
                legs: vec![1, 0],
            }]
        );
    }

    #[test]
    fn default_named_legs_group_without_any_yaml() {
        let jobs = vec![
            job(1, "test (ubuntu-latest, 3.11)"),
            job(2, "test (ubuntu-latest, 3.12)"),
            job(3, "lint"),
        ];
        assert_eq!(
            shape(&jobs, None),
            vec![
                RunNode::Matrix {
                    key: "test".into(),
                    legs: vec![0, 1],
                },
                RunNode::Job(2),
            ]
        );
    }

    #[test]
    fn a_one_leg_matrix_stays_a_plain_job() {
        let g = WorkflowGraph::parse(WF).unwrap();
        let jobs = vec![job(1, "which commit"), job(2, "build gojobi")];
        assert_eq!(shape(&jobs, Some(&g)), vec![RunNode::Job(0), RunNode::Job(1)]);
    }

    #[test]
    fn the_most_literal_template_wins() {
        let wf = r#"
jobs:
  deploy:
    name: deploy
  deploy-matrix:
    name: deploy ${{ matrix.env }}
    strategy:
      matrix:
        env: [stage, prod]
"#;
        let g = WorkflowGraph::parse(wf).unwrap();
        assert_eq!(g.spec_for("deploy").unwrap().key, "deploy");
        assert_eq!(g.spec_for("deploy stage").unwrap().key, "deploy-matrix");
    }

    #[test]
    fn a_reusable_workflow_s_own_chain_is_part_of_the_run_s() {
        let root = std::env::temp_dir().join(format!("jog-calls-{}", std::process::id()));
        let dir = root.join(".github").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        // The called file runs two jobs, the second after the first.
        std::fs::write(
            dir.join("deploy.yml"),
            "jobs:\n  stage:\n    name: v0.0.21 → stage\n  gpu:\n    needs: stage\n    name: v0.0.21 → stage (GPU box)\n",
        )
        .unwrap();
        let mut g = WorkflowGraph::parse(WF).unwrap();

        // Unread, both halves of the call are one job and share a column.
        let flat = stages(
            &[
                job(1, "which commit"),
                job(2, "deploy-stage / v0.0.21 → stage"),
                job(3, "deploy-stage / v0.0.21 → stage (GPU box)"),
            ],
            Some(&g),
        );
        assert_eq!(flat.len(), 2, "{flat:?}");

        // Read, the run's chain runs through it: the GPU half waits on the
        // other, so it gets a column — and an arrow — of its own.
        g.resolve_calls(&root, 3);
        let jobs = vec![
            job(1, "which commit"),
            job(2, "build gojobi"),
            job(3, "build ollama"),
            job(4, "deploy-stage / v0.0.21 → stage"),
            job(5, "deploy-stage / v0.0.21 → stage (GPU box)"),
        ];
        let st = stages(&jobs, Some(&g));
        assert_eq!(st.len(), 4, "{st:?}");
        assert_eq!(st[2], vec![RunNode::Job(3)]);
        assert_eq!(st[3], vec![RunNode::Job(4)]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_workflow_that_calls_itself_does_not_read_forever() {
        let root = std::env::temp_dir().join(format!("jog-loop-{}", std::process::id()));
        let dir = root.join(".github").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        // Two files that call each other. Nothing sane does this; the reader
        // must not fall over when something does.
        std::fs::write(
            dir.join("a.yml"),
            "jobs:\n  go:\n    uses: ./.github/workflows/b.yml\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.yml"),
            "jobs:\n  back:\n    uses: ./.github/workflows/a.yml\n",
        )
        .unwrap();
        let mut g = WorkflowGraph::parse(&std::fs::read_to_string(dir.join("a.yml")).unwrap())
            .unwrap();
        g.resolve_calls(&root, 3);
        assert!(g.place("go / back / go").is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_call_we_cannot_open_leaves_the_chain_where_it_was() {
        let mut g = WorkflowGraph::parse(WF).unwrap();
        // Nothing to read there; the caller stays one column, as before.
        g.resolve_calls(std::path::Path::new("/nonexistent-for-this-test"), 3);
        let jobs = vec![
            job(1, "which commit"),
            job(2, "deploy-stage / a"),
            job(3, "deploy-stage / b"),
        ];
        assert_eq!(stages(&jobs, Some(&g)).len(), 2);
    }

    #[test]
    fn stages_follow_the_needs_chain() {
        let g = WorkflowGraph::parse(WF).unwrap();
        let jobs = vec![
            job(1, "which commit"),
            job(2, "build ingestor"),
            job(3, "build gojobi"),
            job(4, "deploy-stage / v0.0.21 → stage"),
            job(5, "deploy-stage / v0.0.21 → stage (GPU box)"),
        ];
        let st = stages(&jobs, Some(&g));
        // Three columns: the commit check, the matrix, then both halves of the
        // deploy — which are one stage, being one caller job.
        assert_eq!(st.len(), 3);
        assert_eq!(st[0], vec![RunNode::Job(0)]);
        assert_eq!(
            st[1],
            vec![RunNode::Matrix { key: "build".into(), legs: vec![2, 1] }]
        );
        assert_eq!(st[2], vec![RunNode::Job(3), RunNode::Job(4)]);
    }

    #[test]
    fn a_file_that_no_longer_names_the_run_s_jobs_describes_nothing() {
        let g = WorkflowGraph::parse(WF).unwrap();
        assert!(g.describes(&[job(1, "which commit"), job(2, "build gojobi")]));
        // Renamed since: the file parses, and says nothing about these.
        assert!(!g.describes(&[job(1, "unit tests"), job(2, "smoke tests")]));
        // Half-known is not known: the chain would be guesswork either way.
        assert!(!g.describes(&[job(1, "which commit"), job(2, "smoke tests")]));
        assert!(!g.describes(&[]));
    }

    #[test]
    fn a_workflow_with_no_needs_is_one_stage_of_everything() {
        let jobs = vec![job(1, "lint"), job(2, "test"), job(3, "build")];
        let st = stages(&jobs, None);
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].len(), 3);
    }

    #[test]
    fn a_name_that_is_only_an_expression_does_not_swallow_the_others() {
        let wf = r#"
jobs:
  build:
    name: ${{ matrix.name }}
    strategy:
      matrix:
        name: [unit, e2e]
  deploy:
    needs: build
    uses: ./.github/workflows/deploy.yml
"#;
        let g = WorkflowGraph::parse(wf).unwrap();
        // The caller's own name is text, and text wins over a bare hole.
        assert_eq!(g.spec_for("deploy / v1 → stage").unwrap().key, "deploy");
        // The legs still find the job they came from.
        assert_eq!(g.spec_for("unit").unwrap().key, "build");
        let jobs = vec![
            job(1, "unit"),
            job(2, "e2e"),
            job(3, "deploy / v1 → stage"),
        ];
        assert_eq!(
            shape(&jobs, Some(&g)),
            vec![
                RunNode::Matrix {
                    key: "build".into(),
                    legs: vec![1, 0],
                },
                RunNode::Job(2),
            ]
        );
    }

    #[test]
    fn a_file_without_jobs_is_no_graph() {
        assert!(WorkflowGraph::parse("name: x\non: push\n").is_none());
        assert!(WorkflowGraph::parse("::: not yaml :::\n\t- [").is_none());
    }
}
