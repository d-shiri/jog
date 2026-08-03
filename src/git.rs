//! Local git plumbing: workspace discovery plus the small slice of porcelain
//! needed to review and commit changes without leaving the TUI.
//!
//! Everything shells out to `git` rather than linking a library — the same
//! approach `provider::github` already uses for remote/branch lookup, and it
//! keeps the binary free of a libgit2 dependency.

use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

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
pub fn commit(dir: &Path, message: &str) -> Result<String> {
    let out = git(dir, &["commit", "-m", message])?;
    Ok(first_line(out.trim()))
}

/// Push the current branch, setting upstream when there isn't one.
pub fn push(dir: &Path, branch: &str, has_upstream: bool) -> Result<String> {
    let args: Vec<&str> = if has_upstream {
        vec!["push"]
    } else {
        vec!["push", "--set-upstream", "origin", branch]
    };
    let out = git(dir, &args)?;
    let text = out.trim();
    Ok(if text.is_empty() {
        format!("pushed {branch}")
    } else {
        first_line(text)
    })
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
