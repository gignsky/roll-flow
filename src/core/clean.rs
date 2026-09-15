//! `rf clean` — the repo-wide branch janitor.
//!
//! Unlike every other module under `core`, this one does not require a
//! `.roll-flow.toml`: it takes a repo root and an *optional* config, so it works
//! in any git repository. With a config it additionally knows about promoted
//! rolls; without one it falls back to conventions.
//!
//! Like `ops`, nothing here prints — `cli::clean` renders every user-facing
//! line from the returned [`CleanOutcome`].
//!
//! The problem it solves: when one host creates and pushes `roll/101` and a
//! *different* host promotes it, the branch is deleted on the remote. Back on
//! the originating host nothing notices — the local branch remains and the
//! `refs/remotes/origin/roll/101` cache still claims the branch is live, so
//! tools like lazygit keep offering it in both their local and remote views.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::core::{
    branches,
    config::Config,
    git::{self, LocalBranch},
};

// ── Inputs ───────────────────────────────────────────────────────────────────

/// How far `rf clean` is allowed to go.
pub(crate) struct CleanScope {
    /// Also delete the branches on their remote. Note this *extends* clean,
    /// where `rf prune --remote` *narrows* prune to the remote side only.
    pub with_remote: bool,
    /// Delete even when a branch holds commits the base branch lacks.
    pub force: bool,
    /// Refresh remote-tracking refs before planning.
    pub fetch: bool,
}

// ── Outputs ──────────────────────────────────────────────────────────────────

/// Why a branch is cleanup material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanReason {
    /// A roll the config reports as promoted to stable.
    Promoted,
    /// The upstream this branch tracked has been deleted.
    UpstreamGone,
    /// Fully contained in the base branch, with or without an upstream.
    Merged,
}

impl CleanReason {
    pub fn label(&self) -> &'static str {
        match self {
            CleanReason::Promoted => "promoted",
            CleanReason::UpstreamGone => "gone",
            CleanReason::Merged => "merged",
        }
    }
}

/// A branch clean intends to delete, and which copies of it.
pub(crate) struct CleanItem {
    pub branch: String,
    pub reason: CleanReason,
    pub delete_local: bool,
    pub delete_remote: bool,
    /// Which remote [`Self::delete_remote`] targets.
    pub remote: Option<String>,
    /// Commits that would be lost, populated only when `--force` overrode a
    /// failed containment check, so the renderer can state the cost up front.
    pub unmerged: Option<u32>,
}

/// A branch clean declined to touch, with the reason to show. Nothing that
/// would otherwise have been deleted is ever dropped silently.
pub(crate) struct CleanSkip {
    pub branch: String,
    pub reason: String,
}

/// What pruning one remote accomplished.
pub(crate) struct PrunedRemote {
    pub remote: String,
    /// Remote-tracking refs that the prune dropped, as a before/after diff
    /// rather than a parse of git's human-readable fetch output.
    pub removed: Vec<String>,
    /// Set when the fetch failed. Non-fatal: planning continues against the
    /// last successful fetch's data.
    pub error: Option<String>,
}

/// How the base branch was determined, so the renderer can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseSource {
    Config,
    RemoteHead,
    Conventional,
    CurrentBranch,
}

impl BaseSource {
    pub fn label(&self) -> &'static str {
        match self {
            BaseSource::Config => "from .roll-flow.toml",
            BaseSource::RemoteHead => "from remote HEAD",
            BaseSource::Conventional => "conventional name",
            BaseSource::CurrentBranch => "current branch",
        }
    }
}

/// The branch everything is measured against for containment.
pub(crate) struct CleanBase {
    /// Display name, e.g. `"main"`.
    pub name: String,
    /// Refs that count as "already in the base": the local branch and its
    /// remote-tracking counterpart, whichever resolve. Both are consulted
    /// because they routinely differ — a roll promoted upstream is contained in
    /// `origin/main` while a stale local `main` still lacks it.
    pub refs: Vec<String>,
    pub source: BaseSource,
}

/// What a clean would do, computed before anything is deleted so the plan can
/// be shown, confirmed, then applied unchanged.
pub(crate) struct CleanOutcome {
    pub pruned: Vec<PrunedRemote>,
    pub items: Vec<CleanItem>,
    pub skipped: Vec<CleanSkip>,
    pub base: Option<CleanBase>,
    /// Whether a `.roll-flow.toml` was found — distinct from "no promoted
    /// rolls", and the reason promoted-roll cleanup was or was not attempted.
    pub had_config: bool,
    /// Whether the repo has no remotes at all, distinct from "nothing pruned".
    pub no_remotes: bool,
    pub shallow: bool,
}

impl CleanOutcome {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Stale refs dropped across every remote.
    pub fn pruned_count(&self) -> usize {
        self.pruned.iter().map(|p| p.removed.len()).sum()
    }
}

/// Per-branch outcome of applying a plan. A branch can partially succeed (local
/// gone, remote push failed), so both flags and the errors travel together.
pub(crate) struct CleanResult {
    pub branch: String,
    pub local_deleted: bool,
    pub remote_deleted: bool,
    pub errors: Vec<String>,
}

// ── Pure classification ──────────────────────────────────────────────────────

/// Why clean must not touch a branch, regardless of how cleanable it looks.
///
/// Checked *after* categorization so that protected and checked-out branches
/// are only mentioned when they would otherwise have been deleted — a repo's
/// permanent branches should not be listed as skips on every run.
pub(crate) fn guard(branch: &LocalBranch, current: &str, protected: &[String]) -> Option<String> {
    if branch.name == current {
        // Checked ahead of the worktree case: the current branch is always in
        // *some* worktree, and naming this repo's own path back at the user
        // reads as a puzzle rather than an explanation.
        return Some("checked out — switch away to clean it".to_string());
    }
    if branch.is_checked_out() {
        // A different worktree. Here the path is the actionable part, and git
        // refuses the delete regardless.
        return Some(format!(
            "checked out in worktree at {}",
            branch.worktree.trim()
        ));
    }
    if protected.iter().any(|p| p == &branch.name) {
        return Some("protected branch".to_string());
    }
    None
}

/// Which category a branch falls into, given facts the caller computed.
///
/// Precedence is most-informative-first: a promoted roll whose upstream is also
/// gone is reported once, as promoted.
pub(crate) fn categorize(
    branch: &LocalBranch,
    is_promoted: bool,
    merged: bool,
) -> Option<CleanReason> {
    if is_promoted {
        return Some(CleanReason::Promoted);
    }
    if branch.upstream_gone() {
        return Some(CleanReason::UpstreamGone);
    }
    if merged {
        return Some(CleanReason::Merged);
    }
    None
}

// ── Base resolution ──────────────────────────────────────────────────────────

/// Containment refs for `name`: the local branch and `<remote>/<name>`, for
/// whichever of them resolve.
fn containment_refs(repo: &Path, name: &str, remotes: &[String]) -> Vec<String> {
    let mut refs = Vec::new();
    if git::ref_exists(repo, name) {
        refs.push(name.to_string());
    }
    for remote in remotes {
        let tracking = format!("{remote}/{name}");
        if git::ref_exists(repo, &tracking) {
            refs.push(tracking);
        }
    }
    refs
}

/// Determine what counts as "already merged", first hit wins.
///
/// Returns `None` only when the repo offers nothing usable, in which case
/// merged-branch detection is skipped rather than guessed at.
pub(crate) fn resolve_base(
    repo: &Path,
    config: Option<&Config>,
    remotes: &[String],
) -> Option<CleanBase> {
    // 1. The configured stable branch.
    if let Some(config) = config {
        let refs = containment_refs(repo, &config.stable_branch, remotes);
        if !refs.is_empty() {
            return Some(CleanBase {
                name: config.stable_branch.clone(),
                refs,
                source: BaseSource::Config,
            });
        }
    }

    // 2. The remote's own default branch, preferring `origin`. Correct for a
    //    fresh clone that has no local base branch at all.
    let ordered = remotes
        .iter()
        .filter(|r| r.as_str() == "origin")
        .chain(remotes.iter().filter(|r| r.as_str() != "origin"));
    for remote in ordered {
        if let Some(head) = git::remote_head_branch(repo, remote) {
            let refs = containment_refs(repo, &head, remotes);
            if !refs.is_empty() {
                return Some(CleanBase {
                    name: head,
                    refs,
                    source: BaseSource::RemoteHead,
                });
            }
        }
    }

    // 3. Conventional names.
    for name in ["main", "master", "develop", "trunk"] {
        let refs = containment_refs(repo, name, remotes);
        if !refs.is_empty() {
            return Some(CleanBase {
                name: name.to_string(),
                refs,
                source: BaseSource::Conventional,
            });
        }
    }

    // 4. Whatever is checked out, if anything.
    let current = git::current_branch(repo).unwrap_or_default();
    if !current.is_empty() {
        return Some(CleanBase {
            name: current.clone(),
            refs: vec![current],
            source: BaseSource::CurrentBranch,
        });
    }

    None
}

/// Branches clean must never delete.
fn protected_branches(repo: &Path, config: Option<&Config>, remotes: &[String]) -> Vec<String> {
    let mut protected = BTreeSet::new();
    match config {
        Some(config) => {
            protected.insert(config.stable_branch.clone());
            protected.insert(config.rolling_branch.clone());
            protected.extend(config.clean_protect.iter().cloned());
        }
        None => {
            // No config to name them, so fall back to the conventional set.
            protected.extend(["main", "master", "develop"].map(ToString::to_string));
        }
    }
    // A remote's default branch is worth protecting either way: it is the one
    // branch the repo is guaranteed to still need.
    for remote in remotes {
        if let Some(head) = git::remote_head_branch(repo, remote) {
            protected.insert(head);
        }
    }
    protected.into_iter().collect()
}

/// True if every commit reachable from `tip` is already in one of `refs`.
///
/// The same containment rule `rf prune` applies (see `ops::contained_in_stable`),
/// generalized from the stable branch to whatever base clean resolved.
fn contained_in(repo: &Path, tip: &str, refs: &[String]) -> bool {
    refs.iter()
        .any(|base| git::is_ancestor(repo, tip, base).unwrap_or(false))
}

/// Roll branches the config reports as already promoted to stable.
fn promoted_rolls(config: Option<&Config>) -> Result<BTreeSet<String>> {
    let Some(config) = config else {
        return Ok(BTreeSet::new());
    };
    Ok(branches::list_rolls(config)?
        .into_iter()
        .filter(|r| r.state == branches::RollState::Promoted)
        .map(|r| r.branch)
        .collect())
}

// ── Planning ─────────────────────────────────────────────────────────────────

/// Decide what `rf clean` would delete, without deleting anything.
///
/// Pruning every remote comes first and is not optional to the ordering:
/// `%(upstream:track)` reports `gone` from the *absence* of a remote-tracking
/// ref, which a stale cache still supplies. Detect before pruning and the very
/// branch this command exists for reports as in sync.
pub(crate) fn plan(
    repo: &Path,
    config: Option<&Config>,
    scope: &CleanScope,
) -> Result<CleanOutcome> {
    let remotes = git::remotes(repo).unwrap_or_default();
    let pruned = if scope.fetch {
        prune_remotes(repo, &remotes)
    } else {
        Vec::new()
    };

    let details = git::local_branch_details(repo)?;
    let current = git::current_branch(repo).unwrap_or_default();
    let base = resolve_base(repo, config, &remotes);
    let protected = protected_branches(repo, config, &remotes);
    let promoted = promoted_rolls(config)?;

    let mut items = Vec::new();
    let mut skipped = Vec::new();

    for branch in &details {
        let is_promoted = promoted.contains(&branch.name);
        // Only pay for the ancestry walk when the cheaper categories missed.
        let merged = !is_promoted
            && !branch.upstream_gone()
            && base
                .as_ref()
                .is_some_and(|base| contained_in(repo, &branch.name, &base.refs));

        // The base is the yardstick, not cleanup material: it is contained in
        // itself by definition, and reporting that every run is pure noise.
        if base.as_ref().is_some_and(|base| base.name == branch.name) {
            continue;
        }

        let Some(reason) = categorize(branch, is_promoted, merged) else {
            continue;
        };

        if let Some(skip) = guard(branch, &current, &protected) {
            skipped.push(CleanSkip {
                branch: branch.name.clone(),
                reason: skip,
            });
            continue;
        }

        // Containment gate, applied to all three categories alike. A deleted
        // upstream is not proof the local tip is contained anywhere, and this
        // is the only thing standing between the roll/101 case and silent data
        // loss.
        let Some(base) = base.as_ref() else {
            if !scope.force {
                skipped.push(CleanSkip {
                    branch: branch.name.clone(),
                    reason: "no base branch to verify against — rerun with --force".to_string(),
                });
                continue;
            }
            items.push(CleanItem {
                branch: branch.name.clone(),
                reason,
                delete_local: true,
                delete_remote: false,
                remote: None,
                unmerged: None,
            });
            continue;
        };

        let contained = merged || contained_in(repo, &branch.name, &base.refs);
        if !contained && !scope.force {
            skipped.push(CleanSkip {
                branch: branch.name.clone(),
                reason: format!("not fully merged into '{}' — rerun with --force", base.name),
            });
            continue;
        }
        let unmerged = if contained {
            None
        } else {
            git::commits_not_in(repo, &branch.name, &base.refs).ok()
        };

        let (delete_remote, remote) = remote_target(repo, branch, &remotes, base, scope);

        items.push(CleanItem {
            branch: branch.name.clone(),
            reason,
            delete_local: true,
            delete_remote,
            remote,
            unmerged,
        });
    }

    Ok(CleanOutcome {
        pruned,
        items,
        skipped,
        base,
        had_config: config.is_some(),
        no_remotes: remotes.is_empty(),
        shallow: git::is_shallow(repo),
    })
}

/// Fetch-with-prune each remote, recording exactly which stale refs went.
///
/// A failing remote is recorded and skipped rather than aborting: `rf clean`
/// exists for the laptop that has been offline, and the containment gate still
/// guards every deletion, so the worst case is stale-but-real state.
fn prune_remotes(repo: &Path, remotes: &[String]) -> Vec<PrunedRemote> {
    remotes
        .iter()
        .map(|remote| {
            let before = git::remote_tracking_refs(repo, remote).unwrap_or_default();
            match git::fetch_prune_quiet(repo, remote) {
                Ok(()) => {
                    let after: HashSet<String> = git::remote_tracking_refs(repo, remote)
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    let removed = before.into_iter().filter(|r| !after.contains(r)).collect();
                    PrunedRemote {
                        remote: remote.clone(),
                        removed,
                        error: None,
                    }
                }
                Err(err) => PrunedRemote {
                    remote: remote.clone(),
                    removed: Vec::new(),
                    error: Some(err.to_string()),
                },
            }
        })
        .collect()
}

/// Whether and where to delete this branch's remote copy under `--with-remote`.
///
/// An upstream-gone branch has no remote copy by definition, so only merged and
/// promoted branches reach the remote at all. The remote tip is checked for
/// containment separately from the local one — they can differ.
fn remote_target(
    repo: &Path,
    branch: &LocalBranch,
    remotes: &[String],
    base: &CleanBase,
    scope: &CleanScope,
) -> (bool, Option<String>) {
    if !scope.with_remote || branch.upstream_gone() {
        return (false, None);
    }

    // Prefer the branch's own upstream remote: a fork has more than one, and
    // assuming `origin` would push the delete to the wrong place.
    let remote = if !branch.remote_name.is_empty() {
        branch.remote_name.clone()
    } else {
        match remotes
            .iter()
            .find(|r| git::ref_exists(repo, &format!("{r}/{}", branch.name)))
        {
            Some(remote) => remote.clone(),
            None => return (false, None),
        }
    };

    let tracking = format!("{remote}/{}", branch.name);
    if !git::ref_exists(repo, &tracking) {
        return (false, None);
    }
    if !scope.force && !contained_in(repo, &tracking, &base.refs) {
        return (false, None);
    }
    (true, Some(remote))
}

// ── Applying ─────────────────────────────────────────────────────────────────

/// Execute a plan. Never prompts and never decides — the caller confirmed.
///
/// Remote deletions go out as one batched push per remote (each push is a
/// network round-trip); local deletions run per branch so one failure does not
/// abort the rest.
pub(crate) fn apply(repo: &Path, outcome: &CleanOutcome) -> Result<Vec<CleanResult>> {
    let mut remote_errors = std::collections::BTreeMap::new();
    for remote in remote_batches(outcome) {
        let (name, branches) = remote;
        if let Err(err) = git::delete_remote_branches(repo, &name, &branches) {
            remote_errors.insert(name, err.to_string());
        }
    }

    Ok(outcome
        .items
        .iter()
        .map(|item| {
            let mut errors = Vec::new();
            let mut local_deleted = false;
            let mut remote_deleted = false;

            if item.delete_remote {
                match item.remote.as_ref().and_then(|r| remote_errors.get(r)) {
                    Some(err) => errors.push(err.clone()),
                    None => remote_deleted = true,
                }
            }

            if item.delete_local {
                match git::delete_local_branch(repo, &item.branch) {
                    Ok(()) => local_deleted = true,
                    Err(err) => errors.push(err.to_string()),
                }
            }

            CleanResult {
                branch: item.branch.clone(),
                local_deleted,
                remote_deleted,
                errors,
            }
        })
        .collect())
}

/// Branches to delete, grouped by the remote they live on.
fn remote_batches(outcome: &CleanOutcome) -> Vec<(String, Vec<String>)> {
    let mut batches: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for item in &outcome.items {
        if let (true, Some(remote)) = (item.delete_remote, item.remote.as_ref()) {
            batches
                .entry(remote.clone())
                .or_default()
                .push(item.branch.clone());
        }
    }
    batches.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{categorize, guard, CleanReason};
    use crate::core::git::LocalBranch;

    fn branch(name: &str) -> LocalBranch {
        LocalBranch {
            name: name.to_string(),
            upstream: String::new(),
            remote_name: String::new(),
            track: String::new(),
            worktree: String::new(),
        }
    }

    fn tracked(name: &str, track: &str) -> LocalBranch {
        LocalBranch {
            name: name.to_string(),
            upstream: format!("origin/{name}"),
            remote_name: "origin".to_string(),
            track: track.to_string(),
            worktree: String::new(),
        }
    }

    // ── guard ────────────────────────────────────────────────────────────────

    #[test]
    fn guards_nothing_by_default() {
        assert!(guard(&branch("feature/x"), "main", &[]).is_none());
    }

    #[test]
    fn guards_a_branch_checked_out_in_a_worktree() {
        let mut b = branch("feature/x");
        b.worktree = "/tmp/wt".to_string();
        let reason = guard(&b, "main", &[]).expect("guarded");
        assert!(reason.contains("/tmp/wt"), "should name the path: {reason}");
    }

    #[test]
    fn guards_the_current_branch() {
        assert!(guard(&branch("feature/x"), "feature/x", &[]).is_some());
    }

    #[test]
    fn guards_protected_branches() {
        let protected = vec!["develop".to_string()];
        assert_eq!(
            guard(&branch("develop"), "main", &protected).as_deref(),
            Some("protected branch")
        );
        assert!(guard(&branch("feature/x"), "main", &protected).is_none());
    }

    #[test]
    fn current_branch_guard_outranks_the_worktree_guard() {
        // The current branch is always in a worktree; the plain message is the
        // useful one.
        let mut b = branch("feature/x");
        b.worktree = "/tmp/wt".to_string();
        let reason = guard(&b, "feature/x", &[]).expect("guarded");
        assert!(reason.contains("switch away"), "got: {reason}");
    }

    #[test]
    fn worktree_guard_outranks_the_protected_guard() {
        // Both apply to the same branch; the worktree path is the actionable
        // half, so it must be the reason shown.
        let mut b = branch("develop");
        b.worktree = "/tmp/wt".to_string();
        let reason = guard(&b, "main", &["develop".to_string()]).expect("guarded");
        assert!(reason.contains("/tmp/wt"), "got: {reason}");
    }

    // ── categorize ───────────────────────────────────────────────────────────

    #[test]
    fn categorizes_nothing_when_no_signal_applies() {
        assert_eq!(categorize(&tracked("feature/x", ""), false, false), None);
        // Ahead of its upstream and not merged: active work, not cleanup.
        assert_eq!(
            categorize(&tracked("feature/x", "ahead 2"), false, false),
            None
        );
    }

    #[test]
    fn categorizes_a_gone_upstream() {
        assert_eq!(
            categorize(&tracked("roll/101", "gone"), false, false),
            Some(CleanReason::UpstreamGone)
        );
    }

    #[test]
    fn categorizes_a_merged_branch() {
        assert_eq!(
            categorize(&branch("feature/x"), false, true),
            Some(CleanReason::Merged)
        );
    }

    #[test]
    fn promoted_outranks_gone_and_merged() {
        // A promoted roll whose branch was also deleted upstream is reported
        // once, under the most informative label.
        assert_eq!(
            categorize(&tracked("roll/101", "gone"), true, true),
            Some(CleanReason::Promoted)
        );
    }

    #[test]
    fn gone_outranks_merged() {
        assert_eq!(
            categorize(&tracked("roll/101", "gone"), false, true),
            Some(CleanReason::UpstreamGone)
        );
    }

    #[test]
    fn a_never_pushed_branch_is_not_gone() {
        // No upstream at all must not read as a deleted upstream: that is the
        // difference between unpushed work and cleanup material.
        assert_eq!(categorize(&branch("feature/x"), false, false), None);
    }

    #[test]
    fn reason_labels_are_stable() {
        assert_eq!(CleanReason::Promoted.label(), "promoted");
        assert_eq!(CleanReason::UpstreamGone.label(), "gone");
        assert_eq!(CleanReason::Merged.label(), "merged");
    }
}
