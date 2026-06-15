use std::collections::HashSet;
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
    pub deps: Vec<u32>,
}

// ── Public API ────────────────────────────────────────────────────────────────

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
    let graduated_set = scan_graduated(repo, &config.rolling_branch);
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
        let is_graduated = graduated_set.contains(&branch);

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
        });
    }

    rolls.sort_by_key(|r| r.number);

    // Compute deps for active rolls and promote to Blocked when needed.
    let snapshot = rolls.clone();
    for roll in &mut rolls {
        if roll.state == RollState::Active {
            roll.deps = get_deps(
                repo,
                &roll.branch,
                roll.number,
                &snapshot,
                &config.stable_branch,
            );
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

    Ok(rolls)
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

/// True if the roll has a promotion commit on the stable branch.
pub fn check_promoted(repo: &Path, roll_branch: &str, stable_ref: &str) -> bool {
    let stable = match git::resolve_branch(repo, stable_ref) {
        Some(r) => r,
        None => return false,
    };
    let subjects = git::log_subjects(repo, &[&stable]).unwrap_or_default();
    subjects
        .iter()
        .any(|s| s.starts_with(&format!("Promote {roll_branch}")))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Scan rolling log once, returning the set of branch names that have been
/// graduated.  Much cheaper than one `git log` call per roll.
fn scan_graduated(repo: &Path, rolling_ref: &str) -> HashSet<String> {
    let rolling = match git::resolve_branch(repo, rolling_ref) {
        Some(r) => r,
        None => return HashSet::new(),
    };
    git::log_subjects(repo, &[&rolling])
        .unwrap_or_default()
        .iter()
        .filter_map(|s| extract_graduated_branch(s))
        .collect()
}

/// Scan stable log once, returning the set of branch names that have been
/// promoted.
fn scan_promoted(repo: &Path, stable_ref: &str) -> HashSet<String> {
    let stable = match git::resolve_branch(repo, stable_ref) {
        Some(r) => r,
        None => return HashSet::new(),
    };
    git::log_subjects(repo, &[&stable])
        .unwrap_or_default()
        .iter()
        .filter_map(|s| {
            s.strip_prefix("Promote ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(|b| b.to_string())
        })
        .collect()
}

/// Extract the branch name from a graduation subject line.
/// Handles both `Merge branch 'roll/N-...'[...]` and `Graduate roll/N-... [...]`.
fn extract_graduated_branch(subject: &str) -> Option<String> {
    if let Some(rest) = subject.strip_prefix("Merge branch '") {
        // e.g. "roll/5-theme'" or "roll/5-theme' into rolling"
        rest.split('\'').next().map(|b| b.to_string())
    } else if let Some(rest) = subject.strip_prefix("Graduate ") {
        // e.g. "roll/5-theme into rolling"
        rest.split_whitespace().next().map(|b| b.to_string())
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

/// Compute deps for one roll using git ancestry (method 2) and file overlap (method 3).
/// Only considers lower-numbered, non-promoted rolls as candidates.
fn get_deps(
    repo: &Path,
    roll_branch: &str,
    roll_num: u32,
    all_rolls: &[RollInfo],
    stable_ref: &str,
) -> Vec<u32> {
    let roll_ref = match git::resolve_branch(repo, roll_branch) {
        Some(r) => r,
        None => return Vec::new(),
    };

    let candidates: Vec<&RollInfo> = all_rolls
        .iter()
        .filter(|r| r.number < roll_num && r.state != RollState::Promoted)
        .collect();

    let mut deps = Vec::new();

    // Method 2: if another roll's tip is an ancestor of this roll, it's a hard dep.
    for other in &candidates {
        let other_ref = match git::resolve_branch(repo, &other.branch) {
            Some(r) => r,
            None => continue,
        };
        if git::is_ancestor(repo, &other_ref, &roll_ref).unwrap_or(false) {
            deps.push(other.number);
        }
    }

    // Method 3: file overlap — rolls touching the same files are ordered deps.
    let stable = git::resolve_branch(repo, stable_ref).unwrap_or_else(|| stable_ref.to_string());
    let our_files: HashSet<String> = git::diff_name_only(repo, &stable, &roll_ref)
        .unwrap_or_default()
        .into_iter()
        .collect();

    if !our_files.is_empty() {
        for other in &candidates {
            if deps.contains(&other.number) {
                continue;
            }
            let other_ref = match git::resolve_branch(repo, &other.branch) {
                Some(r) => r,
                None => continue,
            };
            let other_files: HashSet<String> = git::diff_name_only(repo, &stable, &other_ref)
                .unwrap_or_default()
                .into_iter()
                .collect();
            if our_files.iter().any(|f| other_files.contains(f)) {
                deps.push(other.number);
            }
        }
    }

    deps.sort_unstable();
    deps
}

/// Find the git hash of the merge/graduation commit for `roll_branch` on
/// `rolling_ref`.  Returns `None` if no graduation commit is found.
fn find_graduation_commit(repo: &Path, roll_branch: &str, rolling_ref: &str) -> Option<String> {
    for pattern in [
        format!("Merge branch '{roll_branch}'"),
        format!("Graduate {roll_branch}"),
    ] {
        if let Ok(hash) = git::capture_git(
            repo,
            &[
                "log",
                "--format=%H",
                &format!("--grep={pattern}"),
                "--fixed-strings",
                "-1",
                rolling_ref,
            ],
        ) {
            if !hash.is_empty() {
                return Some(hash);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_roll_number;

    #[test]
    fn parses_roll_number() {
        assert_eq!(parse_roll_number("roll/12-0611-cli", "roll/"), Some(12));
        assert_eq!(parse_roll_number("roll/x-0611-cli", "roll/"), None);
        assert_eq!(parse_roll_number("feature/foo", "roll/"), None);
    }
}
