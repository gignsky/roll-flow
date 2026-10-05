//! Git merge driver for `Cargo.toml`'s `version` line.
//!
//! A roll's `-roll<N>` marker and wherever the branch it's merging with has
//! moved to both touch the exact same line, which a plain 3-way text merge
//! reads as a real conflict — the bug `rf update` and `rf graduate` used to
//! work around with extra commits and rollback logic. This replaces that:
//! wired up (by `rf init`, see `ops::ensure_version_merge_driver`) as the
//! `merge` attribute for `Cargo.toml`, it resolves the version line itself
//! before a plain text merge ever gets a chance to see it as conflicting, for
//! *every* merge that touches the file — not just the ones `rf` orchestrates.
//!
//! The rule, symmetric across every direction this project merges in:
//!
//! - `rf graduate` (roll merged into rolling; rolling is `ours`): rolling's own
//!   `-dev` marker is kept, and the numeric part becomes whichever side is
//!   higher — in practice the roll's, since that's the point of graduating.
//! - `rf integrate`/`[i]`/`[I]` (another roll, or rolling, merged into a roll;
//!   the roll is `ours`): the roll's own `-roll<N>` marker is kept, numbers
//!   maxed the same way.
//! - `rf update` (stable merged into a roll; the roll is `ours`): same rule
//!   again — the roll's marker is kept, numeric part raised to stable's.
//!
//! All three are the same rule: **keep `ours`'s marker; take the higher of the
//! two sides' `X.Y.Z` numbers.** "Ours" is whichever branch is checked out
//! when the merge runs, which git itself decides — this module never needs to
//! know which `rf` command triggered it, or which marker kind "ours" happens
//! to carry.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::core::version::{self, Semver};
use crate::error::RfError;

/// Resolve two sides of a version-line disagreement. See the module docs for
/// the rule and why it's safe in every direction this project merges in.
pub fn resolve(ours: Semver, theirs: Semver) -> Semver {
    ours.release()
        .max(theirs.release())
        .with_marker(ours.marker)
}

/// Entry point for the `__merge-driver-version` subcommand.
///
/// `ancestor`/`ours`/`theirs` are the paths git's `%O`/`%A`/`%B` substitute in
/// — temp files holding each side's full `Cargo.toml` content. The result
/// must land back in `ours`, which is where git reads the merged content from
/// regardless of outcome. Returns `Ok(true)` for a clean resolution and
/// `Ok(false)` for a real conflict (in which case `ours` already holds a
/// normal conflict-marked merge, exactly what git would have produced with no
/// driver at all).
pub fn run(ancestor: &Path, ours: &Path, theirs: &Path) -> Result<bool, RfError> {
    let ours_text = fs::read_to_string(ours)?;
    let theirs_text = fs::read_to_string(theirs)?;

    let resolved = match (
        version::parse_version(&ours_text),
        version::parse_version(&theirs_text),
    ) {
        (Some(o), Some(t)) => Some(resolve(o, t)),
        // No version to reconcile on one side (no `[package]` table, say) —
        // nothing for this driver to add; fall through to a plain merge.
        _ => None,
    };

    let Some(resolved) = resolved else {
        return plain_merge(ancestor, ours, theirs);
    };

    // Doctor both sides to already agree on the resolved line: a 3-way merge
    // where both branches end up with the same value is never a conflict,
    // whatever the ancestor said. Anything `merge-file` still conflicts on
    // after that is real and unrelated to the version line.
    let (Some(ours_doctored), Some(theirs_doctored)) = (
        version::replace_package_version(&ours_text, resolved),
        version::replace_package_version(&theirs_text, resolved),
    ) else {
        return plain_merge(ancestor, ours, theirs);
    };

    let pid = std::process::id();
    let tmp_ours = std::env::temp_dir().join(format!("rf-merge-driver-{pid}-ours"));
    let tmp_theirs = std::env::temp_dir().join(format!("rf-merge-driver-{pid}-theirs"));
    fs::write(&tmp_ours, &ours_doctored)?;
    fs::write(&tmp_theirs, &theirs_doctored)?;

    let output = Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg(&tmp_ours)
        .arg(ancestor)
        .arg(&tmp_theirs)
        .output();
    let _ = fs::remove_file(&tmp_ours);
    let _ = fs::remove_file(&tmp_theirs);
    let output = output?;

    if output.status.success() {
        fs::write(ours, &output.stdout)?;
        return Ok(true);
    }

    plain_merge(ancestor, ours, theirs)
}

/// A plain three-way text merge with no version-line handling at all — git's
/// own behavior with no driver installed. Used both as the fallback when
/// there's nothing for this driver to resolve, and when a resolved merge
/// still conflicts on something else: in that case the result (conflict
/// markers included) is exactly what the user would see without this driver.
fn plain_merge(ancestor: &Path, ours: &Path, theirs: &Path) -> Result<bool, RfError> {
    let output = Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg(ours)
        .arg(ancestor)
        .arg(theirs)
        .output()?;
    fs::write(ours, &output.stdout)?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Semver {
        Semver::parse(s).unwrap()
    }

    #[test]
    fn graduate_direction_keeps_rolling_s_dev_marker() {
        // ours = rolling (its own -dev), theirs = the roll being graduated.
        // The roll's -roll10 is dropped in favor of ours's -dev, numbers maxed.
        assert_eq!(resolve(v("0.2.6-dev"), v("0.2.5-roll10")), v("0.2.6-dev"));
        assert_eq!(resolve(v("0.2.5-dev"), v("0.2.6-roll10")), v("0.2.6-dev"));
    }

    #[test]
    fn integrate_direction_keeps_the_roll_s_own_marker() {
        // ours = the roll being integrated into, theirs = another roll (or
        // rolling's -dev). The roll keeps its own number, not theirs.
        assert_eq!(
            resolve(v("0.2.5-roll33"), v("0.2.6-roll10")),
            v("0.2.6-roll33")
        );
        assert_eq!(
            resolve(v("0.2.5-roll33"), v("0.2.6-dev")),
            v("0.2.6-roll33")
        );
    }

    #[test]
    fn update_direction_keeps_the_marker_and_raises_the_number() {
        // ours = the roll being updated, theirs = stable (just bumped).
        assert_eq!(resolve(v("0.2.5-roll10"), v("0.2.6")), v("0.2.6-roll10"));
    }

    #[test]
    fn equal_versions_are_a_no_op() {
        assert_eq!(resolve(v("0.2.5-roll10"), v("0.2.5")), v("0.2.5-roll10"));
    }

    #[test]
    fn ours_numeric_already_higher_stays_put() {
        // The roll bumped its own base past stable's current tip; stable's
        // number must not regress it.
        assert_eq!(resolve(v("0.3.0-roll10"), v("0.2.6")), v("0.3.0-roll10"));
    }

    #[test]
    fn finalizing_drops_the_dev_marker() {
        // ours = stable (no marker), theirs = rolling's -dev, as happens if a
        // bare merge ever reached this driver instead of going through
        // `ops::finalize_rolling` first. Stable must never gain a marker.
        assert_eq!(resolve(v("0.2.5"), v("0.2.6-dev")), v("0.2.6"));
    }
}
