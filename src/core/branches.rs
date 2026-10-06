use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::core::{config::Config, git};
use crate::error::RfError;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchLocation {
    Local,
    Remote,
    Both,
    Neither,
}

impl BranchLocation {
    pub fn symbol(&self) -> &'static str {
        match self {
            BranchLocation::Local => "L",
            BranchLocation::Remote => "R",
            BranchLocation::Both => "B",
            BranchLocation::Neither => "-",
        }
    }

    /// Full word form used in the detail overlay: `"local"` / `"remote"` /
    /// `"both"` / `"none"`. The compact table keeps [`symbol`](Self::symbol).
    pub fn label(&self) -> &'static str {
        match self {
            BranchLocation::Local => "local",
            BranchLocation::Remote => "remote",
            BranchLocation::Both => "both",
            BranchLocation::Neither => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollState {
    Active,    // not yet merged to rolling
    Graduated, // merged to rolling, no new commits since
    Diverged,  // merged to rolling, but has new commits since (needs re-graduation)
    Reverted,  // merged to rolling, but that merge was later reverted there (needs re-graduation)
    Promoted,  // merged to main
    Demoted,   // merged to main, but that merge was later reverted there (needs re-promotion)
    Blocked,   // active but has ungraduated dependencies
}

impl RollState {
    pub fn label(&self) -> &'static str {
        match self {
            RollState::Active => "active",
            RollState::Graduated => "✓ graduated",
            RollState::Diverged => "⚠ diverged",
            RollState::Reverted => "↩ reverted",
            RollState::Promoted => "✓ promoted",
            RollState::Demoted => "↩ demoted",
            RollState::Blocked => "⛔ blocked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RollInfo {
    pub branch: String,
    pub number: u32,
    pub state: RollState,
    pub location: BranchLocation,
    pub is_current: bool,
    /// Roll numbers this roll integrated — the rolls it depends on. Populated
    /// for every state, not just `Active`: a graduated roll's dependencies are
    /// what orders its promotion, and a promoted roll's are still worth showing.
    pub deps: Vec<u32>,
    /// The inverse of [`deps`](Self::deps): roll numbers that integrated *this*
    /// roll. Precomputed in [`list_rolls`] from a single reverse index so
    /// renderers never rescan the whole list per row.
    pub dependents: Vec<u32>,
    /// The subset of [`deps`](Self::deps) whose current branch tip is **not**
    /// an ancestor of this roll — i.e. this roll integrated them at some point,
    /// but they have since moved on and this roll's copy is stale. This is a
    /// direct `git merge-base --is-ancestor` check, so unlike [`state`]'s
    /// `Diverged` it fires regardless of the dependency's own state: an
    /// `Active` dependency that keeps gaining commits after being integrated is
    /// just as stale as a `Diverged` one that graduated and then moved. That
    /// matters before merging a batch of dependents — each one that integrated
    /// an older copy of a still-moving dependency needs to say so, not just the
    /// ones whose dependency happens to have graduated.
    pub stale_deps: Vec<u32>,
    /// Hash of this roll's graduation merge on the rolling branch, or `None`
    /// when it has not graduated. This is the commit `rf promote --roll` merges
    /// into stable: advancing stable to it promotes exactly this roll (and
    /// whatever graduated before it), which keeps stable a prefix of rolling.
    pub graduation_commit: Option<String>,
}

/// Prefix of the hotfix tier, which carries its own numbering independent of
/// rolls. Lives here rather than in `ops` because listing hotfixes is a
/// branch-level concern the tables need, not only the landing op.
pub const HOTFIX_PREFIX: &str = "hotfix/";

/// Where a hotfix is in its short life: branched off stable, or landed on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotfixState {
    /// Exists, not yet merged into the stable branch.
    Open,
    /// Its landing merge is on the stable branch.
    Landed,
}

impl HotfixState {
    pub fn label(&self) -> &'static str {
        match self {
            HotfixState::Open => "hotfix",
            HotfixState::Landed => "✓ landed",
        }
    }
}

/// A `hotfix/N-MMDD-slug` branch as the tables show it.
///
/// Its own type rather than a [`RollInfo`] with a kind flag: a hotfix has no
/// dependencies, no graduation commit and no place in the roll numbering (a
/// `hotfix/1` and a `roll/1` coexist), so sharing `RollInfo` would either carry
/// three meaningless fields or force every dependency scan in [`list_rolls`] to
/// filter by kind. Keeping the roll list purely rolls is what keeps those scans
/// simple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotfixInfo {
    pub branch: String,
    pub number: u32,
    pub state: HotfixState,
    pub location: BranchLocation,
    pub is_current: bool,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Render a roll-number list for the `deps` / `dependants` table columns:
/// `[2, 3]` → `"2,3"`, empty → `""`. Shared so the TUI table, `rf status
/// --no-tui` and `rf list --no-tui --deps` cannot drift apart.
pub fn format_roll_numbers(nums: &[u32]) -> String {
    nums.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Render the `deps` column like [`format_roll_numbers`], but suffix a number
/// with `⚠` when it is also in `stale` — i.e. `RollInfo::stale_deps` — so a
/// dependency that has moved since this roll integrated it is visible from the
/// plain table, not only the detail view. This is what matters before
/// reintegrating or merging a batch of dependents against a still-moving
/// dependency: `2,3⚠` says dep 3 has new commits to pick up, dep 2 does not.
/// Never applied to `dependants`: staleness is a property of what this roll
/// integrated, not of who integrated this roll.
pub fn format_deps_with_staleness(nums: &[u32], stale: &[u32]) -> String {
    nums.iter()
        .map(|n| {
            if stale.contains(n) {
                format!("{n}⚠")
            } else {
                n.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Extract the roll number from a branch name given the configured prefix.
/// `"roll/5-theme"` with prefix `"roll/"` → `Some(5)`.
pub fn parse_roll_number(branch: &str, prefix: &str) -> Option<u32> {
    branch.strip_prefix(prefix)?.split('-').next()?.parse().ok()
}

/// Return the current branch name if it is a roll branch, else `None`.
pub fn get_current_roll(config: &Config) -> Result<Option<String>, RfError> {
    let branch = git::current_branch(&config.repo_root)?;
    if branch.starts_with(&config.roll_prefix) {
        Ok(Some(branch))
    } else {
        Ok(None)
    }
}

/// Collect every roll branch (local + remote, deduplicated) and compute their
/// state.  Results are sorted ascending by roll number.
pub fn list_rolls(config: &Config) -> Result<Vec<RollInfo>, RfError> {
    let repo = &config.repo_root;

    let current = git::current_branch(repo).unwrap_or_default();
    let pattern = format!("{}*", config.roll_prefix);

    let mut local = git::local_branches(repo, &pattern)?;
    let remote = git::remote_branches(repo, &pattern)?;
    local.extend(remote);
    local.sort();
    local.dedup();

    // Single-pass log scans — much faster than per-roll log calls.
    let graduated = scan_graduated(repo, &config.rolling_branch);
    let promoted_set = scan_promoted(repo, &config.stable_branch);

    let mut rolls = Vec::new();
    for branch in local {
        let Some(number) = parse_roll_number(&branch, &config.roll_prefix) else {
            continue;
        };

        let loc_local = git::ref_exists(repo, &branch);
        let loc_remote = git::ref_exists(repo, &format!("origin/{branch}"));
        let location = match (loc_local, loc_remote) {
            (true, true) => BranchLocation::Both,
            (true, false) => BranchLocation::Local,
            (false, true) => BranchLocation::Remote,
            _ => BranchLocation::Neither,
        };

        let is_promoted = promoted_set.contains(&branch);
        let graduation_commit = graduated.get(&branch).cloned();
        let is_graduated = graduation_commit.is_some();

        let state = if is_promoted {
            // Only revert-check promoted rolls; a roll that never promoted has
            // nothing on stable to have been reverted.
            if check_promotion_reverted(repo, &branch, &config.stable_branch) {
                RollState::Demoted
            } else {
                RollState::Promoted
            }
        } else if is_graduated {
            // Only revert/divergence-check graduated (not-yet-promoted) rolls.
            // Reverted takes priority over Diverged: a roll can gain new
            // commits on its own branch at any point regardless of whether its
            // graduation merge is still standing, and "the graduation was
            // undone" is the more urgent fact to surface.
            if check_reverted(repo, &branch, &config.rolling_branch) {
                RollState::Reverted
            } else if check_diverged(repo, &branch, &config.rolling_branch) {
                RollState::Diverged
            } else {
                RollState::Graduated
            }
        } else {
            RollState::Active
        };

        rolls.push(RollInfo {
            is_current: branch == current,
            branch,
            number,
            state,
            location,
            deps: Vec::new(),
            dependents: Vec::new(),
            stale_deps: Vec::new(),
            graduation_commit,
        });
    }

    rolls.sort_by_key(|r| r.number);

    // Compute deps for *every* roll, whatever its state. Deps are the roll's
    // actual direct integrations — the roll branches it pulled in via
    // `rf integrate` — detected from its own first-parent merge history. File
    // overlap and broad ancestry are deliberately NOT used here: in a dotfiles
    // repo nearly every roll touches flake.lock, which made them spuriously
    // block one another. The one exception is a merge of the rolling branch
    // itself (`[I]` / `rf integrate <rolling>`): see `integration_deps`.
    //
    // Blocking, by contrast, only applies to Active rolls: an ungraduated
    // integration holds a roll back from graduating, but once the roll itself
    // has graduated the relationship is history, not a blocker.
    let snapshot = rolls.clone();
    for roll in &mut rolls {
        roll.deps = integration_deps(
            repo,
            &roll.branch,
            roll.number,
            &config.roll_prefix,
            &deps_base_ref(roll, &config.stable_branch),
            &config.rolling_branch,
            &graduated,
        );
        // A dependency this roll integrated may have kept moving since — an
        // ancestry check, not a state lookup, so it fires however the
        // dependency's own state reads (Active, Diverged, whatever).
        roll.stale_deps = roll
            .deps
            .iter()
            .copied()
            .filter(|dep_num| {
                snapshot
                    .iter()
                    .find(|r| r.number == *dep_num)
                    .is_some_and(|dep| dep_tip_missing(repo, &dep.branch, &roll.branch))
            })
            .collect();
        if roll.state == RollState::Active {
            let blocked = roll.deps.iter().any(|dep| {
                snapshot
                    .iter()
                    .find(|r| r.number == *dep)
                    .map(|r| matches!(r.state, RollState::Active | RollState::Blocked))
                    .unwrap_or(false)
            });
            if blocked {
                roll.state = RollState::Blocked;
            }
        }
    }

    // Reverse index, built once: dependency number -> rolls that integrated it.
    // Doing this here keeps renderers from rescanning every roll per row.
    let mut reverse: HashMap<u32, Vec<u32>> = HashMap::new();
    for roll in &rolls {
        for dep in &roll.deps {
            reverse.entry(*dep).or_default().push(roll.number);
        }
    }
    for roll in &mut rolls {
        if let Some(mut dependents) = reverse.remove(&roll.number) {
            dependents.sort_unstable();
            dependents.dedup();
            roll.dependents = dependents;
        }
    }

    Ok(rolls)
}

/// The exclusive lower bound for a roll's [`integration_deps`] scan.
///
/// For an ungraduated roll, the stable branch is the right floor: everything
/// above it is the roll's own work. For a graduated one it is not — once the
/// roll has been promoted its tip is contained in stable, `<stable>..<roll>` is
/// empty, and its dependencies would silently vanish. The first parent of the
/// graduation merge is the rolling branch immediately before the roll landed,
/// which bounds the scan to exactly what the roll brought in and keeps working
/// after promotion.
fn deps_base_ref(roll: &RollInfo, stable_ref: &str) -> String {
    match &roll.graduation_commit {
        Some(sha) => format!("{sha}^1"),
        None => stable_ref.to_string(),
    }
}

// ── Per-roll checks (exposed for use in graduate/promote commands) ─────────────

/// True if the roll has a graduation (merge) commit on the rolling branch.
/// Every merge-subject shape [`extract_graduated_branch`] knows is checked,
/// including the hand-written ones a conflicted merge leaves behind.
pub fn check_graduated(repo: &Path, roll_branch: &str, rolling_ref: &str) -> bool {
    let rolling = match git::resolve_branch(repo, rolling_ref) {
        Some(r) => r,
        None => return false,
    };
    let subjects = git::log_subjects(repo, &[&rolling]).unwrap_or_default();
    subjects_contain_graduation(&subjects, roll_branch)
}

/// True if the roll was graduated but has new commits after the merge point.
pub fn check_diverged(repo: &Path, roll_branch: &str, rolling_ref: &str) -> bool {
    let rolling = match git::resolve_branch(repo, rolling_ref) {
        Some(r) => r,
        None => return false,
    };

    let Some(merge_hash) = find_graduation_commit(repo, roll_branch, &rolling) else {
        return false;
    };

    // ^2 parent is the roll's HEAD at graduation time (--no-ff merge).
    let roll_tip_at_merge = match git::capture_git(repo, &["rev-parse", &format!("{merge_hash}^2")])
    {
        Ok(h) => h,
        Err(_) => return false,
    };

    let roll_ref = match git::resolve_branch(repo, roll_branch) {
        Some(r) => r,
        None => return false,
    };

    // Any commits on roll after that merge tip?
    let range = format!("{roll_tip_at_merge}..{roll_ref}");
    git::capture_git(repo, &["rev-list", "--count", &range])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// True if the roll's graduation onto `rolling_ref` was later undone there by
/// a `git revert`. See [`find_reverted_graduation`] for the commit that would
/// need reverting *again* to restore it — the actual re-graduation, since the
/// roll branch's own tip remains an ancestor of rolling either way (a revert
/// adds a commit on top; it does not remove anything from history), so an
/// ordinary `--no-ff` re-merge of the roll has nothing new to bring in and
/// cannot undo the revert.
pub fn check_reverted(repo: &Path, roll_branch: &str, rolling_ref: &str) -> bool {
    find_reverted_graduation(repo, roll_branch, rolling_ref).is_some()
}

/// If `roll_branch`'s graduation onto `rolling_ref` is currently reverted
/// there, the hash of the commit to revert *now* to restore it. `None` when
/// the roll never graduated, or its graduation is currently in effect
/// (never reverted, or reverted an even number of times — see
/// [`find_active_revert_in_range`]).
pub fn find_reverted_graduation(
    repo: &Path,
    roll_branch: &str,
    rolling_ref: &str,
) -> Option<String> {
    let rolling = git::resolve_branch(repo, rolling_ref)?;
    let merge_hash = find_graduation_commit(repo, roll_branch, &rolling)?;
    find_active_revert_in_range(repo, &merge_hash, &rolling)
}

/// True if `roll_branch`'s single-roll promotion (`Promote <roll> to
/// <stable>`, the `rf promote --roll` shape) onto `stable_ref` was later
/// undone there by a `git revert`.
///
/// Deliberately narrower than [`check_reverted`]: it only recognizes the
/// single-roll promotion shape ([`scan_promotion_commits`]), not the
/// multi-roll `Promote <rolling> to <stable>` shape that can carry several
/// rolls in one merge — a revert of a bundled promotion is not attributed to
/// any one roll here. Detect-only: nothing currently automates the fix the
/// way [`find_reverted_graduation`] does for a reverted graduation, since
/// that would mean teaching the per-roll promotion pipeline (version gate,
/// release tags, carried-rolls disclosure) a second kind of step.
pub fn check_promotion_reverted(repo: &Path, roll_branch: &str, stable_ref: &str) -> bool {
    let Some(stable) = git::resolve_branch(repo, stable_ref) else {
        return false;
    };
    let Some(hash) = scan_promotion_commits(repo, &stable).remove(roll_branch) else {
        return false;
    };
    find_active_revert_in_range(repo, &hash, &stable).is_some()
}

/// True if the roll has been promoted to the stable branch.
///
/// All three attribution sources must agree with [`scan_promoted`], which is why
/// this defers to it rather than reimplementing two of them: a `Promote <roll> …`
/// subject, a `Rolls:` body naming the roll, or the roll's graduation merge being
/// reachable from stable. The body source used to be missing here, so a roll
/// attributed only by a `Rolls:` list read as promoted in `rf list` but not to
/// `rf graduate`'s already-promoted guard.
pub fn check_promoted(repo: &Path, roll_branch: &str, stable_ref: &str) -> bool {
    scan_promoted(repo, stable_ref).contains(roll_branch)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Scan the rolling log, mapping each graduated branch name to the hash of its
/// graduation merge. Much cheaper than one `git log` call per roll, and the hash
/// is needed anyway — by `deps_base_ref` to bound the dependency scan, and by
/// `rf promote --roll` as the commit to advance stable to.
///
/// Two passes, because "a merge naming this branch is reachable from rolling"
/// and "this is where the branch landed on rolling" are different questions:
///
/// 1. `--first-parent` finds the merge on rolling's own mainline — the actual
///    graduation.
/// 2. A full reachability pass then fills in branches with no mainline merge of
///    their own, which is how a roll that only reached rolling inside another
///    roll's history still counts as graduated. Membership therefore matches the
///    long-standing behaviour exactly; only the hash is sharpened.
///
/// Without pass 1 the hash can land on an unrelated merge that merely mentions
/// the branch — an `rf integrate` merge inside a *different* roll, say — which
/// would make `rf promote --roll` advance stable to the wrong point.
///
/// Newest-first log order means a branch graduated more than once (graduate,
/// diverge, re-graduate) maps to its *latest* graduation, which is the one that
/// carries all of its work.
fn scan_graduated(repo: &Path, rolling_ref: &str) -> HashMap<String, String> {
    let rolling = match git::resolve_branch(repo, rolling_ref) {
        Some(r) => r,
        None => return HashMap::new(),
    };

    let mut graduated = HashMap::new();
    for args in [
        vec![
            "log",
            "--first-parent",
            "--merges",
            "--format=%H%x09%s",
            &rolling,
        ],
        vec!["log", "--merges", "--format=%H%x09%s", &rolling],
    ] {
        let Ok(out) = git::capture_git(repo, &args) else {
            continue;
        };
        for line in out.lines() {
            let Some((hash, subject)) = line.split_once('\t') else {
                continue;
            };
            if let Some(branch) = extract_graduated_branch(subject) {
                graduated.entry(branch).or_insert_with(|| hash.to_string());
            }
        }
    }
    graduated
}

/// Short reference form used in hotfix merge subjects: the branch
/// `hotfix/N-MMDD-slug` renders as `hotfix/N-slug` (date dropped).
pub fn hotfix_short_name(branch: &str) -> Option<String> {
    let rest = branch.strip_prefix(HOTFIX_PREFIX)?;
    let mut parts = rest.splitn(3, '-');
    let number = parts.next()?;
    let _mmdd = parts.next()?;
    let slug = parts.next()?;
    if number.is_empty() || slug.is_empty() {
        return None;
    }
    Some(format!("{HOTFIX_PREFIX}{number}-{slug}"))
}

/// Collect every hotfix branch (local + remote, deduplicated) with its state,
/// sorted ascending by number. A hotfix is landed once its merge is on the
/// stable branch — read from merge subjects exactly as promotion is.
pub fn list_hotfixes(config: &Config) -> Result<Vec<HotfixInfo>, RfError> {
    let repo = &config.repo_root;
    let current = git::current_branch(repo).unwrap_or_default();
    let pattern = format!("{HOTFIX_PREFIX}*");

    let mut names = git::local_branches(repo, &pattern)?;
    names.extend(git::remote_branches(repo, &pattern)?);
    names.sort();
    names.dedup();

    let landed = scan_landed_hotfixes(repo, &config.stable_branch);

    let mut hotfixes = Vec::new();
    for branch in names {
        let Some(number) = parse_roll_number(&branch, HOTFIX_PREFIX) else {
            continue;
        };
        let location = match (
            git::ref_exists(repo, &branch),
            git::ref_exists(repo, &format!("origin/{branch}")),
        ) {
            (true, true) => BranchLocation::Both,
            (true, false) => BranchLocation::Local,
            (false, true) => BranchLocation::Remote,
            _ => BranchLocation::Neither,
        };
        // The landing subject names the *short* form; a hand-made
        // `Merge branch 'hotfix/…'` names the full one. Either counts.
        let is_landed = landed.contains(&branch)
            || hotfix_short_name(&branch)
                .map(|short| landed.contains(&short))
                .unwrap_or(false);
        hotfixes.push(HotfixInfo {
            is_current: branch == current,
            branch,
            number,
            state: if is_landed {
                HotfixState::Landed
            } else {
                HotfixState::Open
            },
            location,
        });
    }
    hotfixes.sort_by_key(|h| h.number);
    Ok(hotfixes)
}

/// Names (short or full) of hotfixes whose landing merge is reachable from
/// `stable_ref`. One log pass, like [`scan_promoted`].
fn scan_landed_hotfixes(repo: &Path, stable_ref: &str) -> HashSet<String> {
    let Some(stable) = git::resolve_branch(repo, stable_ref) else {
        return HashSet::new();
    };
    let subjects = git::log_subjects(repo, &["--merges", &stable]).unwrap_or_default();
    subjects
        .iter()
        .filter_map(|s| extract_landed_hotfix(s).or_else(|| extract_graduated_branch(s)))
        .filter(|name| name.starts_with(HOTFIX_PREFIX))
        .collect()
}

/// Extract the hotfix named as the *source* of a landing subject:
/// `Hotfix hotfix/N-slug into main` yields `hotfix/N-slug`.
///
/// A small parallel to [`extract_graduated_branch`] rather than a new arm in
/// it, because that function answers "which roll graduated" and this answers a
/// different question with a different vocabulary. It keeps the same rule,
/// though: the ` into ` clause names the target and is cut first, so a subject
/// can never be read as landing the branch it landed *on*.
fn extract_landed_hotfix(subject: &str) -> Option<String> {
    let head = match subject.find(" into ") {
        Some(at) => &subject[..at],
        None => subject,
    };
    let rest = head.strip_prefix("Hotfix ")?;
    let name = rest.split_whitespace().next()?;
    name.starts_with(HOTFIX_PREFIX).then(|| name.to_string())
}

/// Scan stable log once, returning the set of branch names that have been
/// promoted. Three sources over a single log pass:
/// 1. `Promote <roll> …` subjects (single-roll promotions),
/// 2. graduation subjects reachable from stable — the promote merge of rolling
///    carries every graduation merge along, so reachability marks those rolls
///    promoted (covers multi-roll `Promote <rolling> to <stable>` commits),
/// 3. `Rolls:` body lines of Promote commits (explicit attribution).
fn scan_promoted(repo: &Path, stable_ref: &str) -> HashSet<String> {
    let stable = match git::resolve_branch(repo, stable_ref) {
        Some(r) => r,
        None => return HashSet::new(),
    };

    let mut promoted = HashSet::new();
    for (subject, body) in git::log_with_body(repo, &[&stable]).unwrap_or_default() {
        if let Some(rest) = subject.strip_prefix("Promote ") {
            if let Some(branch) = rest.split_whitespace().next() {
                promoted.insert(branch.to_string());
            }
            for line in body.lines() {
                let line = line.trim();
                if !line.is_empty() && line != "Rolls:" {
                    // Lenient: non-branch lines are harmless — the result is
                    // matched against actual roll branch names.
                    promoted.insert(line.to_string());
                }
            }
        } else if let Some(branch) = extract_graduated_branch(&subject) {
            promoted.insert(branch);
        }
    }
    promoted
}

/// Which rolls a "verify many" pass covers. A user-facing menu shared by
/// `rf verify --all --state` and the TUI's `[V]`, so the two can never offer
/// different sets. `Local` is a location, not a state: it exists because
/// verification runs in the working tree, so a remote-only roll can only ever be
/// skipped, and a user with many of those wants a set that never mentions them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum VerifySet {
    All,
    Active,
    Blocked,
    Graduated,
    Diverged,
    Local,
}

impl VerifySet {
    /// Every set, in the order menus list them.
    pub const ALL: [VerifySet; 6] = [
        VerifySet::All,
        VerifySet::Active,
        VerifySet::Blocked,
        VerifySet::Graduated,
        VerifySet::Diverged,
        VerifySet::Local,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VerifySet::All => "all rolls",
            VerifySet::Active => "active",
            VerifySet::Blocked => "blocked",
            VerifySet::Graduated => "graduated",
            VerifySet::Diverged => "diverged",
            VerifySet::Local => "rolls with a local copy",
        }
    }

    /// The branches in this set, in table order. Promoted rolls are never
    /// included: they have nowhere left to go, and `ops::verify` would only
    /// report "nothing to merge" for each one.
    pub fn select(self, rolls: &[RollInfo]) -> Vec<String> {
        rolls
            .iter()
            .filter(|r| r.state != RollState::Promoted)
            .filter(|r| match self {
                VerifySet::All => true,
                VerifySet::Active => r.state == RollState::Active,
                VerifySet::Blocked => r.state == RollState::Blocked,
                VerifySet::Graduated => r.state == RollState::Graduated,
                VerifySet::Diverged => r.state == RollState::Diverged,
                VerifySet::Local => {
                    matches!(r.location, BranchLocation::Local | BranchLocation::Both)
                }
            })
            .map(|r| r.branch.clone())
            .collect()
    }
}

/// The ref to read a branch's file content at: its own tip when there is a
/// local copy, `origin/<branch>` when the branch only exists on the remote.
///
/// The same local-first order as [`git::resolve_branch`], in one place, because
/// the TUI table and both plain tables all have to agree about which commit a
/// branch's version was read from.
pub fn content_ref(branch: &str, location: &BranchLocation) -> String {
    match location {
        BranchLocation::Remote => format!("origin/{branch}"),
        _ => branch.to_string(),
    }
}

/// Extract the branch name from a graduation subject line. Handles the three
/// merge-subject shapes a roll can land through:
/// - `Merge branch 'roll/N-...'[ into ...]` — a local `git merge --no-ff`.
/// - `Graduate roll/N-... [...]` — an `rf graduate` structured merge.
/// - `Merge pull request #M from OWNER/roll/N-...` — a GitHub PR merge, which is
///   how PR-based repos (including roll-flow dogfooding itself) land rolls.
/// - anything else beginning with `merge` that names a branch — see the fallback.
///
/// Matching is case-insensitive, and that is not cosmetic. A merge that
/// conflicts drops the user in an editor, and what comes back is whatever they
/// wrote: `merge branch 'roll/8-0918-help-menu'`, lowercase and without the
/// `into` clause, is in this repo's own history. The anchored, case-sensitive
/// match this replaced returned `None` for it, and since this one function is
/// how *every* consumer reads a merge subject, that single miss took the roll's
/// dependency, its graduation and its graduation commit with it.
pub(crate) fn extract_graduated_branch(subject: &str) -> Option<String> {
    // Cut the ` into ` clause first. It names the merge *target*, never the
    // source, and dropping it is what makes the lenient fallback below safe:
    // `Merge branch 'roll/8-x' into roll/7-y` must never yield roll/7, because
    // on the rolling branch that reads as "roll/7 graduated" — a far worse
    // failure than missing a dependency.
    let subject = before_into(subject);

    // The PR shape is tried first because its token carries the owner
    // (`OWNER/roll/N-…`), which the fallback would hand back verbatim.
    if let Some(rest) = strip_prefix_ci(subject, "Merge pull request #") {
        return rest
            .split_once(" from ")
            .and_then(|(_, owner_branch)| owner_branch.split_once('/'))
            .map(|(_owner, branch)| branch.trim().to_string());
    }
    if let Some(rest) = strip_prefix_ci(subject, "Merge branch '") {
        // e.g. "roll/5-theme'" or "roll/5-theme' into rolling"
        return rest.split('\'').next().map(|b| b.to_string());
    }
    if let Some(rest) = strip_prefix_ci(subject, "Graduate ") {
        // e.g. "roll/5-theme into rolling"
        return rest.split_whitespace().next().map(|b| b.to_string());
    }

    // Hand-written merges: `merge roll/8-0918-help-menu`, `Merged roll/8-x`.
    // Gated on the subject still announcing itself as a merge, which is what
    // keeps `Revert "Merge branch 'roll/8-x'"` from reading as a graduation.
    // The token is returned unvalidated on purpose — every caller already
    // matches it against real roll branch names or runs it through
    // `parse_roll_number`, so a candidate that is not a roll simply never
    // matches anything.
    strip_prefix_ci(subject, "merge")?;
    subject
        .split_whitespace()
        .map(|token| token.trim_matches(['\'', '"', ',', '.']))
        .find(|token| token.contains('/'))
        .map(|token| token.to_string())
}

/// `strip_prefix`, ignoring ASCII case.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|head| &s[head.len()..])
}

/// The part of `subject` before a ` into ` clause, which names the merge target.
///
/// The index comes from an ASCII-lowercased copy, which is byte-for-byte the
/// same length as the original — `to_ascii_lowercase` only maps `A-Z` — so it
/// stays a valid index into `subject` even when the subject holds multi-byte
/// characters.
fn before_into(subject: &str) -> &str {
    match subject.to_ascii_lowercase().find(" into ") {
        Some(at) => &subject[..at],
        None => subject,
    }
}

fn subjects_contain_graduation(subjects: &[String], roll_branch: &str) -> bool {
    subjects
        .iter()
        .filter_map(|s| extract_graduated_branch(s))
        .any(|b| b == roll_branch)
}

/// Roll numbers this roll has *directly integrated* via `rf integrate`
/// (`git merge --no-ff <branch>`).
///
/// Detected by parsing the roll's own first-parent merge history in the range
/// `<base>..<roll>`. `--first-parent` combined with that range restricts results
/// to merges THIS roll introduced (direct integrations), excluding transitive
/// ones carried in by an integrated roll's own history. Each subject matching
/// `Merge branch 'roll/<N>-…'` yields `<N>`.
///
/// `base_ref` is chosen by [`deps_base_ref`] — the stable branch for an
/// ungraduated roll, the graduation merge's first parent otherwise. It may be a
/// raw revision (`<sha>^1`) rather than a branch name, so it is resolved with
/// `rev_parse` and only falls back to branch resolution.
///
/// This is the only dependency signal that gates blocking: file overlap and
/// broad ancestry are intentionally excluded (see `list_rolls`) — with one
/// exception. A direct merge of the rolling branch itself (`[I]` / `rf
/// integrate <rolling>`) produces a subject naming rolling, not a roll, so the
/// subject scan alone finds nothing — yet that merge brings in every roll
/// already graduated onto it. When one of the merges in range names the
/// rolling branch, `graduated` (every known graduated branch mapped to its
/// graduation commit, already computed once by `list_rolls`) is consulted:
/// any graduation commit newly reachable from this roll is a real dependency,
/// acquired in that one merge.
///
/// "Newly reachable" is load-bearing, not merely an ancestor of the roll's
/// tip: a graduation from months ago is an ancestor of nearly every branch
/// created afterward, because `rf promote` folds it into stable and every
/// roll forks from stable. Without excluding `base`'s own ancestry too, every
/// roll that ever did an `[I]` merge reports a dependency on the *entire*
/// graduation history of the repo — the exact "broad ancestry" explosion the
/// paragraph above says this function avoids. Requiring the commit to be
/// absent from `base` restricts it to graduations this merge actually
/// introduced, matching the `base..roll` scoping the subject scan above
/// already uses.
fn integration_deps(
    repo: &Path,
    roll_branch: &str,
    roll_num: u32,
    prefix: &str,
    base_ref: &str,
    rolling_branch: &str,
    graduated: &HashMap<String, String>,
) -> Vec<u32> {
    let (Some(roll_ref), Some(base)) = (
        git::resolve_branch(repo, roll_branch),
        git::rev_parse(repo, base_ref)
            .ok()
            .or_else(|| git::resolve_branch(repo, base_ref)),
    ) else {
        return Vec::new();
    };

    let range = format!("{base}..{roll_ref}");
    let subjects =
        git::log_subjects(repo, &["--first-parent", "--merges", &range]).unwrap_or_default();

    let mut merged_rolling = false;
    let mut deps: Vec<u32> = Vec::new();
    for branch in subjects.iter().filter_map(|s| extract_graduated_branch(s)) {
        match parse_roll_number(&branch, prefix) {
            Some(n) if n != roll_num => deps.push(n),
            Some(_) => {}
            None if branch == rolling_branch => merged_rolling = true,
            None => {}
        }
    }

    if merged_rolling {
        for (branch, commit) in graduated {
            if branch == roll_branch {
                continue;
            }
            let Some(n) = parse_roll_number(branch, prefix) else {
                continue;
            };
            let newly_reachable = git::is_ancestor(repo, commit, &roll_ref).unwrap_or(false)
                && !git::is_ancestor(repo, commit, &base).unwrap_or(true);
            if n != roll_num && newly_reachable {
                deps.push(n);
            }
        }
    }

    deps.sort_unstable();
    deps.dedup();
    deps
}

/// True if `dep_branch`'s current tip is not reachable from `roll_branch` —
/// i.e. `roll_branch` does not (yet, or any longer) contain `dep_branch`'s
/// latest work. Used to populate [`RollInfo::stale_deps`]: this is a plain
/// `git merge-base --is-ancestor` check against each branch's *current* ref
/// (local preferred, `origin/<branch>` as fallback, same as every other
/// branch lookup here), not a comparison against the point `dep_branch` was
/// originally integrated — so it answers "is there anything new to pick up",
/// which is what matters before reintegrating or merging a batch of
/// dependents. Either branch failing to resolve answers `false`: no claim of
/// staleness can be made about a branch that no longer exists.
fn dep_tip_missing(repo: &Path, dep_branch: &str, roll_branch: &str) -> bool {
    let (Some(dep_ref), Some(roll_ref)) = (
        git::resolve_branch(repo, dep_branch),
        git::resolve_branch(repo, roll_branch),
    ) else {
        return false;
    };
    !git::is_ancestor(repo, &dep_ref, &roll_ref).unwrap_or(true)
}

/// Find the git hash of the merge/graduation commit for `roll_branch` on
/// `rolling_ref` — pass a stable ref to get the graduation once it has been
/// promoted. Returns `None` if no graduation commit is found. Scans merge
/// commits and matches subjects through [`extract_graduated_branch`], so all
/// three merge-subject shapes (local `Merge branch`, `Graduate`, and GitHub
/// `Merge pull request`) are recognized from one source of truth.
fn find_graduation_commit(repo: &Path, roll_branch: &str, rolling_ref: &str) -> Option<String> {
    let out =
        git::capture_git(repo, &["log", "--merges", "--format=%H%x09%s", rolling_ref]).ok()?;
    for line in out.lines() {
        let Some((hash, subject)) = line.split_once('\t') else {
            continue;
        };
        if extract_graduated_branch(subject).as_deref() == Some(roll_branch) {
            return Some(hash.to_string());
        }
    }
    None
}

/// Whether `event_hash` is currently "in effect" on `range_head`'s history,
/// by chaining `git revert`'s own `` "This reverts commit <hash>." `` body
/// line: a revert flips the parity, and the revert commit itself becomes the new
/// thing a later revert-of-the-revert must reference (so a revert, undone,
/// re-reverted, ... is tracked correctly rather than only one level deep).
///
/// Returns the hash to revert *now* to restore `event_hash`'s effect — its
/// own revert, or the latest un-reverting revert still standing in the chain
/// — or `None` when `event_hash` is currently in effect (never reverted, or
/// reverted an even number of times).
///
/// Matched on the body rather than the subject: that boilerplate line is
/// pre-filled by git itself and tends to survive even a conflicted revert's
/// hand-edited subject (the editor opens with it already there), the same
/// leniency [`extract_graduated_branch`] relies on for hand-written merges.
fn find_active_revert_in_range(repo: &Path, event_hash: &str, range_head: &str) -> Option<String> {
    let range = format!("{event_hash}..{range_head}");
    // Oldest first, so the chain is walked in the order it actually happened.
    let out =
        git::capture_git(repo, &["log", "--reverse", &range, "--format=%H%x09%b%x00"]).ok()?;

    let mut current = event_hash.to_string();
    let mut reverted = false;
    for entry in out.split('\0') {
        let Some((hash, body)) = entry.split_once('\t') else {
            continue;
        };
        let reverts_current = body.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("This reverts commit") && line.contains(&current)
        });
        if reverts_current {
            reverted = !reverted;
            current = hash.to_string();
        }
    }

    reverted.then_some(current)
}

/// Scan `stable_ref`'s own mainline for `Promote <branch> to <stable>`
/// merges — the mirror of [`scan_graduated`], but for the single-roll
/// promotion shape (`rf promote --roll`) rather than graduation. Maps each
/// promoted branch to the hash of that promotion commit, which is what a
/// revert on stable would target.
///
/// Deliberately narrow, same caveat as [`check_promotion_reverted`]: the
/// multi-roll `Promote <rolling> to <stable>` shape names `<rolling>`, not a
/// roll, so it is harmlessly recorded under a key no roll branch matches.
fn scan_promotion_commits(repo: &Path, stable_ref: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(out) = git::capture_git(
        repo,
        &[
            "log",
            "--first-parent",
            "--merges",
            "--format=%H%x09%s",
            stable_ref,
        ],
    ) else {
        return map;
    };
    for line in out.lines() {
        let Some((hash, subject)) = line.split_once('\t') else {
            continue;
        };
        if let Some(rest) = subject.strip_prefix("Promote ") {
            if let Some(branch) = rest.split_whitespace().next() {
                map.entry(branch.to_string())
                    .or_insert_with(|| hash.to_string());
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::{
        extract_graduated_branch, extract_landed_hotfix, hotfix_short_name, parse_roll_number,
        HOTFIX_PREFIX,
    };

    #[test]
    fn hotfix_names_parse_and_shorten() {
        assert_eq!(
            parse_roll_number("hotfix/3-0720-urgent", HOTFIX_PREFIX),
            Some(3)
        );
        assert_eq!(
            hotfix_short_name("hotfix/3-0720-urgent-fix").as_deref(),
            Some("hotfix/3-urgent-fix")
        );
        assert_eq!(hotfix_short_name("hotfix/3-0720"), None);
        assert_eq!(hotfix_short_name("roll/3-0720-x"), None);
    }

    #[test]
    fn a_landing_subject_names_its_source_never_its_target() {
        assert_eq!(
            extract_landed_hotfix("Hotfix hotfix/1-urgent into main").as_deref(),
            Some("hotfix/1-urgent")
        );
        // The reintegration merge that follows a landing names stable as its
        // source and must not read as a landed hotfix.
        assert_eq!(
            extract_landed_hotfix("Reintegrate main into rolling (hotfix hotfix/1-urgent)"),
            None
        );
        assert_eq!(extract_landed_hotfix("Hotfix roll/1-x into main"), None);
        assert_eq!(extract_landed_hotfix("chore: bump"), None);
    }

    #[test]
    fn parses_roll_number() {
        assert_eq!(parse_roll_number("roll/12-0611-cli", "roll/"), Some(12));
        assert_eq!(parse_roll_number("roll/x-0611-cli", "roll/"), None);
        assert_eq!(parse_roll_number("feature/foo", "roll/"), None);
    }

    #[test]
    fn extracts_branch_from_all_merge_subject_shapes() {
        // Local `git merge --no-ff`.
        assert_eq!(
            extract_graduated_branch("Merge branch 'roll/5-0611-theme' into develop").as_deref(),
            Some("roll/5-0611-theme")
        );
        // `rf graduate` structured merge.
        assert_eq!(
            extract_graduated_branch("Graduate roll/5-0611-theme into develop").as_deref(),
            Some("roll/5-0611-theme")
        );
        // GitHub PR merge — the format PR-based repos (and dogfooding) land through.
        assert_eq!(
            extract_graduated_branch(
                "Merge pull request #87 from gignsky/roll/16-0721-tui-dependents"
            )
            .as_deref(),
            Some("roll/16-0721-tui-dependents")
        );
        // A non-roll PR merge extracts the branch but won't parse as a roll number.
        assert_eq!(
            extract_graduated_branch("Merge pull request #99 from gignsky/claude/some-fix")
                .as_deref(),
            Some("claude/some-fix")
        );
        assert_eq!(parse_roll_number("claude/some-fix", "roll/"), None);
        // Unrelated subject.
        assert_eq!(extract_graduated_branch("chore: bump version"), None);
    }

    #[test]
    fn extracts_branch_from_a_hand_written_merge_subject() {
        // The literal subject on roll/7 in this repo, kept as a regression
        // fixture: the merge conflicted, so the subject was typed by hand and
        // came back lowercase and without the `into` clause. The anchored,
        // case-sensitive match this replaced returned `None` here, which is how
        // roll/7 came to have no recorded dependency on roll/8.
        assert_eq!(
            extract_graduated_branch("merge branch 'roll/8-0918-help-menu'").as_deref(),
            Some("roll/8-0918-help-menu")
        );
        // No `branch`, no quotes — still a merge, still names its source.
        assert_eq!(
            extract_graduated_branch("merge roll/8-0918-help-menu").as_deref(),
            Some("roll/8-0918-help-menu")
        );
        assert_eq!(
            extract_graduated_branch("Merged roll/8-0918-help-menu into the keymap").as_deref(),
            Some("roll/8-0918-help-menu")
        );
        // Case is ignored on the structured shapes too.
        assert_eq!(
            extract_graduated_branch("graduate roll/5-0611-theme into develop").as_deref(),
            Some("roll/5-0611-theme")
        );
    }

    #[test]
    fn a_merge_subject_never_yields_its_target() {
        // The safety property the lenient fallback rests on. Reporting the
        // target would mark an unmerged roll as graduated, which is far worse
        // than missing a dependency — so the ` into ` clause is cut before
        // anything else looks at the subject.
        for subject in [
            "Merge branch 'roll/8-x' into roll/7-y",
            "merge branch 'roll/8-x' INTO roll/7-y",
            "merge roll/8-x into roll/7-y",
        ] {
            assert_eq!(
                extract_graduated_branch(subject).as_deref(),
                Some("roll/8-x"),
                "{subject}"
            );
        }
        // A merge whose source is not a roll yields the source, not the target,
        // and simply matches no roll downstream.
        assert_eq!(
            extract_graduated_branch("Merge branch 'feature/x' into roll/7-y").as_deref(),
            Some("feature/x")
        );
        assert_eq!(parse_roll_number("feature/x", "roll/"), None);
    }

    #[test]
    fn a_revert_is_not_a_graduation() {
        // The fallback is gated on the subject announcing itself as a merge.
        // Without that gate this reads as "roll/8 graduated" — the exact
        // opposite of what the commit did.
        assert_eq!(
            extract_graduated_branch(r#"Revert "Merge branch 'roll/8-0918-help-menu'""#),
            None
        );
        // And an ordinary subject that happens to mention a branch is untouched.
        assert_eq!(
            extract_graduated_branch("docs: explain roll/8-0918-help-menu"),
            None
        );
    }
}
