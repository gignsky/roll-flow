use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::RfError;

// ── Repository discovery ──────────────────────────────────────────────────────

/// Resolve the top-level directory of the git repository containing `dir`.
///
/// Produces a clear, actionable error when `dir` is not inside a git repository
/// (the common "ran `rf` in the wrong place" case), rather than surfacing git's
/// raw `fatal:` text. Other git failures (e.g. the `git` binary missing) still
/// propagate as an IO error.
pub fn repo_root(dir: &Path) -> Result<PathBuf, RfError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if output.status.success() {
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    } else {
        Err(RfError::Git("not inside a git repository".to_string()))
    }
}

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

/// Every configured remote, in git's own order.
pub fn remotes(repo: &Path) -> Result<Vec<String>, RfError> {
    let out = capture_git(repo, &["remote"])?;
    Ok(out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// True if a remote named `name` is configured.
///
/// [`remote_branches`] silently yields nothing for a repo without a remote, so
/// callers that need to distinguish "no remote" from "no matching branches" —
/// like `rf prune` — must ask separately.
pub fn has_remote(repo: &Path, name: &str) -> bool {
    remotes(repo)
        .map(|rs| rs.iter().any(|r| r == name))
        .unwrap_or(false)
}

/// Remote-tracking ref names under one remote, e.g. `"origin/main"`.
///
/// Unlike [`remote_branches`] this keeps the `<remote>/` prefix and takes no
/// pattern: `rf clean` diffs the full set before and after a prune to report
/// exactly which stale refs were dropped, without parsing git's human-readable
/// fetch output.
pub fn remote_tracking_refs(repo: &Path, remote: &str) -> Result<Vec<String>, RfError> {
    let pattern = format!("refs/remotes/{remote}/");
    let out = capture_git(
        repo,
        &["for-each-ref", "--format=%(refname:short)", &pattern],
    )?;
    Ok(out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .collect())
}

/// The branch `<remote>/HEAD` points at (e.g. `"main"`), or `None` when unset.
///
/// This is the remote's own idea of its default branch, and the most reliable
/// base for containment checks in a repo `rf` knows nothing else about. It is
/// only set when the clone recorded it (or `git remote set-head` was run), so
/// callers must have a fallback.
pub fn remote_head_branch(repo: &Path, remote: &str) -> Option<String> {
    let refspec = format!("refs/remotes/{remote}/HEAD");
    let out = capture_git(repo, &["symbolic-ref", "--quiet", "--short", &refspec]).ok()?;
    let prefix = format!("{remote}/");
    out.strip_prefix(&prefix).map(ToString::to_string)
}

/// One local branch, as a single `for-each-ref` reports it.
///
/// Gathering tracking state and worktree occupancy together matters: `rf clean`
/// needs both for every branch, and asking per-branch would be one subprocess
/// each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBranch {
    /// `%(refname:short)`.
    pub name: String,
    /// `%(upstream:short)`, e.g. `"origin/main"`. Empty when none is configured.
    pub upstream: String,
    /// `%(upstream:remotename)`, e.g. `"origin"`. Empty when no upstream.
    pub remote_name: String,
    /// `%(upstream:track,nobracket)`: `"gone"` once the upstream is deleted,
    /// `"ahead 2"` / `"behind 1"` / `"ahead 1, behind 3"` when diverged, and
    /// empty both when in sync *and* when there is no upstream at all.
    pub track: String,
    /// `%(worktreepath)`: the worktree this branch is checked out in, empty when
    /// it is checked out nowhere. Populated for the main worktree too.
    pub worktree: String,
}

impl LocalBranch {
    /// True when the branch tracks an upstream that no longer exists.
    ///
    /// Only meaningful after a `--prune` fetch: git reports `gone` from the
    /// absence of the remote-tracking ref, which a stale cache still provides.
    pub fn upstream_gone(&self) -> bool {
        !self.upstream.is_empty() && matches!(self.track.trim(), "gone" | "[gone]")
    }

    /// True when some worktree has this branch checked out.
    pub fn is_checked_out(&self) -> bool {
        !self.worktree.is_empty()
    }
}

/// Field separator for [`local_branch_details`]. Tab is safe: git refnames
/// forbid ASCII control characters, so only the trailing path could contain one.
const FIELD_SEP: char = '\t';

/// Every local branch with its tracking and worktree state, in one git call.
pub fn local_branch_details(repo: &Path) -> Result<Vec<LocalBranch>, RfError> {
    const FORMAT: &str = concat!(
        "--format=%(refname:short)\t",
        "%(upstream:short)\t",
        "%(upstream:remotename)\t",
        "%(upstream:track,nobracket)\t",
        "%(worktreepath)",
    );
    let out = capture_git(repo, &["for-each-ref", FORMAT, "refs/heads/"])?;
    Ok(out.lines().filter_map(parse_local_branch_line).collect())
}

/// Parse one `--format` line from [`local_branch_details`].
///
/// `worktreepath` is last and taken as the remainder: it is a filesystem path
/// and so is the one field that could itself contain a tab. Pure, so the field
/// handling is unit-testable without a repo.
pub fn parse_local_branch_line(line: &str) -> Option<LocalBranch> {
    let mut fields = line.splitn(5, FIELD_SEP);
    let name = fields.next()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(LocalBranch {
        name,
        upstream: fields.next().unwrap_or_default().trim().to_string(),
        remote_name: fields.next().unwrap_or_default().trim().to_string(),
        track: fields.next().unwrap_or_default().trim().to_string(),
        // Not trimmed: a path may legitimately end in whitespace, and only
        // emptiness is ever asked of it.
        worktree: fields.next().unwrap_or_default().to_string(),
    })
}

/// True when the repository has truncated history.
///
/// Merge detection walks ancestry, so a shallow clone can report a branch as
/// uncontained when the connecting commits simply were not fetched.
pub fn is_shallow(repo: &Path) -> bool {
    capture_git(repo, &["rev-parse", "--is-shallow-repository"])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

// ── Mutating branch helpers ───────────────────────────────────────────────────

/// Refresh remote-tracking refs and drop the ones whose upstream is gone.
///
/// `rf` is otherwise local-only, so `origin/*` refs go stale: a branch may
/// already be deleted upstream, or exist there without a local ref. Commands
/// that act on the remote call this first so they act on current data.
pub fn fetch_prune(repo: &Path, remote: &str) -> Result<(), RfError> {
    run_git(repo, &["fetch", "--prune", remote])
}

/// [`fetch_prune`], with git's own output captured rather than inherited.
///
/// `fetch_prune` shells through `run_git`, which inherits stdio, so git prints
/// its ` - [deleted] … -> origin/x` table straight to the terminal. `rf clean`
/// prunes every remote and reports the dropped refs itself — by diffing
/// [`remote_tracking_refs`] across the call — so git's version would be both
/// duplicated and inconsistently formatted.
pub fn fetch_prune_quiet(repo: &Path, remote: &str) -> Result<(), RfError> {
    capture_git(repo, &["fetch", "--prune", "--quiet", remote]).map(|_| ())
}

/// Delete a local branch, unconditionally (`git branch -D`).
///
/// `-D` rather than `-d` is deliberate. `-d` refuses unless the branch is merged
/// into `HEAD` or its upstream, which answers the wrong question: it vetoes
/// correct deletions when an unrelated branch is checked out, and permits ones we
/// would not want when a descendant is. Callers are expected to have established
/// containment themselves (see [`is_ancestor`]) so the decision does not depend on
/// which branch happens to be checked out.
pub fn delete_local_branch(repo: &Path, branch: &str) -> Result<(), RfError> {
    capture_git(repo, &["branch", "-D", branch]).map(|_| ())
}

/// Delete branches on `remote` in a single push.
///
/// Batched because each push is a network round-trip and callers routinely have
/// a dozen-plus branches to remove. Git deletes the corresponding
/// remote-tracking refs as a side effect, so no follow-up prune is needed.
/// All-or-nothing: if the push fails, no branch in the batch was deleted.
pub fn delete_remote_branches(
    repo: &Path,
    remote: &str,
    branches: &[String],
) -> Result<(), RfError> {
    if branches.is_empty() {
        return Ok(());
    }
    let mut args = vec!["push", remote, "--delete"];
    args.extend(branches.iter().map(String::as_str));
    capture_git(repo, &args).map(|_| ())
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
    // (Must not contain NUL — process args are C strings.)
    const SEP: &str = "\x1eRF\x1e";
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

/// Number of commits reachable from `tip` but from none of `excludes` —
/// `git rev-list --count <tip> --not <e1> <e2> ...`.
///
/// Used to say how much a deletion would actually lose, by both `rf prune`/`rf
/// delete` (excluding the stable branch) and `rf clean` (excluding whatever base
/// it resolved). Returns 0 when `excludes` is empty: with nothing to exclude the
/// count would be the branch's entire history, which reads as a catastrophic
/// loss and is never what the caller means — an empty exclude set means the base
/// could not be resolved at all, so no honest claim about lost commits can be
/// made.
pub fn commits_not_in(repo: &Path, tip: &str, excludes: &[String]) -> Result<u32, RfError> {
    if excludes.is_empty() {
        return Ok(0);
    }
    let mut args = vec!["rev-list", "--count", tip, "--not"];
    args.extend(excludes.iter().map(String::as_str));
    let out = capture_git(repo, &args)?;
    out.trim()
        .parse()
        .map_err(|_| RfError::Git(format!("could not parse commit count from {out:?}")))
}

/// Return the best common ancestor of `a` and `b`.
pub fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String, RfError> {
    capture_git(repo, &["merge-base", a, b])
}

/// Return the full SHA of the resolved ref.
pub fn rev_parse(repo: &Path, refspec: &str) -> Result<String, RfError> {
    capture_git(repo, &["rev-parse", refspec])
}

/// Commits `branch` is ahead of / behind `origin/<branch>`, as `(ahead, behind)`.
///
/// `git rev-list --left-right --count <branch>...origin/<branch>` prints two
/// counts separated by a tab: the left side (commits on `branch` not on the
/// remote — *ahead*) and the right side (commits on the remote not on `branch`
/// — *behind*). Fails if either ref does not resolve.
pub fn ahead_behind(repo: &Path, branch: &str) -> Result<(u32, u32), RfError> {
    let spec = format!("{branch}...origin/{branch}");
    let out = capture_git(repo, &["rev-list", "--left-right", "--count", &spec])?;
    parse_ahead_behind(&out)
        .ok_or_else(|| RfError::Git(format!("could not parse ahead/behind counts from {out:?}")))
}

/// Parse the two whitespace-separated counts `git rev-list --left-right
/// --count` emits into `(ahead, behind)`. Pure so it can be unit-tested without
/// a repo.
pub fn parse_ahead_behind(out: &str) -> Option<(u32, u32)> {
    let mut fields = out.split_whitespace();
    let ahead = fields.next()?.parse().ok()?;
    let behind = fields.next()?.parse().ok()?;
    Some((ahead, behind))
}

// ── Blob reads ────────────────────────────────────────────────────────────────

/// Read `path` as it exists at `refspec` (`git show <ref>:<path>`).
///
/// Returns `Ok(None)` when the path does not exist at that ref — the common
/// "this repo has no Cargo.toml" / "the file was added later" case — so callers
/// can treat absence as data rather than as an error. Other git failures still
/// propagate.
pub fn show_file_at_ref(repo: &Path, refspec: &str, path: &str) -> Result<Option<String>, RfError> {
    let spec = format!("{refspec}:{path}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", &spec])
        .output()?;
    if output.status.success() {
        // Not trimmed: callers parse file content, where trailing newlines and
        // leading whitespace are meaningful.
        return Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("does not exist") || stderr.contains("exists on disk, but not in") {
        return Ok(None);
    }
    Err(RfError::Git(format!("`git show {spec}` failed: {stderr}")))
}

// ── Tags ──────────────────────────────────────────────────────────────────────

/// True if `tag` already exists. Checked under `refs/tags/` specifically so a
/// branch or commit sharing the name cannot be mistaken for the tag.
pub fn tag_exists(repo: &Path, tag: &str) -> bool {
    ref_exists(repo, &format!("refs/tags/{tag}"))
}

/// Create an annotated tag pointing at `target`.
pub fn create_annotated_tag(
    repo: &Path,
    tag: &str,
    message: &str,
    target: &str,
) -> Result<(), RfError> {
    run_git(repo, &["tag", "-a", tag, "-m", message, target])
}

/// Push a single tag to `remote`.
pub fn push_tag(repo: &Path, remote: &str, tag: &str) -> Result<(), RfError> {
    run_git(repo, &["push", remote, &format!("refs/tags/{tag}")])
}

// ── Commits ───────────────────────────────────────────────────────────────────

/// Stage `paths` and commit them with `message`.
///
/// Paths that do not exist are skipped rather than failing the whole commit, so
/// a caller can offer e.g. `["Cargo.toml", "Cargo.lock"]` without knowing
/// whether a lockfile is present.
pub fn commit_paths(repo: &Path, paths: &[&str], message: &str) -> Result<(), RfError> {
    let present: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|p| repo.join(p).exists())
        .collect();
    if present.is_empty() {
        return Err(RfError::Git(
            "nothing to commit: none of the requested paths exist".to_string(),
        ));
    }
    let mut add_args = vec!["add", "--"];
    add_args.extend_from_slice(&present);
    run_git(repo, &add_args)?;
    run_git(repo, &["commit", "-m", message])
}
#[cfg(test)]
mod tests {
    use super::{
        ahead_behind, local_branch_details, parse_ahead_behind, parse_local_branch_line,
        remote_head_branch, remote_tracking_refs, remotes,
    };
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git")
            .status
            .success();
        assert!(ok, "git {args:?} failed in {dir:?}");
    }

    #[test]
    fn ahead_behind_counts_local_vs_origin() {
        // A bare remote plus a clone, then diverge both sides on `feat`.
        let remote = tempfile::tempdir().expect("remote dir");
        let local = tempfile::tempdir().expect("local dir");
        let (rp, lp) = (remote.path(), local.path());

        git(rp, &["init", "-b", "main", "--bare"]);

        // Seed via a working clone so we can push an initial `feat`.
        let seed = tempfile::tempdir().expect("seed dir");
        let sp = seed.path();
        git(sp, &["clone", rp.to_str().unwrap(), "."]);
        git(sp, &["config", "user.email", "t@e.test"]);
        git(sp, &["config", "user.name", "t"]);
        git(sp, &["commit", "--allow-empty", "-m", "init"]);
        git(sp, &["branch", "feat"]);
        git(sp, &["push", "origin", "main", "feat"]);

        // Clone into `local`; it now has origin/feat.
        git(lp, &["clone", rp.to_str().unwrap(), "."]);
        git(lp, &["config", "user.email", "t@e.test"]);
        git(lp, &["config", "user.name", "t"]);
        git(lp, &["switch", "feat"]);
        // Two local-only commits → ahead 2.
        git(lp, &["commit", "--allow-empty", "-m", "local a"]);
        git(lp, &["commit", "--allow-empty", "-m", "local b"]);

        // Advance origin's feat by one commit from the seed, then fetch.
        git(sp, &["switch", "feat"]);
        git(sp, &["commit", "--allow-empty", "-m", "remote c"]);
        git(sp, &["push", "origin", "feat"]);
        git(lp, &["fetch", "origin"]);

        assert_eq!(ahead_behind(lp, "feat").unwrap(), (2, 1));
    }

    #[test]
    fn parses_left_right_counts() {
        assert_eq!(parse_ahead_behind("3\t5"), Some((3, 5)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        // Spaces are tolerated too.
        assert_eq!(parse_ahead_behind("2 4"), Some((2, 4)));
        // Malformed / short input yields None rather than a panic.
        assert_eq!(parse_ahead_behind(""), None);
        assert_eq!(parse_ahead_behind("7"), None);
        assert_eq!(parse_ahead_behind("a\tb"), None);
    }

    // ── Branch detail parsing ─────────────────────────────────────────────────

    #[test]
    fn parses_a_branch_with_no_upstream() {
        let b = parse_local_branch_line("feature/x\t\t\t\t").expect("parse");
        assert_eq!(b.name, "feature/x");
        assert!(b.upstream.is_empty());
        assert!(b.remote_name.is_empty());
        assert!(b.track.is_empty());
        assert!(!b.is_checked_out());
        // An empty `track` must not read as "gone" when there is no upstream at
        // all — that is the difference between "never pushed" and "deleted
        // upstream", and only the latter is safe to treat as cleanup material.
        assert!(!b.upstream_gone());
    }

    #[test]
    fn parses_a_gone_upstream() {
        let b =
            parse_local_branch_line("roll/101\torigin/roll/101\torigin\tgone\t").expect("parse");
        assert_eq!(b.remote_name, "origin");
        assert!(b.upstream_gone());
    }

    #[test]
    fn parses_a_bracketed_gone_upstream() {
        // Defensive: `nobracket` yields bare `gone`, but accept the bracketed
        // form too rather than silently classifying the branch as in-sync.
        let b = parse_local_branch_line("x\torigin/x\torigin\t[gone]\t").expect("parse");
        assert!(b.upstream_gone());
    }

    #[test]
    fn parses_a_diverged_branch_as_not_gone() {
        let b = parse_local_branch_line("main\torigin/main\torigin\tahead 5\t").expect("parse");
        assert_eq!(b.track, "ahead 5");
        assert!(!b.upstream_gone());
    }

    #[test]
    fn parses_a_worktree_path_containing_a_tab() {
        // The path is the final field precisely so an embedded tab cannot shift
        // the tracking columns.
        let b = parse_local_branch_line("x\torigin/x\torigin\t\t/tmp/od\td/wt").expect("parse");
        assert_eq!(b.track, "");
        assert_eq!(b.worktree, "/tmp/od\td/wt");
        assert!(b.is_checked_out());
    }

    #[test]
    fn skips_blank_lines() {
        assert!(parse_local_branch_line("").is_none());
        assert!(parse_local_branch_line("\t\t\t\t").is_none());
    }

    // ── Branch details against a real repo ────────────────────────────────────

    #[test]
    fn reports_gone_upstream_after_a_pruning_fetch() {
        let remote = tempfile::tempdir().expect("remote dir");
        let local = tempfile::tempdir().expect("local dir");
        let (rp, lp) = (remote.path(), local.path());

        git(rp, &["init", "-b", "main", "--bare"]);
        git(lp, &["clone", rp.to_str().unwrap(), "."]);
        git(lp, &["config", "user.email", "t@e.test"]);
        git(lp, &["config", "user.name", "t"]);
        git(lp, &["commit", "--allow-empty", "-m", "init"]);
        git(lp, &["push", "-u", "origin", "main"]);
        git(lp, &["switch", "-c", "roll/101"]);
        git(lp, &["commit", "--allow-empty", "-m", "work"]);
        git(lp, &["push", "-u", "origin", "roll/101"]);
        git(lp, &["switch", "main"]);

        // Another host merges and deletes the branch upstream.
        git(rp, &["branch", "-D", "roll/101"]);

        // Before the prune the stale remote-tracking ref still answers for it,
        // so git reports the branch as in sync. This is exactly what makes a
        // detect-then-prune ordering wrong.
        let before = local_branch_details(lp).expect("details");
        let roll = before.iter().find(|b| b.name == "roll/101").expect("roll");
        assert!(!roll.upstream_gone(), "stale ref should still look in-sync");

        git(lp, &["fetch", "--prune", "origin"]);

        let after = local_branch_details(lp).expect("details");
        let roll = after.iter().find(|b| b.name == "roll/101").expect("roll");
        assert!(roll.upstream_gone(), "upstream should be gone after prune");
        assert_eq!(roll.remote_name, "origin");

        // `main` is checked out here, `roll/101` is not.
        let main = after.iter().find(|b| b.name == "main").expect("main");
        assert!(main.is_checked_out());
        assert!(!roll.is_checked_out());
    }

    #[test]
    fn lists_remotes_and_their_tracking_refs() {
        let remote = tempfile::tempdir().expect("remote dir");
        let local = tempfile::tempdir().expect("local dir");
        let (rp, lp) = (remote.path(), local.path());

        git(rp, &["init", "-b", "main", "--bare"]);
        git(lp, &["clone", rp.to_str().unwrap(), "."]);
        git(lp, &["config", "user.email", "t@e.test"]);
        git(lp, &["config", "user.name", "t"]);
        git(lp, &["commit", "--allow-empty", "-m", "init"]);
        git(lp, &["push", "-u", "origin", "main"]);

        assert_eq!(remotes(lp).expect("remotes"), vec!["origin".to_string()]);

        let refs = remote_tracking_refs(lp, "origin").expect("refs");
        assert!(refs.contains(&"origin/main".to_string()));
        // The `origin/HEAD` symref is not a branch and must not be offered as
        // one to the prune diff.
        assert!(!refs.iter().any(|r| r.ends_with("/HEAD")));

        // A fresh `git clone` records origin/HEAD; assert only when present so
        // this does not depend on the git version's clone behavior.
        if let Some(head) = remote_head_branch(lp, "origin") {
            assert_eq!(head, "main");
        }
    }

    #[test]
    fn reports_no_remotes_for_a_bare_local_repo() {
        let dir = tempfile::tempdir().expect("dir");
        git(dir.path(), &["init", "-b", "main"]);
        assert!(remotes(dir.path()).expect("remotes").is_empty());
    }
}
