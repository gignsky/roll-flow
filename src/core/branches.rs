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
    Promoted,  // merged to main
    Blocked,   // active but has ungraduated dependencies
}

impl RollState {
    pub fn label(&self) -> &'static str {
        match self {
            RollState::Active => "active",
            RollState::Graduated => "✓ graduated",
            RollState::Diverged => "⚠ diverged",
            RollState::Promoted => "✓ promoted",
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
    /// Hash of this roll's graduation merge on the rolling branch, or `None`
    /// when it has not graduated. This is the commit `rf promote --roll` merges
    /// into stable: advancing stable to it promotes exactly this roll (and
    /// whatever graduated before it), which keeps stable a prefix of rolling.
    pub graduation_commit: Option<String>,
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
            RollState::Promoted
        } else if is_graduated {
            // Only divergence-check graduated (not-yet-promoted) rolls.
            if check_diverged(repo, &branch, &config.rolling_branch) {
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
            graduation_commit,
        });
    }

    rolls.sort_by_key(|r| r.number);

    // Compute deps for *every* roll, whatever its state. Deps are the roll's
    // actual direct integrations — the roll branches it pulled in via
    // `rf integrate` — detected from its own first-parent merge history. File
    // overlap and broad ancestry are deliberately NOT used here: in a dotfiles
    // repo nearly every roll touches flake.lock, which made them spuriously
    // block one another.
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
        );
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
/// Checks both `Merge branch 'roll/N-...'` and `Graduate roll/N-...` formats.
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
fn extract_graduated_branch(subject: &str) -> Option<String> {
    if let Some(rest) = subject.strip_prefix("Merge branch '") {
        // e.g. "roll/5-theme'" or "roll/5-theme' into rolling"
        rest.split('\'').next().map(|b| b.to_string())
    } else if let Some(rest) = subject.strip_prefix("Graduate ") {
        // e.g. "roll/5-theme into rolling"
        rest.split_whitespace().next().map(|b| b.to_string())
    } else if subject.starts_with("Merge pull request #") {
        // "Merge pull request #M from OWNER/BRANCH" — split off the owner only,
        // since BRANCH itself contains '/' (e.g. "gignsky/roll/5-theme").
        subject
            .split_once(" from ")
            .and_then(|(_, owner_branch)| owner_branch.split_once('/'))
            .map(|(_owner, branch)| branch.trim().to_string())
    } else {
        None
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
/// broad ancestry are intentionally excluded (see `list_rolls`).
fn integration_deps(
    repo: &Path,
    roll_branch: &str,
    roll_num: u32,
    prefix: &str,
    base_ref: &str,
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

    let mut deps: Vec<u32> = subjects
        .iter()
        .filter_map(|s| extract_graduated_branch(s))
        .filter_map(|b| parse_roll_number(&b, prefix))
        .filter(|&n| n != roll_num)
        .collect();

    deps.sort_unstable();
    deps.dedup();
    deps
}

/// Find the git hash of the merge/graduation commit for `roll_branch` on
/// `rolling_ref` — pass a stable ref to get the graduation once it has been
/// promoted, which is what the future revert flow (issue #38) needs. Returns
/// `None` if no graduation commit is found. Scans merge
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

#[cfg(test)]
mod tests {
    use super::{extract_graduated_branch, parse_roll_number};

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
}
