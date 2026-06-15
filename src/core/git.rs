use std::path::Path;
use std::process::Command;

use crate::error::RfError;

// ── Primitives ────────────────────────────────────────────────────────────────

/// Run a git command in `repo`, returning an error if it exits non-zero.
pub fn run_git(repo: &Path, args: &[&str]) -> Result<(), RfError> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(RfError::Git(format!(
            "`git {}` exited with {}",
            args.join(" "),
            status
        )))
    }
}

/// Run a git command in `repo`, capturing and returning trimmed stdout.
pub fn capture_git(repo: &Path, args: &[&str]) -> Result<String, RfError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(RfError::Git(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

// ── Branch helpers ────────────────────────────────────────────────────────────

pub fn current_branch(repo: &Path) -> Result<String, RfError> {
    capture_git(repo, &["branch", "--show-current"])
}

pub fn is_detached_head(repo: &Path) -> Result<bool, RfError> {
    let out = capture_git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    match out {
        Ok(_) => Ok(false),
        Err(_) => Ok(true),
    }
}

pub fn working_tree_clean(repo: &Path) -> Result<bool, RfError> {
    let out = capture_git(repo, &["status", "--porcelain"])?;
    Ok(out.trim().is_empty())
}

/// True if `refspec` resolves (local branch, remote branch, tag, commit, etc.).
pub fn ref_exists(repo: &Path, refspec: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", refspec])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve `branch` → local if it exists, else `origin/<branch>`, else None.
pub fn resolve_branch(repo: &Path, branch: &str) -> Option<String> {
    if ref_exists(repo, branch) {
        Some(branch.to_string())
    } else {
        let remote = format!("origin/{branch}");
        if ref_exists(repo, &remote) {
            Some(remote)
        } else {
            None
        }
    }
}

/// List local branches matching a glob pattern (e.g. `"roll/*"`).
pub fn local_branches(repo: &Path, pattern: &str) -> Result<Vec<String>, RfError> {
    let out = capture_git(repo, &["branch", "--list", pattern])?;
    Ok(out
        .lines()
        .map(|l| l.trim().trim_start_matches("* ").to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// List remote-tracking branches matching a glob pattern (returns bare names, origin/ stripped).
pub fn remote_branches(repo: &Path, pattern: &str) -> Result<Vec<String>, RfError> {
    let remote_pattern = format!("origin/{pattern}");
    let out = capture_git(repo, &["branch", "-r", "--list", &remote_pattern])?;
    Ok(out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.contains("->"))
        .map(|l| l.trim_start_matches("origin/").to_string())
        .collect())
}

// ── Log helpers ───────────────────────────────────────────────────────────────

/// Return commit subjects for the given log range / extra args.
/// `extra_args` are appended after `--format=%s`.  Pass e.g. `&["rolling"]`
/// to get all subjects on `rolling`.
pub fn log_subjects(repo: &Path, extra_args: &[&str]) -> Result<Vec<String>, RfError> {
    let mut args = vec!["log", "--format=%s"];
    args.extend_from_slice(extra_args);
    let out = capture_git(repo, &args)?;
    Ok(out
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Return `(subject, body)` pairs for the given log args.
/// Body is the raw commit body (everything after the first blank line).
pub fn log_with_body(repo: &Path, extra_args: &[&str]) -> Result<Vec<(String, String)>, RfError> {
    // Use a stable record separator that won't appear in real commit messages.
    const SEP: &str = "\x00RF\x00";
    let format = format!("--format=%s%n%b{SEP}");
    let mut args = vec!["log", &format];
    args.extend_from_slice(extra_args);
    let out = capture_git(repo, &args)?;

    let mut commits = Vec::new();
    for record in out.split(SEP) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let (subject, body) = record
            .split_once('\n')
            .map(|(s, b)| (s.trim().to_string(), b.trim().to_string()))
            .unwrap_or_else(|| (record.to_string(), String::new()));
        commits.push((subject, body));
    }
    Ok(commits)
}

// ── Diff helpers ──────────────────────────────────────────────────────────────

/// Files that differ between `from` and `to` (or a single range like `"A..B"`).
pub fn diff_name_only(repo: &Path, from: &str, to: &str) -> Result<Vec<String>, RfError> {
    let out = capture_git(repo, &["diff", "--name-only", from, to])?;
    Ok(out
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

// ── Ancestry / merge helpers ──────────────────────────────────────────────────

/// True if `candidate` is an ancestor of `descendant`.
pub fn is_ancestor(repo: &Path, candidate: &str, descendant: &str) -> Result<bool, RfError> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", candidate, descendant])
        .status()?;
    Ok(status.success())
}

/// Return the best common ancestor of `a` and `b`.
pub fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String, RfError> {
    capture_git(repo, &["merge-base", a, b])
}

/// Return the full SHA of the resolved ref.
pub fn rev_parse(repo: &Path, refspec: &str) -> Result<String, RfError> {
    capture_git(repo, &["rev-parse", refspec])
}
