//! Pulling, pushing and fetching one selected branch.
//!
//! These are the first commands in `rf` that exist to talk to the remote for its
//! own sake rather than as a step in a workflow op, and the only ones reachable
//! from the TUI's `[p]` / `[P]` / `[f]` keys. The shapes here follow lazygit
//! closely on purpose — it is the tool this keymap is borrowed from, and its
//! handling of the awkward cases (no upstream, a branch that is not checked out,
//! a rejected push) is well-tested against real-world remotes.
//!
//! Planning is separated from running: [`pull_plan`] and [`push_args`] are pure
//! functions over a [`SyncTarget`], so which git command a keypress turns into is
//! decided by testable code rather than by branching inside the event loop.

use std::path::Path;
use std::process::Command;

use crate::core::branches::BranchLocation;
use crate::core::config::PullMode;
use crate::core::git::{self, GitFailure, TrackState};
use crate::error::RfError;

/// Everything the planners need to know about the branch the cursor is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTarget {
    pub branch: String,
    /// Whether this branch is the checked-out one, which decides whether a pull
    /// can touch the worktree.
    pub is_current: bool,
    pub location: BranchLocation,
    /// The branch's relationship to its upstream; `None` when there is no local
    /// copy to have one.
    pub track: Option<TrackState>,
    /// Remote to talk to: the configured upstream's remote, else `origin`.
    pub remote: String,
    /// Short branch name on the remote (`%(upstream:short)` minus the remote
    /// prefix). `None` when no upstream is configured.
    pub upstream_branch: Option<String>,
}

impl SyncTarget {
    /// Build a target for `branch` from the batch of branch details the view
    /// already loaded, so this costs no extra subprocess.
    pub fn resolve(
        branch: &str,
        current_branch: &str,
        location: BranchLocation,
        details: Option<&git::LocalBranch>,
    ) -> Self {
        let track = details.map(git::track_state);
        let (remote, upstream_branch) = match details {
            Some(d) if !d.upstream.is_empty() => {
                let remote = if d.remote_name.is_empty() {
                    "origin".to_string()
                } else {
                    d.remote_name.clone()
                };
                // `upstream` is `<remote>/<branch>`; the branch may itself
                // contain slashes (`roll/1-…`), so strip exactly the prefix.
                let short = d
                    .upstream
                    .strip_prefix(&format!("{remote}/"))
                    .unwrap_or(&d.upstream)
                    .to_string();
                (remote, Some(short))
            }
            _ => ("origin".to_string(), None),
        };
        SyncTarget {
            branch: branch.to_string(),
            is_current: branch == current_branch,
            location,
            track,
            remote,
            upstream_branch,
        }
    }

    fn has_local_copy(&self) -> bool {
        matches!(self.location, BranchLocation::Local | BranchLocation::Both)
    }
}

/// What `[p]` resolves to for a given target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullPlan {
    /// The branch is checked out, so a real `git pull` can run.
    Pull { args: Vec<String> },
    /// The branch exists locally but is not checked out. Advance its ref through
    /// a fetch refspec, which git refuses unless it is a fast-forward — so this
    /// cannot lose work and needs no worktree.
    FastForward { args: Vec<String> },
    /// No local copy: just update the remote-tracking ref.
    FetchRemote { args: Vec<String> },
    /// Nothing to pull from, with the reason to show the user.
    Refused { reason: String },
}

/// Decide what pulling `target` means.
pub fn pull_plan(target: &SyncTarget, mode: PullMode) -> PullPlan {
    if !target.has_local_copy() {
        return PullPlan::FetchRemote {
            args: owned(&[
                "fetch",
                "--no-write-fetch-head",
                &target.remote,
                &target.branch,
            ]),
        };
    }

    let upstream = match (&target.upstream_branch, target.track) {
        (_, Some(TrackState::Gone)) => {
            return PullPlan::Refused {
                reason: format!(
                    "upstream of '{}' is gone — press P to push it again",
                    target.branch
                ),
            }
        }
        (None, _) | (_, Some(TrackState::NoUpstream)) => {
            return PullPlan::Refused {
                reason: format!(
                    "'{}' has no upstream — press P to push and set one",
                    target.branch
                ),
            }
        }
        (Some(up), _) => up.clone(),
    };

    if target.is_current {
        // `--no-edit` keeps git from launching an editor for a merge commit
        // message behind the alternate screen.
        let mut args = owned(&["pull", "--no-edit"]);
        if let Some(flag) = mode.flag() {
            args.push(flag.to_string());
        }
        args.push(target.remote.clone());
        args.push(format!("refs/heads/{upstream}"));
        PullPlan::Pull { args }
    } else {
        PullPlan::FastForward {
            args: owned(&[
                "fetch",
                "--no-write-fetch-head",
                &target.remote,
                &format!("refs/heads/{upstream}:{}", target.branch),
            ]),
        }
    }
}

/// The `git push` argument list for `target`.
///
/// The local side of the refspec is always fully qualified, as lazygit does, so a
/// branch whose local and remote names differ still pushes to the right place.
/// Flag order mirrors lazygit's builder: force, then `--set-upstream`, then the
/// remote and refspec.
pub fn push_args(target: &SyncTarget, force: bool) -> Vec<String> {
    let mut args = vec!["push".to_string()];
    if force {
        // Bare `--force-with-lease`, never the `=<ref>:<sha>` form: git's own
        // remote-tracking expectation is exactly the check we want, and spelling
        // the expectation out by hand would defeat the point of the lease.
        args.push("--force-with-lease".to_string());
    }
    match &target.upstream_branch {
        Some(upstream) => {
            args.push(target.remote.clone());
            args.push(format!("refs/heads/{}:{upstream}", target.branch));
        }
        None => {
            args.push("--set-upstream".to_string());
            args.push(target.remote.clone());
            args.push(format!("refs/heads/{0}:{0}", target.branch));
        }
    }
    args
}

/// True when `target` is already known to be un-pushable without a force, so the
/// plain attempt can be skipped and the confirmation offered straight away.
pub fn needs_force_prompt(target: &SyncTarget) -> bool {
    target
        .track
        .map(|t| t.needs_force_to_push())
        .unwrap_or(false)
}

/// True when git's stderr says the push was refused as non-fast-forward.
///
/// Matched on text because git offers no machine-readable signal for it; this is
/// how lazygit decides too. Both spellings appear depending on git version and
/// whether the hint block is enabled.
pub fn is_rejection(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("updates were rejected")
        || lower.contains("non-fast-forward")
        || lower.contains("fetch first")
}

/// True when a `--force-with-lease` push was refused because the
/// remote-tracking ref is out of date rather than because of the remote's state.
///
/// This is the one case where lazygit falls back to a plain `--force`. We do not:
/// a stale lease means we genuinely do not know what is on the remote, and the
/// honest answer is to fetch and look before overwriting it.
pub fn is_stale_lease(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("stale info")
}

/// True when git could not authenticate without a terminal it does not have.
///
/// Sync commands run with `GIT_TERMINAL_PROMPT=0` so a credential request fails
/// immediately instead of blocking forever on a pipe the TUI is not reading.
pub fn is_credential_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("terminal prompts disabled")
        || lower.contains("could not read username")
        || lower.contains("could not read password")
        || lower.contains("authentication failed")
}

/// Build a git command for a sync operation, with the environment that keeps it
/// non-interactive.
///
/// `GIT_TERMINAL_PROMPT=0` turns a credential prompt into an immediate, readable
/// failure; without it git would block on a pipe nobody is reading and the job
/// would never finish. `GIT_SEQUENCE_EDITOR=:` neuters
/// `pull.rebase = interactive`, which would otherwise try to open an editor
/// behind the alternate screen.
pub fn sync_command(repo: &Path, args: &[String]) -> Command {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = git::git_command(repo, &refs);
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SEQUENCE_EDITOR", ":");
    cmd
}

/// Outcome of a push attempt that did not simply succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    /// Refused as non-fast-forward; forcing is the user's call.
    Rejected {
        stderr: String,
    },
}

/// Run a plain or forced push, classifying a non-fast-forward refusal rather
/// than treating it as a hard error.
pub fn run_push(repo: &Path, target: &SyncTarget, force: bool) -> Result<PushOutcome, SyncFailure> {
    let args = push_args(target, force);
    let label = format!("git {}", args.join(" "));
    match git::run_git_capturing_stderr(sync_command(repo, &args), &label) {
        Ok(()) => Ok(PushOutcome::Pushed),
        Err(failure) if !force && is_rejection(&failure.stderr) => Ok(PushOutcome::Rejected {
            stderr: failure.stderr,
        }),
        Err(failure) => Err(SyncFailure::from(failure)),
    }
}

/// Run a pull/fast-forward/fetch plan.
pub fn run_pull(repo: &Path, args: &[String]) -> Result<(), SyncFailure> {
    let label = format!("git {}", args.join(" "));
    git::run_git_capturing_stderr(sync_command(repo, args), &label).map_err(SyncFailure::from)
}

/// Fetch every remote-tracking ref, dropping the ones whose upstream is gone.
pub fn run_fetch(repo: &Path, remote: &str) -> Result<(), SyncFailure> {
    let args = owned(&["fetch", "--prune", "--no-write-fetch-head", remote]);
    run_pull(repo, &args)
}

/// A failed sync, with the hint that makes the failure actionable.
#[derive(Debug, Clone)]
pub struct SyncFailure {
    pub failure: GitFailure,
    /// Advice derived from the stderr text, shown under the git output.
    pub hint: Option<String>,
}

impl From<GitFailure> for SyncFailure {
    fn from(failure: GitFailure) -> Self {
        let hint = hint_for(&failure.stderr);
        SyncFailure { failure, hint }
    }
}

impl From<SyncFailure> for RfError {
    fn from(err: SyncFailure) -> Self {
        RfError::Git(err.failure.to_string())
    }
}

/// Turn a git failure into one line of advice, or nothing when we have none to
/// give. Pure, so the mapping is testable against real stderr text.
pub fn hint_for(stderr: &str) -> Option<String> {
    if is_stale_lease(stderr) {
        Some("remote ref is stale — press f to fetch, then retry".to_string())
    } else if is_credential_failure(stderr) {
        Some("needs credentials — run it in a shell, or press gg for lazygit".to_string())
    } else if is_rejection(stderr) {
        Some("rejected as non-fast-forward — press p to pull first".to_string())
    } else {
        None
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(is_current: bool, location: BranchLocation, track: Option<TrackState>) -> SyncTarget {
        SyncTarget {
            branch: "roll/1-alpha".to_string(),
            is_current,
            location,
            track,
            remote: "origin".to_string(),
            upstream_branch: track
                .map(|t| !matches!(t, TrackState::NoUpstream))
                .unwrap_or(false)
                .then(|| "roll/1-alpha".to_string()),
        }
    }

    #[test]
    fn pulling_the_checked_out_branch_runs_a_real_pull_with_the_configured_mode() {
        let t = target(true, BranchLocation::Both, Some(TrackState::Behind(2)));
        assert_eq!(
            pull_plan(&t, PullMode::FfOnly),
            PullPlan::Pull {
                args: owned(&[
                    "pull",
                    "--no-edit",
                    "--ff-only",
                    "origin",
                    "refs/heads/roll/1-alpha"
                ])
            }
        );
        assert_eq!(
            pull_plan(&t, PullMode::Rebase),
            PullPlan::Pull {
                args: owned(&[
                    "pull",
                    "--no-edit",
                    "--rebase",
                    "origin",
                    "refs/heads/roll/1-alpha"
                ])
            }
        );
        // Merge mode adds no flag at all, leaving git's own default in charge.
        assert_eq!(
            pull_plan(&t, PullMode::Merge),
            PullPlan::Pull {
                args: owned(&["pull", "--no-edit", "origin", "refs/heads/roll/1-alpha"])
            }
        );
    }

    #[test]
    fn pulling_a_branch_that_is_not_checked_out_fast_forwards_its_ref() {
        let t = target(false, BranchLocation::Both, Some(TrackState::Behind(1)));
        assert_eq!(
            pull_plan(&t, PullMode::FfOnly),
            PullPlan::FastForward {
                args: owned(&[
                    "fetch",
                    "--no-write-fetch-head",
                    "origin",
                    "refs/heads/roll/1-alpha:roll/1-alpha"
                ])
            }
        );
    }

    #[test]
    fn the_fast_forward_path_ignores_pull_mode_entirely() {
        // A fetch refspec cannot merge or rebase, so the mode is meaningless
        // here — and silently honouring it would imply otherwise.
        let t = target(false, BranchLocation::Both, Some(TrackState::Behind(1)));
        assert_eq!(
            pull_plan(&t, PullMode::Merge),
            pull_plan(&t, PullMode::Rebase)
        );
    }

    #[test]
    fn pulling_a_remote_only_branch_just_updates_the_tracking_ref() {
        let t = target(false, BranchLocation::Remote, None);
        assert_eq!(
            pull_plan(&t, PullMode::FfOnly),
            PullPlan::FetchRemote {
                args: owned(&["fetch", "--no-write-fetch-head", "origin", "roll/1-alpha"])
            }
        );
    }

    #[test]
    fn pulling_without_an_upstream_is_refused_with_advice() {
        let t = target(true, BranchLocation::Local, Some(TrackState::NoUpstream));
        match pull_plan(&t, PullMode::FfOnly) {
            PullPlan::Refused { reason } => assert!(reason.contains("no upstream"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn pulling_a_branch_whose_upstream_is_gone_is_refused_separately() {
        let t = target(true, BranchLocation::Both, Some(TrackState::Gone));
        match pull_plan(&t, PullMode::FfOnly) {
            PullPlan::Refused { reason } => assert!(reason.contains("gone"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn pushing_with_an_upstream_uses_a_fully_qualified_refspec() {
        let t = target(true, BranchLocation::Both, Some(TrackState::Ahead(1)));
        assert_eq!(
            push_args(&t, false),
            owned(&["push", "origin", "refs/heads/roll/1-alpha:roll/1-alpha"])
        );
    }

    #[test]
    fn pushing_without_an_upstream_sets_one() {
        let t = target(true, BranchLocation::Local, Some(TrackState::NoUpstream));
        assert_eq!(
            push_args(&t, false),
            owned(&[
                "push",
                "--set-upstream",
                "origin",
                "refs/heads/roll/1-alpha:roll/1-alpha"
            ])
        );
    }

    #[test]
    fn forcing_uses_force_with_lease_ahead_of_the_remote() {
        let t = target(
            true,
            BranchLocation::Both,
            Some(TrackState::Diverged {
                ahead: 1,
                behind: 2,
            }),
        );
        assert_eq!(
            push_args(&t, true),
            owned(&[
                "push",
                "--force-with-lease",
                "origin",
                "refs/heads/roll/1-alpha:roll/1-alpha"
            ])
        );
        // Never the explicit-expectation form: that would defeat the lease.
        assert!(!push_args(&t, true)
            .iter()
            .any(|a| a.contains("--force-with-lease=")));
    }

    #[test]
    fn a_push_that_sets_upstream_can_still_be_forced() {
        let t = target(true, BranchLocation::Local, Some(TrackState::NoUpstream));
        assert_eq!(
            push_args(&t, true),
            owned(&[
                "push",
                "--force-with-lease",
                "--set-upstream",
                "origin",
                "refs/heads/roll/1-alpha:roll/1-alpha"
            ])
        );
    }

    #[test]
    fn a_remote_named_something_other_than_origin_is_honoured() {
        let mut t = target(true, BranchLocation::Both, Some(TrackState::Ahead(1)));
        t.remote = "upstream".to_string();
        assert_eq!(
            push_args(&t, false),
            owned(&["push", "upstream", "refs/heads/roll/1-alpha:roll/1-alpha"])
        );
    }

    #[test]
    fn a_remote_branch_name_that_differs_from_the_local_one_is_preserved() {
        let mut t = target(true, BranchLocation::Both, Some(TrackState::Ahead(1)));
        t.upstream_branch = Some("renamed".to_string());
        assert_eq!(
            push_args(&t, false),
            owned(&["push", "origin", "refs/heads/roll/1-alpha:renamed"])
        );
    }

    #[test]
    fn only_a_branch_behind_its_upstream_prompts_for_force_up_front() {
        assert!(!needs_force_prompt(&target(
            true,
            BranchLocation::Both,
            Some(TrackState::Ahead(3))
        )));
        assert!(!needs_force_prompt(&target(
            true,
            BranchLocation::Both,
            Some(TrackState::InSync)
        )));
        assert!(needs_force_prompt(&target(
            true,
            BranchLocation::Both,
            Some(TrackState::Behind(1))
        )));
        assert!(needs_force_prompt(&target(
            true,
            BranchLocation::Both,
            Some(TrackState::Diverged {
                ahead: 1,
                behind: 1
            })
        )));
    }

    #[test]
    fn real_git_rejection_text_is_recognised() {
        let stderr = " ! [rejected]        main -> main (non-fast-forward)\n\
            error: failed to push some refs to 'origin'\n\
            hint: Updates were rejected because the tip of your current branch is behind";
        assert!(is_rejection(stderr));
        assert!(!is_stale_lease(stderr));
        assert_eq!(
            hint_for(stderr).as_deref(),
            Some("rejected as non-fast-forward — press p to pull first")
        );
    }

    #[test]
    fn a_fetch_first_rejection_is_recognised_too() {
        assert!(is_rejection(
            " ! [rejected]  main -> main (fetch first)\nerror: failed to push some refs"
        ));
    }

    #[test]
    fn a_stale_lease_is_distinguished_from_a_plain_rejection() {
        let stderr = " ! [rejected]  main -> main (stale info)\nerror: failed to push some refs";
        assert!(is_stale_lease(stderr));
        assert_eq!(
            hint_for(stderr).as_deref(),
            Some("remote ref is stale — press f to fetch, then retry")
        );
    }

    #[test]
    fn a_credential_failure_points_at_a_shell_rather_than_a_retry() {
        let stderr = "fatal: could not read Username for 'https://github.com': \
            terminal prompts disabled";
        assert!(is_credential_failure(stderr));
        assert_eq!(
            hint_for(stderr).as_deref(),
            Some("needs credentials — run it in a shell, or press gg for lazygit")
        );
    }

    #[test]
    fn an_unrecognised_failure_gets_no_invented_advice() {
        assert_eq!(hint_for("fatal: the remote end hung up unexpectedly"), None);
    }

    #[test]
    fn resolve_splits_the_upstream_into_remote_and_branch() {
        let details = git::LocalBranch {
            name: "roll/1-alpha".to_string(),
            upstream: "origin/roll/1-alpha".to_string(),
            remote_name: "origin".to_string(),
            track: "ahead 2".to_string(),
            worktree: String::new(),
        };
        let t = SyncTarget::resolve(
            "roll/1-alpha",
            "roll/1-alpha",
            BranchLocation::Both,
            Some(&details),
        );
        assert!(t.is_current);
        assert_eq!(t.remote, "origin");
        // The branch name contains a slash, so only the remote prefix may go.
        assert_eq!(t.upstream_branch.as_deref(), Some("roll/1-alpha"));
        assert_eq!(t.track, Some(TrackState::Ahead(2)));
    }

    #[test]
    fn resolve_falls_back_to_origin_when_there_is_no_upstream() {
        let details = git::LocalBranch {
            name: "roll/2-beta".to_string(),
            upstream: String::new(),
            remote_name: String::new(),
            track: String::new(),
            worktree: String::new(),
        };
        let t = SyncTarget::resolve("roll/2-beta", "main", BranchLocation::Local, Some(&details));
        assert!(!t.is_current);
        assert_eq!(t.remote, "origin");
        assert_eq!(t.upstream_branch, None);
        assert_eq!(t.track, Some(TrackState::NoUpstream));
    }

    #[test]
    fn a_remote_only_branch_resolves_without_local_details() {
        let t = SyncTarget::resolve("roll/3-gamma", "main", BranchLocation::Remote, None);
        assert_eq!(t.track, None);
        assert_eq!(t.remote, "origin");
        assert!(matches!(
            pull_plan(&t, PullMode::FfOnly),
            PullPlan::FetchRemote { .. }
        ));
    }

    #[test]
    fn sync_commands_disable_git_terminal_prompts() {
        // Without this a credential request blocks on a pipe nobody reads, and
        // the job never finishes.
        let cmd = sync_command(Path::new("/tmp"), &owned(&["push"]));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(
            std::ffi::OsStr::new("GIT_TERMINAL_PROMPT"),
            Some(std::ffi::OsStr::new("0"))
        )));
        assert!(envs.contains(&(
            std::ffi::OsStr::new("GIT_SEQUENCE_EDITOR"),
            Some(std::ffi::OsStr::new(":"))
        )));
    }

    // ── Against a real bare remote ──────────────────────────────────────────
    //
    // The pure tests above assert which arguments we build; these assert that
    // those arguments do what we think against a real git. Same fixture style as
    // the tests in `core::git`.

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed in {dir:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn capture(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare origin plus a clone with `main` tracking it.
    fn clone_with_origin() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().expect("origin dir");
        let work = tempfile::tempdir().expect("work dir");
        git_ok(origin.path(), &["init", "-b", "main", "--bare"]);
        git_ok(
            work.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(work.path(), &["config", "user.email", "t@e.test"]);
        git_ok(work.path(), &["config", "user.name", "t"]);
        git_ok(work.path(), &["commit", "--allow-empty", "-m", "init"]);
        git_ok(work.path(), &["push", "-u", "origin", "main"]);
        (origin, work)
    }

    fn target_from_repo(repo: &Path, branch: &str) -> SyncTarget {
        let details = git::local_branch_details(repo).expect("details");
        let current = git::current_branch(repo).expect("current branch");
        let found = details.iter().find(|b| b.name == branch);
        let location = match (
            git::ref_exists(repo, branch),
            git::ref_exists(repo, &format!("origin/{branch}")),
        ) {
            (true, true) => BranchLocation::Both,
            (true, false) => BranchLocation::Local,
            (false, true) => BranchLocation::Remote,
            (false, false) => BranchLocation::Neither,
        };
        SyncTarget::resolve(branch, &current, location, found)
    }

    #[test]
    fn a_plain_push_lands_the_commit_on_the_remote() {
        let (origin, work) = clone_with_origin();
        git_ok(
            work.path(),
            &["commit", "--allow-empty", "-m", "local work"],
        );

        let target = target_from_repo(work.path(), "main");
        assert_eq!(
            run_push(work.path(), &target, false).expect("push"),
            PushOutcome::Pushed
        );
        assert_eq!(
            capture(origin.path(), &["log", "-1", "--format=%s", "main"]),
            "local work"
        );
    }

    #[test]
    fn pushing_a_new_branch_sets_its_upstream() {
        let (_origin, work) = clone_with_origin();
        git_ok(work.path(), &["switch", "-c", "roll/1-alpha"]);
        git_ok(work.path(), &["commit", "--allow-empty", "-m", "roll work"]);

        let target = target_from_repo(work.path(), "roll/1-alpha");
        assert_eq!(target.upstream_branch, None, "no upstream to start with");
        assert_eq!(
            run_push(work.path(), &target, false).expect("push"),
            PushOutcome::Pushed
        );
        assert_eq!(
            capture(
                work.path(),
                &["rev-parse", "--abbrev-ref", "roll/1-alpha@{upstream}"]
            ),
            "origin/roll/1-alpha"
        );
    }

    #[test]
    fn a_diverged_push_is_reported_as_rejected_rather_than_as_an_error() {
        let (origin, work) = clone_with_origin();
        // Advance origin behind our back, then rewrite locally so the two lines
        // genuinely diverge.
        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["commit", "--allow-empty", "-m", "theirs"]);
        git_ok(other.path(), &["push", "origin", "main"]);

        git_ok(work.path(), &["commit", "--allow-empty", "-m", "ours"]);

        let target = target_from_repo(work.path(), "main");
        // Deliberately *not* fetched first: this is the case where we cannot know
        // in advance, so the plain attempt has to run and be classified.
        match run_push(work.path(), &target, false).expect("push should not error") {
            PushOutcome::Rejected { stderr } => {
                assert!(is_rejection(&stderr), "unexpected stderr: {stderr}")
            }
            PushOutcome::Pushed => panic!("a diverged push must not succeed"),
        }
        assert_eq!(
            capture(origin.path(), &["log", "-1", "--format=%s", "main"]),
            "theirs",
            "the remote must be untouched by a rejected push"
        );
    }

    #[test]
    fn a_force_with_lease_push_overwrites_the_remote_once_we_have_fetched() {
        let (origin, work) = clone_with_origin();
        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["commit", "--allow-empty", "-m", "theirs"]);
        git_ok(other.path(), &["push", "origin", "main"]);

        git_ok(work.path(), &["commit", "--allow-empty", "-m", "ours"]);
        // The lease is against our remote-tracking ref, so it only passes once we
        // have actually looked at the remote.
        run_fetch(work.path(), "origin").expect("fetch");

        let target = target_from_repo(work.path(), "main");
        assert!(
            needs_force_prompt(&target),
            "divergence should be visible after a fetch: {:?}",
            target.track
        );
        assert_eq!(
            run_push(work.path(), &target, true).expect("force push"),
            PushOutcome::Pushed
        );
        assert_eq!(
            capture(origin.path(), &["log", "-1", "--format=%s", "main"]),
            "ours"
        );
    }

    #[test]
    fn a_force_with_lease_push_is_refused_while_our_tracking_ref_is_stale() {
        let (origin, work) = clone_with_origin();
        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["commit", "--allow-empty", "-m", "theirs"]);
        git_ok(other.path(), &["push", "origin", "main"]);

        git_ok(work.path(), &["commit", "--allow-empty", "-m", "ours"]);

        // No fetch: the lease expects the old tip, so git refuses. This is the one
        // place we diverge from lazygit, which would fall back to a bare --force
        // and clobber "theirs" sight unseen.
        let target = target_from_repo(work.path(), "main");
        let err = run_push(work.path(), &target, true).expect_err("stale lease must fail");
        assert!(is_stale_lease(&err.failure.stderr), "{:?}", err.failure);
        assert_eq!(
            err.hint.as_deref(),
            Some("remote ref is stale — press f to fetch, then retry")
        );
        assert_eq!(
            capture(origin.path(), &["log", "-1", "--format=%s", "main"]),
            "theirs",
            "a refused lease must leave the remote alone"
        );
    }

    #[test]
    fn pulling_the_checked_out_branch_fast_forwards_the_worktree() {
        let (origin, work) = clone_with_origin();
        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["commit", "--allow-empty", "-m", "theirs"]);
        git_ok(other.path(), &["push", "origin", "main"]);
        let _ = origin;

        let target = target_from_repo(work.path(), "main");
        let PullPlan::Pull { args } = pull_plan(&target, PullMode::FfOnly) else {
            panic!("the checked-out branch should get a real pull");
        };
        run_pull(work.path(), &args).expect("pull");
        assert_eq!(
            capture(work.path(), &["log", "-1", "--format=%s", "main"]),
            "theirs"
        );
    }

    #[test]
    fn ff_only_refuses_to_merge_a_diverged_branch() {
        let (origin, work) = clone_with_origin();
        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["commit", "--allow-empty", "-m", "theirs"]);
        git_ok(other.path(), &["push", "origin", "main"]);
        let _ = origin;

        git_ok(work.path(), &["commit", "--allow-empty", "-m", "ours"]);

        let target = target_from_repo(work.path(), "main");
        let PullPlan::Pull { args } = pull_plan(&target, PullMode::FfOnly) else {
            panic!("expected a pull");
        };
        run_pull(work.path(), &args).expect_err("ff-only must refuse to merge");
        // The whole point of the default: no merge commit appears behind the
        // user's back.
        assert_eq!(
            capture(work.path(), &["log", "-1", "--format=%s", "main"]),
            "ours"
        );
    }

    #[test]
    fn a_branch_that_is_not_checked_out_fast_forwards_without_touching_the_worktree() {
        let (origin, work) = clone_with_origin();
        // Publish `feature`, then advance it on the remote from elsewhere.
        git_ok(work.path(), &["switch", "-c", "feature"]);
        git_ok(work.path(), &["commit", "--allow-empty", "-m", "feature a"]);
        git_ok(work.path(), &["push", "-u", "origin", "feature"]);
        git_ok(work.path(), &["switch", "main"]);

        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["switch", "feature"]);
        git_ok(
            other.path(),
            &["commit", "--allow-empty", "-m", "feature b"],
        );
        git_ok(other.path(), &["push", "origin", "feature"]);

        let target = target_from_repo(work.path(), "feature");
        assert!(!target.is_current);
        let PullPlan::FastForward { args } = pull_plan(&target, PullMode::FfOnly) else {
            panic!("a branch that is not checked out should fast-forward");
        };
        run_pull(work.path(), &args).expect("fast-forward");

        assert_eq!(
            capture(work.path(), &["log", "-1", "--format=%s", "feature"]),
            "feature b"
        );
        assert_eq!(
            git::current_branch(work.path()).unwrap(),
            "main",
            "the checked-out branch must not change"
        );
    }

    #[test]
    fn a_fetch_refspec_cannot_fast_forward_past_a_divergence() {
        let (origin, work) = clone_with_origin();
        git_ok(work.path(), &["switch", "-c", "feature"]);
        git_ok(work.path(), &["commit", "--allow-empty", "-m", "feature a"]);
        git_ok(work.path(), &["push", "-u", "origin", "feature"]);

        let other = tempfile::tempdir().expect("other dir");
        git_ok(
            other.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        );
        git_ok(other.path(), &["config", "user.email", "t@e.test"]);
        git_ok(other.path(), &["config", "user.name", "t"]);
        git_ok(other.path(), &["switch", "feature"]);
        git_ok(other.path(), &["reset", "--hard", "HEAD~1"]);
        git_ok(
            other.path(),
            &["commit", "--allow-empty", "-m", "rewritten"],
        );
        git_ok(other.path(), &["push", "--force", "origin", "feature"]);

        git_ok(work.path(), &["switch", "main"]);
        let before = capture(work.path(), &["rev-parse", "feature"]);

        let target = target_from_repo(work.path(), "feature");
        let PullPlan::FastForward { args } = pull_plan(&target, PullMode::FfOnly) else {
            panic!("expected a fast-forward plan");
        };
        // This is why the non-checked-out path is safe: git refuses a
        // non-fast-forward refspec update, so it can never discard local commits.
        run_pull(work.path(), &args).expect_err("a rewritten remote must not be fast-forwarded");
        assert_eq!(capture(work.path(), &["rev-parse", "feature"]), before);
    }

    #[test]
    fn fetching_prunes_a_branch_deleted_on_the_remote() {
        let (origin, work) = clone_with_origin();
        git_ok(work.path(), &["switch", "-c", "doomed"]);
        git_ok(
            work.path(),
            &["commit", "--allow-empty", "-m", "doomed work"],
        );
        git_ok(work.path(), &["push", "-u", "origin", "doomed"]);
        git_ok(work.path(), &["switch", "main"]);
        git_ok(origin.path(), &["branch", "-D", "doomed"]);

        assert!(git::ref_exists(work.path(), "origin/doomed"));
        run_fetch(work.path(), "origin").expect("fetch");
        assert!(
            !git::ref_exists(work.path(), "origin/doomed"),
            "the stale tracking ref must be pruned"
        );
        let target = target_from_repo(work.path(), "doomed");
        assert_eq!(target.track, Some(TrackState::Gone));
        // And pulling it is refused with advice rather than attempted.
        assert!(matches!(
            pull_plan(&target, PullMode::FfOnly),
            PullPlan::Refused { .. }
        ));
    }
}
