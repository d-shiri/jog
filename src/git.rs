//! Local git plumbing: workspace discovery plus the small slice of porcelain
//! needed to review and commit changes without leaving the TUI.
//!
//! Everything shells out to `git` rather than linking a library — the same
//! approach `provider::github` already uses for remote/branch lookup, and it
//! keeps the binary free of a libgit2 dependency.

use anyhow::{Context, Result, anyhow};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Directory names that never contain a checkout worth listing and are
/// expensive to walk.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
    ".git",
];

/// How deep below the workspace root to look for checkouts. Depth 2 covers the
/// common `workspace/project` layout plus one level of grouping
/// (`workspace/group/project`) without turning startup into a full tree walk.
const MAX_DEPTH: usize = 2;

/// Find git checkouts underneath `root`.
///
/// `root` itself is not considered — this is for the case where you are sitting
/// in a plain directory whose *children* are repos. Descent stops at the first
/// checkout on each branch of the tree, so submodules and nested worktrees don't
/// produce duplicate rows.
pub fn discover_workspace(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_repos(root, 1, &mut found);
    found.sort();
    found
}

fn collect_repos(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if path.join(".git").exists() {
            out.push(path);
            // Don't descend into a checkout: submodules would show up as
            // separate repos the user never asked about.
            continue;
        }
        collect_repos(&path, depth + 1, out);
    }
}

/// A single line of `git status --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Index (staged) state, or ' ' when unchanged. '?' for untracked.
    pub index: char,
    /// Worktree (unstaged) state, or ' ' when unchanged.
    pub worktree: char,
    pub path: String,
}

impl StatusEntry {
    pub fn is_untracked(&self) -> bool {
        self.index == '?'
    }

    /// Whether this path has anything in the index ready to be committed.
    pub fn is_staged(&self) -> bool {
        !self.is_untracked() && self.index != ' '
    }

    /// Whether this path has changes that are *not* staged.
    pub fn has_unstaged(&self) -> bool {
        self.is_untracked() || self.worktree != ' '
    }

    /// Two-letter code as git prints it, for display.
    pub fn code(&self) -> String {
        format!("{}{}", self.index, self.worktree)
    }

    /// Human label for the dominant change.
    pub fn label(&self) -> &'static str {
        if self.is_untracked() {
            return "untracked";
        }
        match if self.index != ' ' { self.index } else { self.worktree } {
            'M' => "modified",
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'U' => "conflict",
            'T' => "typechange",
            _ => "changed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: String,
    /// Commits ahead of / behind the upstream, when an upstream is configured.
    pub ahead: u32,
    pub behind: u32,
    pub has_upstream: bool,
    /// No branch checked out — `branch` is the placeholder `HEAD`.
    pub detached: bool,
    pub entries: Vec<StatusEntry>,
}

impl RepoStatus {
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn staged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_staged()).count()
    }

    pub fn unstaged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.has_unstaged()).count()
    }
}

/// Run a git command in `dir`, returning stdout on success.
///
/// Terminal and GUI credential prompts are disabled: a `git push` that wants a
/// password would otherwise block forever behind the alternate screen with no
/// way for the user to answer it.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            err
        };
        return Err(anyhow!("git {}: {}", args.join(" "), first_line(&err)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a diff-producing git command in `dir`.
///
/// Separate from `git` because a diff exits 1 to mean "there were differences"
/// rather than to report an error — `--no-index` does this on every non-empty
/// diff — so exit 1 has to count as success here and only here.
fn git_diff_cmd(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    match out.status.code() {
        Some(0) | Some(1) => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        _ => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(anyhow!("git {}: {}", args.join(" "), first_line(&err)))
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("failed").to_string()
}

fn last_line(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("failed")
        .to_string()
}

/// Run a git command in `dir`, handing each output line to `on_line` as it is
/// produced instead of buffering until exit.
///
/// This exists for the commands that run hooks. `git commit` in a repo with a
/// `pre-commit` hook is not a git command that takes 40 milliseconds — it is a
/// pytest or pyright run that takes 40 seconds, and when it fails its output is
/// the only description of what went wrong. `Command::output()` would hold all
/// of that until the process exits and then let the caller drop it.
///
/// stdout and stderr are merged — into one OS pipe, the way `2>&1` merges them
/// on a terminal: hooks write to both, interleaved, and the distinction is not
/// one the reader cares about, but the *order* is. `stdin` is `null` — a hook
/// that reads from stdin would otherwise be handed the terminal the TUI is
/// reading keystrokes from, and the two would race for the user's typing.
///
/// `on_line` gets `(text, partial)`. A partial line is one the process has
/// written but not yet finished — `pre-commit` prints `pytest.......` the
/// moment a step *starts* and only appends `Passed\n` when it ends, so for a
/// slow step the unterminated head of the line is the entire progress report.
/// A partial is always superseded by the next call, which carries the same
/// line grown longer (still partial) or completed (not).
fn git_streaming(
    dir: &Path,
    args: &[&str],
    on_line: &mut dyn FnMut(String, bool),
) -> Result<(bool, Vec<String>)> {
    let (reader, writer) = std::io::pipe().context("create output pipe")?;
    let writer2 = writer.try_clone().context("clone output pipe")?;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Hooks that colour their output would otherwise have to be stripped
        // line by line; asking them not to is cheaper and more reliable.
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer2)
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    // The parent's copies of the write end live in the `Command`, which is a
    // temporary and so is dropped at the end of this statement — that is what
    // closes them and lets `reader` see EOF when the child (and anything it
    // handed the pipe to) exits. Binding the builder to a `let` instead would
    // keep a write end open in this process and `pump_lines` would never return.

    let mut collected = Vec::new();
    pump_lines(reader, &mut |text, partial| {
        // Partials are progress, not record: each is superseded by the finished
        // line, which is the one an error message should quote.
        if !partial {
            collected.push(text.clone());
        }
        on_line(text, partial);
    });

    let status = child
        .wait()
        .with_context(|| format!("wait for git {}", args.join(" ")))?;
    Ok((status.success(), collected))
}

/// Forward the pipe's output as lines, without waiting for lines to end.
///
/// `read_until(b'\n')` would be simpler, but it sits on an unterminated line
/// forever — and `pre-commit`'s unterminated line is the name of the step
/// currently running. So this reads whatever bytes are available, emits every
/// completed line as `partial = false`, and emits the unterminated tail as
/// `partial = true` whenever it has changed.
///
/// Reads bytes rather than text because a hook's output is not guaranteed to
/// be UTF-8 and one bad byte should not truncate the log. Lines rewritten in
/// place with `\r` — every test runner's progress bar — are reported as their
/// latest state, which is what the same output would have looked like on a
/// terminal.
fn pump_lines(mut reader: impl Read, emit: &mut dyn FnMut(String, bool)) {
    // What a terminal would show for one line's bytes: text after the last
    // carriage return, i.e. the state the line was last rewritten to.
    fn rendered(bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        text.rsplit('\r').next().unwrap_or(text).to_string()
    }

    let mut acc: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut last_partial = String::new();
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                acc.extend_from_slice(&chunk[..n]);
                while let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = acc.drain(..=pos).collect();
                    emit(rendered(&line), false);
                    last_partial.clear();
                }
                // Everything before the last carriage return has already been
                // overwritten and can never render again. Dropping it bounds
                // `acc` to one screen line: a hook whose progress bar rewrites
                // itself for ten minutes without ever printing a newline would
                // otherwise accumulate the whole stream, and re-render all of it
                // on every read.
                let tail_crs = acc.iter().rev().take_while(|&&b| b == b'\r').count();
                let end = acc.len() - tail_crs;
                if let Some(cr) = acc[..end].iter().rposition(|&b| b == b'\r') {
                    acc.drain(..=cr);
                }
                if !acc.is_empty() {
                    let text = rendered(&acc);
                    // Don't repeat an unchanged tail: a chunk that ends exactly
                    // on a newline boundary hasn't grown the partial line.
                    if text != last_partial {
                        last_partial = text.clone();
                        emit(text, true);
                    }
                }
            }
        }
    }
    // A process can exit mid-line; the tail is real output, not progress.
    if !acc.is_empty() {
        emit(rendered(&acc), false);
    }
}

/// Path to `hook` if this repo has an *enabled* one, honouring `core.hooksPath`.
///
/// Used only to name what is running: "running pre-commit hook" is a far better
/// answer to a stalled-looking screen than "committing", and when the command
/// fails it says which half failed. A hook git would skip — missing, or present
/// but not executable — must not be named, or the label starts lying.
pub fn hook_path(dir: &Path, hook: &str) -> Option<PathBuf> {
    let configured = git(dir, &["config", "--get", "core.hooksPath"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let hooks_dir = match configured {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() { p } else { dir.join(p) }
        }
        None => {
            let git_dir = git(dir, &["rev-parse", "--git-dir"]).ok()?.trim().to_string();
            let git_dir = PathBuf::from(&git_dir);
            let git_dir = if git_dir.is_absolute() {
                git_dir
            } else {
                dir.join(git_dir)
            };
            git_dir.join("hooks")
        }
    };
    let path = hooks_dir.join(hook);
    if !path.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(&path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !executable {
            return None;
        }
    }
    Some(path)
}

pub fn current_branch(dir: &Path) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

pub fn remote_url(dir: &Path) -> Result<String> {
    Ok(git(dir, &["remote", "get-url", "origin"])?
        .trim()
        .to_string())
}

/// A cheap identity for the state `.git` itself records — HEAD, the index,
/// the loose and packed refs, a merge in progress.
///
/// This is what lets the dashboard skip most of its `git status` subprocesses:
/// commits, staging, branch switches and pulls all move one of these files (a
/// ref update lands by rename, which stamps its parent directory), so an
/// unchanged fingerprint means that whole class of change did not happen.
/// What it cannot see is an edit to a tracked file, which touches nothing
/// under `.git` — the caller covers that with a periodic unconditional
/// refresh rather than by statting the working tree, which is the very cost
/// being avoided.
pub fn fingerprint(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let git = dir.join(".git");
    for rel in ["HEAD", "index", "packed-refs", "MERGE_HEAD", "refs", "refs/heads"] {
        // Absence is state too: MERGE_HEAD disappearing is a merge finishing.
        match std::fs::metadata(git.join(rel)) {
            Ok(m) => {
                rel.hash(&mut h);
                m.len().hash(&mut h);
                if let Ok(t) = m.modified()
                    && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
                {
                    d.as_nanos().hash(&mut h);
                }
            }
            Err(_) => (rel, u64::MAX).hash(&mut h),
        }
    }
    // A linked worktree's `.git` is a file pointing elsewhere; hashing it too
    // costs one stat and keeps repos like that from all fingerprinting alike.
    if let Ok(m) = std::fs::metadata(&git) {
        m.is_file().hash(&mut h);
        m.len().hash(&mut h);
    }
    h.finish()
}

pub fn status(dir: &Path) -> Result<RepoStatus> {
    // NUL-separated so paths with spaces, quotes or newlines survive intact.
    let raw = git(dir, &["status", "--porcelain=v1", "-b", "-z"])?;
    Ok(parse_status(&raw))
}

/// Parse `git status --porcelain=v1 -b -z` output.
pub fn parse_status(raw: &str) -> RepoStatus {
    let mut out = RepoStatus::default();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    // Iterate manually: rename/copy entries consume a second field (the origin
    // path), which a plain for-loop would misread as another status line.
    let mut pending: Vec<&str> = Vec::new();
    for f in fields.by_ref() {
        pending.push(f);
    }
    let mut i = 0;
    while i < pending.len() {
        let line = pending[i];
        i += 1;
        if let Some(rest) = line.strip_prefix("## ") {
            parse_branch_header(rest, &mut out);
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let mut chars = line.chars();
        let index = chars.next().unwrap_or(' ');
        let worktree = chars.next().unwrap_or(' ');
        let path = line[2..].trim_start().to_string();
        if index == 'R' || index == 'C' || worktree == 'R' || worktree == 'C' {
            // The following field is the rename/copy source; skip it.
            i += 1;
        }
        out.entries.push(StatusEntry {
            index,
            worktree,
            path,
        });
    }
    out
}

/// Parse the `## main...origin/main [ahead 1, behind 2]` header.
fn parse_branch_header(rest: &str, out: &mut RepoStatus) {
    let (names, tracking) = match rest.split_once(" [") {
        Some((n, t)) => (n, t.trim_end_matches(']')),
        None => (rest, ""),
    };
    let (local, upstream) = match names.split_once("...") {
        Some((l, u)) => (l, Some(u)),
        None => (names, None),
    };
    // Detached HEAD reads `## HEAD (no branch)`.
    out.detached = local.trim().ends_with("(no branch)");
    out.branch = local.trim().trim_end_matches(" (no branch)").to_string();
    out.has_upstream = upstream.is_some();
    for part in tracking.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            out.ahead = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            out.behind = n.trim().parse().unwrap_or(0);
        }
    }
}

pub fn stage(dir: &Path, path: &str) -> Result<()> {
    git(dir, &["add", "--", path]).map(|_| ())
}

pub fn stage_all(dir: &Path) -> Result<()> {
    git(dir, &["add", "-A"]).map(|_| ())
}

pub fn unstage(dir: &Path, path: &str) -> Result<()> {
    // `reset -q HEAD --` works on a repo with no commits yet, where
    // `restore --staged` errors out.
    git(dir, &["reset", "-q", "HEAD", "--", path]).map(|_| ())
}

/// Commit whatever is staged. Returns the short summary git prints.
///
/// Streams rather than buffers: `on_line` sees the `pre-commit` hook's output
/// while it is still running, so a test suite that takes a minute reports its
/// progress instead of looking like a hang.
pub fn commit(
    dir: &Path,
    message: &str,
    on_line: &mut dyn FnMut(String, bool),
) -> Result<String> {
    let (ok, lines) = git_streaming(dir, &["commit", "-m", message], on_line)?;
    let text = lines.join("\n");
    if !ok {
        return Err(command_failure(dir, "commit", "pre-commit", &text));
    }
    // Not `first_line`: the hook has already printed by the time git says
    // anything, so the first line of a hooked commit belongs to pytest. git's
    // own summary is its `[branch sha] message` line, and the last one of those
    // is git's even if a hook printed something bracketed of its own.
    Ok(text
        .lines()
        .rev()
        .find(|l| l.starts_with('['))
        .unwrap_or("committed")
        .to_string())
}

/// Push the current branch, setting upstream when there isn't one.
///
/// Streams for the same reason `commit` does — `pre-push` hooks are where the
/// slow end of a test suite tends to live.
pub fn push(
    dir: &Path,
    branch: &str,
    has_upstream: bool,
    on_line: &mut dyn FnMut(String, bool),
) -> Result<String> {
    let args: Vec<&str> = if has_upstream {
        vec!["push"]
    } else {
        vec!["push", "--set-upstream", "origin", branch]
    };
    let (ok, lines) = git_streaming(dir, &args, on_line)?;
    if !ok {
        return Err(command_failure(dir, "push", "pre-push", &lines.join("\n")));
    }
    // git narrates a push over stderr — object counts, deltas, the remote's
    // replies — and with a `pre-push` hook the narration starts with the hook's.
    // None of it summarises anything, and all of it is in the pane, so the
    // status line says the one thing that matters.
    Ok(format!("pushed {branch}"))
}

/// Turn a failed streaming command into the one line the status bar gets.
///
/// The full output is already on screen in the op pane, so this only has to say
/// *which* thing failed. git's own `fatal:`/`error:` lines are preferred when
/// present because they are the precise diagnosis; otherwise a repo with the
/// relevant hook installed gets blamed on the hook, which is right far more
/// often than not — git itself rarely fails silently.
fn command_failure(dir: &Path, verb: &str, hook: &str, output: &str) -> anyhow::Error {
    if let Some(line) = output
        .lines()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
    {
        return anyhow!("{}", line.trim());
    }
    if hook_path(dir, hook).is_some() {
        return anyhow!("{hook} hook failed — {verb} aborted");
    }
    anyhow!("git {verb}: {}", last_line(output))
}

/// One `git diff` invocation's output, labelled with what it compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSection {
    pub label: &'static str,
    pub text: String,
}

/// Flags shared by every diff we run: no colour codes to strip back out, and no
/// user-configured external differ, whose output would be anything at all.
const DIFF_FLAGS: [&str; 3] = ["diff", "--no-color", "--no-ext-diff"];

/// The diff for one working-tree entry, split into the parts that actually
/// exist: what is staged, what is not, or — for a file git has never seen — its
/// whole content.
///
/// A file that is staged *and* further modified yields both sections, because
/// "what am I about to commit" and "what would I still be leaving behind" are
/// different questions and the status view shows the file only once.
pub fn file_diff(dir: &Path, entry: &StatusEntry) -> Result<Vec<DiffSection>> {
    let mut out = Vec::new();
    let mut section = |label: &'static str, args: &[&str]| -> Result<()> {
        let mut argv: Vec<&str> = DIFF_FLAGS.to_vec();
        argv.extend_from_slice(args);
        let text = git_diff_cmd(dir, &argv)?;
        if !text.trim().is_empty() {
            out.push(DiffSection { label, text });
        }
        Ok(())
    };

    if entry.is_untracked() {
        // Plain `git diff` cannot see an untracked file at all. Diffing it
        // against /dev/null renders it as one big addition — and still prints
        // "Binary files differ" rather than dumping bytes into the terminal.
        section(
            "untracked — entire file",
            &["--no-index", "--", "/dev/null", &entry.path],
        )?;
        return Ok(out);
    }
    if entry.is_staged() {
        section("staged — index vs HEAD", &["--cached", "--", &entry.path])?;
    }
    if entry.worktree != ' ' {
        section("unstaged — working tree vs index", &["--", &entry.path])?;
    }
    Ok(out)
}

/// Short SHA of HEAD, for reporting what was just committed.
pub fn head_sha(dir: &Path) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fingerprint_holds_still_until_git_state_moves() {
        let dir = std::env::temp_dir().join(format!("jog-fp-test-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let a = fingerprint(&dir);
        assert_eq!(a, fingerprint(&dir), "nothing moved, nothing changes");
        // A merge starting is a new file under `.git`; its existence alone
        // moves the print, no mtime granularity required.
        std::fs::write(git_dir.join("MERGE_HEAD"), "abc123\n").unwrap();
        assert_ne!(a, fingerprint(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_branch_header_with_tracking() {
        let s = parse_status("## main...origin/main [ahead 2, behind 1]\0");
        assert_eq!(s.branch, "main");
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert!(s.has_upstream);
        assert!(s.is_clean());
    }

    #[test]
    fn parses_branch_header_without_upstream() {
        let s = parse_status("## feature/x\0");
        assert_eq!(s.branch, "feature/x");
        assert!(!s.has_upstream);
        assert_eq!((s.ahead, s.behind), (0, 0));
    }


    #[test]
    fn branch_is_reported_even_when_clean() {
        // The dashboard shows the local branch from this same call, so a clean
        // tree must still yield the branch name rather than an empty status.
        let s = parse_status("## new-design...origin/new-design\0");
        assert_eq!(s.branch, "new-design");
        assert!(s.is_clean());
    }

    #[test]
    fn branch_with_slashes_survives() {
        let s = parse_status("## dependabot/pub/multi-9e40...origin/dependabot/pub/multi-9e40\0");
        assert_eq!(s.branch, "dependabot/pub/multi-9e40");
    }

    #[test]
    fn parses_entries_and_stage_state() {
        let s = parse_status("## main\0M  src/a.rs\0 M src/b.rs\0?? new.txt\0MM src/c.rs\0");
        assert_eq!(s.entries.len(), 4);

        let a = &s.entries[0];
        assert_eq!(a.path, "src/a.rs");
        assert!(a.is_staged() && !a.has_unstaged());

        let b = &s.entries[1];
        assert_eq!(b.path, "src/b.rs");
        assert!(!b.is_staged() && b.has_unstaged());

        let n = &s.entries[2];
        assert_eq!(n.path, "new.txt");
        assert!(n.is_untracked() && n.has_unstaged() && !n.is_staged());

        // Staged *and* further modified in the worktree.
        let c = &s.entries[3];
        assert!(c.is_staged() && c.has_unstaged());

        assert_eq!(s.staged_count(), 2);
        assert_eq!(s.unstaged_count(), 3);
    }

    #[test]
    fn rename_entry_consumes_its_source_path() {
        // Renames emit "R  new\0old\0" — the source must not become its own row.
        let s = parse_status("## main\0R  new.rs\0old.rs\0M  other.rs\0");
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].path, "new.rs");
        assert_eq!(s.entries[0].label(), "renamed");
        assert_eq!(s.entries[1].path, "other.rs");
    }

    #[test]
    fn paths_with_spaces_survive() {
        let s = parse_status("## main\0?? my file.txt\0");
        assert_eq!(s.entries[0].path, "my file.txt");
    }

    #[test]
    fn detached_head_reads_cleanly() {
        let s = parse_status("## HEAD (no branch)\0");
        assert_eq!(s.branch, "HEAD");
        assert!(!s.has_upstream);
        // Pushing this would create a remote branch named `HEAD`, so it has to
        // be distinguishable from a real branch.
        assert!(s.detached);
    }

    #[test]
    fn a_real_branch_is_not_detached() {
        assert!(!parse_status("## main...origin/main\0").detached);
        assert!(!parse_status("## feature/x\0").detached);
    }

    /// A throwaway repo with an identity of its own, so the test does not
    /// depend on (or touch) whatever git config the machine has.
    fn scratch_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jog-diff-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "jog test"],
        ] {
            git(&dir, &args).unwrap();
        }
        dir
    }

    /// Install an executable hook that runs `script`.
    #[cfg(unix)]
    fn install_hook(dir: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let hooks = dir.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A repo with one commit behind it and a second change staged — the state
    /// a user is in when they press `c`.
    #[cfg(unix)]
    fn staged_repo(name: &str) -> PathBuf {
        let dir = scratch_repo(name);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "a.txt"]).unwrap();
        git(&dir, &["commit", "-qm", "init"]).unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        git(&dir, &["add", "a.txt"]).unwrap();
        dir
    }

    #[test]
    #[cfg(unix)]
    fn a_hooks_output_is_reported_line_by_line() {
        let dir = staged_repo("hook-stream");
        install_hook(
            &dir,
            "pre-commit",
            "#!/bin/sh\necho 'ruff.....Passed'\necho 'pytest..Passed' >&2\nexit 0\n",
        );

        let mut seen = Vec::new();
        let summary = commit(&dir, "with hook", &mut |l, partial| {
            if !partial {
                seen.push(l);
            }
        })
        .unwrap();

        // Both streams, in one sequence: hooks write to whichever they like and
        // the reader does not care which.
        assert!(seen.iter().any(|l| l == "ruff.....Passed"), "{seen:?}");
        assert!(seen.iter().any(|l| l == "pytest..Passed"), "{seen:?}");
        assert!(summary.contains("with hook"), "{summary}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn output_arrives_while_the_hook_is_still_running() {
        use std::time::{Duration, Instant};
        let dir = staged_repo("hook-live");
        install_hook(
            &dir,
            "pre-commit",
            "#!/bin/sh\necho early\nsleep 1\necho late\nexit 0\n",
        );

        let start = Instant::now();
        let mut early_at = None;
        commit(&dir, "live", &mut |l, _| {
            if l == "early" {
                early_at = Some(start.elapsed());
            }
        })
        .unwrap();
        let total = start.elapsed();

        // The whole point of streaming: the first line is readable a second
        // before the command finishes, instead of the screen sitting blank and
        // then filling in all at once.
        let early = early_at.expect("the first line never arrived");
        assert!(total >= Duration::from_millis(900), "hook exited too fast to tell: {total:?}");
        assert!(
            early < Duration::from_millis(500),
            "first line took {early:?} of a {total:?} command — that is buffered, not streamed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn the_step_currently_running_is_visible_before_it_finishes() {
        use std::time::{Duration, Instant};
        let dir = staged_repo("hook-partial");
        // How `pre-commit` reports a step: name and dots the moment it starts,
        // no newline; the verdict — and the newline — only when it ends.
        install_hook(
            &dir,
            "pre-commit",
            "#!/bin/sh\nprintf 'pytest...................'\nsleep 1\nprintf 'Passed\\n'\nexit 0\n",
        );

        let start = Instant::now();
        let mut partial_at = None;
        let mut final_line = None;
        commit(&dir, "partial", &mut |l, partial| {
            if partial && l.starts_with("pytest") && partial_at.is_none() {
                partial_at = Some(start.elapsed());
            }
            if !partial && l.starts_with("pytest") {
                final_line = Some(l);
            }
        })
        .unwrap();
        let total = start.elapsed();

        // The step's name must show up while the step is still running — that
        // unterminated line is the only sign of what the wait is for.
        let at = partial_at.expect("the running step's name never arrived");
        assert!(total >= Duration::from_millis(900), "hook exited too fast to tell: {total:?}");
        assert!(
            at < Duration::from_millis(500),
            "step name took {at:?} of a {total:?} hook — the running step was invisible"
        );
        // And once the step ends, the completed line supersedes the partial.
        assert_eq!(
            final_line.as_deref(),
            Some("pytest...................Passed"),
            "the finished line should arrive whole"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_failing_hook_aborts_the_commit_and_keeps_its_output() {
        let dir = staged_repo("hook-fail");
        install_hook(
            &dir,
            "pre-commit",
            "#!/bin/sh\necho 'FAILED tests/test_api.py::test_login'\necho 'assert 1 == 2'\nexit 1\n",
        );

        let mut seen = Vec::new();
        let err = commit(&dir, "blocked", &mut |l, partial| {
            if !partial {
                seen.push(l);
            }
        })
        .unwrap_err();

        // The one-line message names the hook — the detail is in the output the
        // caller just collected, which is the whole reason it is collected.
        assert!(format!("{err}").contains("pre-commit hook failed"), "{err}");
        assert!(
            seen.iter().any(|l| l.contains("test_login")),
            "the failure has to survive the command that failed: {seen:?}"
        );
        // And nothing was committed: still just the repo's initial commit.
        let log = git(&dir, &["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_rewritten_progress_line_reports_its_final_state() {
        let dir = staged_repo("hook-progress");
        // What every test runner's progress bar looks like on a pipe.
        install_hook(
            &dir,
            "pre-commit",
            "#!/bin/sh\nprintf '10%%\\r50%%\\r100%% done\\n'\nexit 0\n",
        );

        let mut seen = Vec::new();
        commit(&dir, "progress", &mut |l, partial| {
            if !partial {
                seen.push(l);
            }
        })
        .unwrap();

        assert!(
            seen.iter().any(|l| l == "100% done"),
            "carriage returns should collapse to what a terminal would show: {seen:?}"
        );
        assert!(!seen.iter().any(|l| l.contains('\r')), "{seen:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn only_a_hook_git_would_actually_run_gets_named() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_repo("hook-detect");
        assert!(hook_path(&dir, "pre-commit").is_none());

        install_hook(&dir, "pre-commit", "#!/bin/sh\nexit 0\n");
        assert!(hook_path(&dir, "pre-commit").is_some());

        // git skips a hook that isn't executable, so labelling the pane after
        // it would be a lie about what is taking the time.
        let path = dir.join(".git/hooks/pre-commit");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(hook_path(&dir, "pre-commit").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn hook_detection_follows_core_hookspath() {
        let dir = scratch_repo("hook-path-config");
        let elsewhere = dir.join("tooling-hooks");
        std::fs::create_dir_all(&elsewhere).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let p = elsewhere.join("pre-commit");
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Not in `.git/hooks`, so a naive lookup would report no hook at all.
        git(&dir, &["config", "core.hooksPath", "tooling-hooks"]).unwrap();
        assert_eq!(hook_path(&dir, "pre-commit"), Some(elsewhere.join("pre-commit")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_separates_what_is_staged_from_what_is_not() {
        let dir = scratch_repo("split");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "a.txt"]).unwrap();
        git(&dir, &["commit", "-qm", "init"]).unwrap();

        // Stage one edit, then make a second one on top of it: the file is a
        // single status row but two genuinely different diffs.
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        git(&dir, &["add", "a.txt"]).unwrap();
        std::fs::write(dir.join("a.txt"), "three\n").unwrap();

        let status = status(&dir).unwrap();
        let entry = status.entries.iter().find(|e| e.path == "a.txt").unwrap();
        let sections = file_diff(&dir, entry).unwrap();
        assert_eq!(sections.len(), 2);
        assert!(sections[0].label.starts_with("staged"));
        assert!(sections[0].text.contains("+two"));
        assert!(sections[1].label.starts_with("unstaged"));
        assert!(sections[1].text.contains("+three"));
        assert!(sections[1].text.contains("-two"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untracked_file_diffs_as_all_additions() {
        let dir = scratch_repo("untracked");
        std::fs::write(dir.join("new.txt"), "hello\n").unwrap();
        let status = status(&dir).unwrap();
        let entry = &status.entries[0];
        assert!(entry.is_untracked());

        // Plain `git diff` shows nothing for an untracked file, and the
        // `--no-index` call it needs instead exits 1 — which must not read as
        // a failure.
        let sections = file_diff(&dir, entry).unwrap();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].text.contains("+hello"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_file_reports_rather_than_dumps() {
        let dir = scratch_repo("binary");
        std::fs::write(dir.join("q.png"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        git(&dir, &["add", "q.png"]).unwrap();
        git(&dir, &["commit", "-qm", "init"]).unwrap();
        std::fs::write(dir.join("q.png"), [0u8, 1, 2, 3, 4, 5, 6, 7]).unwrap();

        let status = status(&dir).unwrap();
        let sections = file_diff(&dir, &status.entries[0]).unwrap();
        // Raw bytes in the terminal would wreck the alternate screen.
        assert!(sections[0].text.contains("Binary files"));
        assert!(!sections[0].text.contains('\u{9f}'));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_skips_nested_and_noise() {
        let tmp = std::env::temp_dir().join(format!("jog-ws-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // A repo at depth 1, a repo at depth 2, a nested repo that must be
        // ignored, and a directory on the skip list.
        for p in [
            "alpha/.git",
            "group/beta/.git",
            "alpha/vendored/.git",
            "node_modules/pkg/.git",
        ] {
            std::fs::create_dir_all(tmp.join(p)).unwrap();
        }
        let found = discover_workspace(&tmp);
        // Compared as paths, not strings: the separator is `\` on Windows, so a
        // hardcoded "group/beta" fails there for a reason that has nothing to do
        // with what this test is checking.
        let names: Vec<PathBuf> = found
            .iter()
            .map(|p| p.strip_prefix(&tmp).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            names,
            vec![PathBuf::from("alpha"), ["group", "beta"].iter().collect::<PathBuf>()]
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
