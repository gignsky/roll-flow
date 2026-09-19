use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::core::proc;
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

/// Build a `git -C <repo> <args...>` command without running it.
///
/// Shared by the runners below and by callers that need to add environment
/// variables (the sync commands set `GIT_TERMINAL_PROMPT`) before spawning.
pub fn git_command(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    cmd
}

/// Run a git command in `repo`, returning an error if it exits non-zero.
///
/// Goes through [`crate::core::proc::run`], so git's output is inherited by the
/// terminal normally and relayed to the TUI's output panel when the calling
/// thread has a sink installed.
pub fn run_git(repo: &Path, args: &[&str]) -> Result<(), RfError> {
    let status = proc::run(&mut git_command(repo, args))?;
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

/// Run `cmd` as a git invocation described by `label`, folding git's stderr into
/// the error message on failure.
///
/// [`run_git`] cannot do this: it inherits stdio, so by the time it sees a
/// non-zero status the diagnosis has already gone to the terminal and only the
/// exit code is left. The sync commands need the text — deciding whether a push
/// was rejected as non-fast-forward, or refused because the lease was stale,
/// means reading what git actually said.
///
/// Output still reaches an installed sink; the returned `stderr` is a *copy*
/// collected alongside, not a diversion of it.
pub fn run_git_capturing_stderr(mut cmd: Command, label: &str) -> Result<(), GitFailure> {
    let (tx, rx) = std::sync::mpsc::channel();
    let collector = std::thread::spawn(move || {
        rx.iter()
            .filter(|line: &proc::OutLine| line.is_err())
            .map(|line| line.text().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    });

    let status = proc::run_teed(&mut cmd, tx);
    let stderr = collector.join().unwrap_or_default();

    match status {
        Err(err) => Err(GitFailure {
            stderr,
            message: format!("`{label}` could not run: {err}"),
        }),
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(GitFailure {
            message: format!("`{label}` exited with {status}"),
            stderr,
        }),
    }
}

/// A failed git invocation, with the stderr text that explains it.
#[derive(Debug, Clone)]
pub struct GitFailure {
    /// Everything git wrote to stderr, newline-joined.
    pub stderr: String,
    /// One-line summary, used when `stderr` is empty.
    pub message: String,
}

impl std::fmt::Display for GitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stderr.trim().is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}", self.stderr.trim())
        }
    }
}

impl std::error::Error for GitFailure {}

impl From<GitFailure> for RfError {
    fn from(err: GitFailure) -> Self {
        RfError::Git(err.to_string())
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

/// How a local branch sits relative to its configured upstream.
///
/// Derived from `%(upstream:track)`, which git already computes during the
/// single [`local_branch_details`] call — far cheaper than an
/// [`ahead_behind`] subprocess per branch when the whole list needs the answer
/// at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackState {
    /// No upstream is configured for this branch.
    NoUpstream,
    /// An upstream is configured but its remote-tracking ref is gone.
    Gone,
    /// In sync with the upstream.
    InSync,
    /// Commits here that the upstream lacks.
    Ahead(u32),
    /// Commits on the upstream that are missing here.
    Behind(u32),
    /// Both, in either direction.
    Diverged { ahead: u32, behind: u32 },
}

impl TrackState {
    /// True when pushing would be rejected as non-fast-forward, so a push must
    /// either be forced or preceded by a pull.
    pub fn needs_force_to_push(&self) -> bool {
        matches!(self, TrackState::Behind(_) | TrackState::Diverged { .. })
    }
}

/// Classify a branch's relationship to its upstream.
///
/// The `track` field is empty in *two* different situations — in sync, and no
/// upstream at all — so the upstream field is what separates them. Pure, so the
/// whole table is testable without a repo.
pub fn track_state(branch: &LocalBranch) -> TrackState {
    if branch.upstream.is_empty() {
        return TrackState::NoUpstream;
    }
    let track = branch.track.trim();
    if matches!(track, "gone" | "[gone]") {
        return TrackState::Gone;
    }
    match (
        parse_track_count(track, "ahead"),
        parse_track_count(track, "behind"),
    ) {
        (0, 0) => TrackState::InSync,
        (ahead, 0) => TrackState::Ahead(ahead),
        (0, behind) => TrackState::Behind(behind),
        (ahead, behind) => TrackState::Diverged { ahead, behind },
    }
}

/// Pull the number out of an `ahead 2`/`behind 1` fragment of a `track` string,
/// returning 0 when the keyword is absent.
///
/// Hand-rolled rather than regex: the crate has no regex dependency, and the
/// grammar git emits here is fixed at `"ahead N"`, `"behind N"`, or
/// `"ahead N, behind M"`.
fn parse_track_count(track: &str, keyword: &str) -> u32 {
    track
        .split(',')
        .filter_map(|part| part.trim().strip_prefix(keyword))
        .filter_map(|rest| rest.trim().parse().ok())
        .next()
        .unwrap_or(0)
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

/// Read one `path` at many refs in a single `git cat-file --batch`.
///
/// Returns a map from the `<refspec>:<path>` spec that was asked for to the
/// file's contents, omitting the specs where the path does not exist.
///
/// One subprocess for the whole batch rather than a [`show_file_at_ref`] per
/// ref, for the same reason [`local_branch_details`] is one `for-each-ref`: the
/// caller wants an answer for every branch on screen, on every reload, and N
/// forks a keypress is a cost the user feels. `--batch` takes the specs on
/// stdin and answers each with an `<oid> <type> <size>` header followed by the
/// bytes, or `<spec> missing` — so "this branch has no Cargo.toml" arrives as
/// data rather than as an error, exactly as it does from `show_file_at_ref`.
pub fn show_files_at_refs(
    repo: &Path,
    specs: &[String],
) -> Result<HashMap<String, String>, RfError> {
    if specs.is_empty() {
        return Ok(HashMap::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    // Written and dropped before the output is read: `--batch` streams answers
    // as it goes, and holding stdin open while draining a pipe that git is
    // still filling is how this would deadlock on a large batch.
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| RfError::Git("cat-file --batch: no stdin".to_string()))?;
        for spec in specs {
            writeln!(stdin, "{spec}")?;
        }
    }
    drop(child.stdin.take());

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(RfError::Git("`git cat-file --batch` failed".to_string()));
    }
    Ok(parse_cat_file_batch(&output.stdout, specs))
}

/// Split `cat-file --batch` output into one entry per spec that resolved.
///
/// The stream is bytes, not lines: a header names a byte count and exactly that
/// many bytes of content follow, then a newline. Walking it by that count —
/// rather than splitting on newlines — is what keeps a file containing a line
/// that looks like a header from derailing the parse. Specs are consumed in
/// order, which is how each answer is matched back to what was asked: git
/// echoes the *spec* only on a miss, and an oid on a hit.
fn parse_cat_file_batch(stdout: &[u8], specs: &[String]) -> HashMap<String, String> {
    let mut found = HashMap::new();
    let mut at = 0usize;
    let mut spec_iter = specs.iter();

    while at < stdout.len() {
        let Some(end) = stdout[at..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&stdout[at..at + end]).to_string();
        at += end + 1;
        let Some(spec) = spec_iter.next() else { break };

        // `<oid> <type> <size>` on a hit; anything else (`<spec> missing`,
        // `<spec> ambiguous`) means there is no content to step over.
        let size = header
            .rsplit_once(' ')
            .and_then(|(rest, size)| size.parse::<usize>().ok().filter(|_| rest.contains(' ')));
        let Some(size) = size else { continue };
        if at + size > stdout.len() {
            break;
        }
        found.insert(
            spec.clone(),
            String::from_utf8_lossy(&stdout[at..at + size]).to_string(),
        );
        // The content is followed by a newline that belongs to neither.
        at += size + 1;
    }
    found
}

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
        ahead_behind, local_branch_details, parse_ahead_behind, parse_cat_file_batch,
        parse_local_branch_line, remote_head_branch, remote_tracking_refs, remotes,
        show_files_at_refs, track_state, LocalBranch, TrackState,
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

    /// Build a `LocalBranch` carrying only the two fields `track_state` reads.
    fn tracked(upstream: &str, track: &str) -> LocalBranch {
        LocalBranch {
            name: "feature/x".to_string(),
            upstream: upstream.to_string(),
            remote_name: if upstream.is_empty() {
                String::new()
            } else {
                "origin".to_string()
            },
            track: track.to_string(),
            worktree: String::new(),
        }
    }

    #[test]
    fn an_empty_upstream_is_no_upstream_not_in_sync() {
        // `track` is empty in both cases; only the upstream field separates them.
        assert_eq!(track_state(&tracked("", "")), TrackState::NoUpstream);
        assert_eq!(
            track_state(&tracked("origin/feature/x", "")),
            TrackState::InSync
        );
    }

    #[test]
    fn a_gone_upstream_is_reported_in_both_spellings() {
        assert_eq!(
            track_state(&tracked("origin/feature/x", "gone")),
            TrackState::Gone
        );
        assert_eq!(
            track_state(&tracked("origin/feature/x", "[gone]")),
            TrackState::Gone
        );
    }

    #[test]
    fn ahead_behind_and_divergence_are_parsed_from_track() {
        assert_eq!(
            track_state(&tracked("origin/feature/x", "ahead 2")),
            TrackState::Ahead(2)
        );
        assert_eq!(
            track_state(&tracked("origin/feature/x", "behind 3")),
            TrackState::Behind(3)
        );
        assert_eq!(
            track_state(&tracked("origin/feature/x", "ahead 1, behind 4")),
            TrackState::Diverged {
                ahead: 1,
                behind: 4
            }
        );
    }

    #[test]
    fn unrecognised_track_text_degrades_to_in_sync_rather_than_erroring() {
        // Never panics or misreports divergence on text we did not anticipate:
        // showing "in sync" is wrong but harmless, whereas a panic kills the TUI.
        assert_eq!(
            track_state(&tracked("origin/feature/x", "something new")),
            TrackState::InSync
        );
    }

    #[test]
    fn only_behind_and_diverged_require_a_force_push() {
        assert!(!TrackState::NoUpstream.needs_force_to_push());
        assert!(!TrackState::InSync.needs_force_to_push());
        assert!(!TrackState::Ahead(3).needs_force_to_push());
        assert!(!TrackState::Gone.needs_force_to_push());
        assert!(TrackState::Behind(1).needs_force_to_push());
        assert!(TrackState::Diverged {
            ahead: 1,
            behind: 1
        }
        .needs_force_to_push());
    }

    #[test]
    fn track_state_matches_what_git_reports_for_a_real_diverged_branch() {
        // Guards the parser against git changing its `%(upstream:track)` wording:
        // every pure test above encodes that wording as a literal, so nothing
        // else would notice if git started phrasing it differently.
        let remote = tempfile::tempdir().expect("remote dir");
        let local = tempfile::tempdir().expect("local dir");
        let seed = tempfile::tempdir().expect("seed dir");
        let (rp, lp, sp) = (remote.path(), local.path(), seed.path());

        git(rp, &["init", "-b", "main", "--bare"]);
        git(sp, &["clone", rp.to_str().unwrap(), "."]);
        git(sp, &["config", "user.email", "t@e.test"]);
        git(sp, &["config", "user.name", "t"]);
        git(sp, &["commit", "--allow-empty", "-m", "init"]);
        git(sp, &["push", "origin", "main"]);

        git(lp, &["clone", rp.to_str().unwrap(), "."]);
        git(lp, &["config", "user.email", "t@e.test"]);
        git(lp, &["config", "user.name", "t"]);
        git(lp, &["commit", "--allow-empty", "-m", "local a"]);
        git(lp, &["commit", "--allow-empty", "-m", "local b"]);

        git(sp, &["commit", "--allow-empty", "-m", "remote c"]);
        git(sp, &["push", "origin", "main"]);
        git(lp, &["fetch", "origin"]);

        let details = local_branch_details(lp).expect("branch details");
        let main = details.iter().find(|b| b.name == "main").expect("main");
        assert_eq!(
            track_state(main),
            TrackState::Diverged {
                ahead: 2,
                behind: 1
            },
            "track was {:?}",
            main.track
        );
    }

    #[test]
    fn cat_file_batch_walks_by_byte_count_not_by_lines() {
        // The parse has to step over content by the size the header declares.
        // This blob deliberately contains a line that looks like a `missing`
        // answer — split the stream on newlines instead and that line is read
        // as an answer to the next spec.
        let content = "version = \"0.2.4\"\nb:Cargo.toml missing\n";
        let mut stdout = Vec::new();
        stdout.extend_from_slice(format!("abc123 blob {}\n", content.len()).as_bytes());
        stdout.extend_from_slice(content.as_bytes());
        stdout.push(b'\n');
        stdout.extend_from_slice(b"b:Cargo.toml missing\n");

        let specs = vec!["a:Cargo.toml".to_string(), "b:Cargo.toml".to_string()];
        let found = parse_cat_file_batch(&stdout, &specs);

        assert_eq!(found.get("a:Cargo.toml").map(String::as_str), Some(content));
        // The ref that genuinely has no manifest is absent, not empty.
        assert!(!found.contains_key("b:Cargo.toml"));
    }

    #[test]
    fn cat_file_batch_reports_a_miss_as_absence() {
        let specs = vec!["nope:Cargo.toml".to_string()];
        let found = parse_cat_file_batch(b"nope:Cargo.toml missing\n", &specs);
        assert!(found.is_empty());

        // A truncated stream (git killed mid-write) yields what was complete
        // rather than a panic on an out-of-range slice.
        let truncated = b"abc123 blob 99\nshort";
        assert!(parse_cat_file_batch(truncated, &specs).is_empty());
    }

    #[test]
    fn show_files_at_refs_reads_the_manifest_at_several_refs_at_once() {
        // Against this very repo: HEAD has a Cargo.toml, a bogus ref does not,
        // and one subprocess answers for both.
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let specs = vec![
            "HEAD:Cargo.toml".to_string(),
            "HEAD:definitely-not-here.toml".to_string(),
        ];
        let found = show_files_at_refs(repo, &specs).expect("batch read");
        assert!(
            found["HEAD:Cargo.toml"].contains("[package]"),
            "{:?}",
            found.get("HEAD:Cargo.toml")
        );
        assert!(!found.contains_key("HEAD:definitely-not-here.toml"));
    }
}
