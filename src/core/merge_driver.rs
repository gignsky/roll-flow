//! Git merge driver for a crate or package's own version — the `version`
//! line in its manifest (`Cargo.toml` or `package.json`), and the same value
//! repeated in its lockfile's own entry (`Cargo.lock`'s `[[package]]`, or
//! `package-lock.json`'s root/`packages[""]`).
//!
//! A roll's `-roll<N>` marker and wherever the branch it's merging with has
//! moved to both touch the exact same line, which a plain 3-way text merge
//! reads as a real conflict — the bug `rf update` and `rf graduate` used to
//! work around with extra commits and rollback logic. This replaces that:
//! wired up as the `merge` attribute for both of the detected kind's files
//! (see `ops::ensure_version_merge_driver`, which `rf init` and every `rf`
//! merge call), it resolves the version line itself before a plain text merge
//! ever gets a chance to see it as conflicting, for *every* merge that
//! touches the files — not just the ones `rf` orchestrates.
//!
//! Both files, because they change in lockstep: every version rewrite
//! refreshes the lockfile's own entry too, so two branches with different
//! markers disagree on *both* lines, and resolving only the manifest still
//! stops the merge on the lockfile. Only the package's own lockfile entry is
//! ever touched; a dependency that moved differently on each side is a real
//! conflict and surfaces as one.
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
//!
//! [`merge_texts`] is the rule applied to three in-memory sides. Git's driver
//! entry point ([`run`]) is one caller; `ops`'s own merge path is the other,
//! for a merge that stopped because git could not run the driver at all (it
//! was never configured in this clone, or `rf` is not on git's `PATH`).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::core::git;
use crate::core::version::{ManifestKind, Role, Semver};
use crate::error::RfError;

/// Resolve two sides of a version-line disagreement. See the module docs for
/// the rule and why it's safe in every direction this project merges in.
pub fn resolve(ours: Semver, theirs: Semver) -> Semver {
    ours.release()
        .max(theirs.release())
        .with_marker(ours.marker)
}

/// Which of a kind's two files is being merged, with the kind itself —
/// `(ManifestKind::Cargo, Role::Manifest)` for `Cargo.toml`, and so on. They
/// hold the same value in different shapes, so the rule is shared and only
/// reading and rewriting the line differ, which [`ManifestKind`]'s own
/// methods already do.
type VersionFile = (ManifestKind, Role);

fn read_version_file(
    (kind, role): VersionFile,
    text: &str,
    package: Option<&str>,
) -> Option<Semver> {
    match role {
        Role::Manifest => kind.parse_version(text),
        Role::Lockfile => kind.parse_lock_version(text, package),
    }
}

fn rewrite_version_file(
    (kind, role): VersionFile,
    text: &str,
    package: Option<&str>,
    new: Semver,
) -> Option<String> {
    match role {
        Role::Manifest => kind.replace_version(text, new),
        Role::Lockfile => kind.replace_lock_version(text, package, new),
    }
}

/// The crate/package name a lockfile at `path` (repo-relative) keys its own
/// entry by, read from the sibling manifest as committed at `HEAD` — ours,
/// during a merge — falling back to the working tree's copy.
///
/// `HEAD` first because git merges the lockfile before the manifest (index
/// order), so mid-merge the working tree's manifest is not something to lean
/// on; a package's name is the same on both sides in any merge this resolves.
pub fn package_name(repo: &Path, path: &Path) -> Option<String> {
    let (kind, _) = ManifestKind::for_path(path)?;
    let manifest = path.with_file_name(kind.manifest());
    let rel = manifest.to_str()?.replace('\\', "/");
    let text = match git::show_file_at_ref(repo, "HEAD", &rel) {
        Ok(Some(text)) => text,
        _ => fs::read_to_string(repo.join(&manifest)).ok()?,
    };
    kind.parse_name(&text)
}

/// Three-way merge three in-memory sides of a version-carrying file, with the
/// version line settled by [`resolve`] first.
///
/// `Ok(Some(merged))` only for a clean result. `Ok(None)` when there is
/// nothing to resolve (no readable version on one side, no `package` name for
/// a lockfile that needs one) or when the merge still conflicts once the
/// version line agrees — which means something else in the file is a real
/// conflict, and the caller must surface it rather than this deciding
/// anything about it.
pub fn merge_texts(
    file: VersionFile,
    package: Option<&str>,
    base: &str,
    ours: &str,
    theirs: &str,
) -> Result<Option<String>, RfError> {
    let (Some(o), Some(t)) = (
        read_version_file(file, ours, package),
        read_version_file(file, theirs, package),
    ) else {
        return Ok(None);
    };
    let resolved = resolve(o, t);

    // Doctor every side to already agree on the resolved line: a 3-way merge
    // where both branches *and* the ancestor carry the same value never sees
    // that line as changed at all, so it can't even crowd an unrelated edit on
    // a neighbouring line into a conflict. Anything `merge-file` still
    // conflicts on after that is real and unrelated to the version line. The
    // ancestor is left as-is when it has no version to rewrite (the file was
    // added on both sides) — the merge just has less to go on.
    let (Some(ours), Some(theirs)) = (
        rewrite_version_file(file, ours, package, resolved),
        rewrite_version_file(file, theirs, package, resolved),
    ) else {
        return Ok(None);
    };
    let base =
        rewrite_version_file(file, base, package, resolved).unwrap_or_else(|| base.to_string());

    let (clean, merged) = merge_file(&base, &ours, &theirs)?;
    Ok(clean.then(|| String::from_utf8_lossy(&merged).into_owned()))
}

/// Entry point for the `__merge-driver-version` subcommand.
///
/// `ancestor`/`ours`/`theirs` are the paths git's `%O`/`%A`/`%B` substitute in
/// — temp files holding each side's full content. `path` is `%P`, the file's
/// repo-relative name, which says which kind of file this is; it is optional
/// only so a clone still configured with the older three-argument command
/// keeps working (as the Cargo-manifest-only driver it was) until the next
/// `rf` merge rewires it. The result must land back in `ours`, which is where
/// git reads the merged content from regardless of outcome. Returns
/// `Ok(true)` for a clean resolution and `Ok(false)` for a real conflict (in
/// which case `ours` already holds a normal conflict-marked merge, exactly
/// what git would have produced with no driver at all).
pub fn run(
    ancestor: &Path,
    ours: &Path,
    theirs: &Path,
    path: Option<&Path>,
) -> Result<bool, RfError> {
    let file = path
        .and_then(ManifestKind::for_path)
        .unwrap_or((ManifestKind::Cargo, Role::Manifest));
    // Git runs a merge driver from the top of the working tree.
    let package = match (file, path) {
        ((_, Role::Lockfile), Some(path)) => package_name(Path::new("."), path),
        _ => None,
    };

    let base_text = fs::read_to_string(ancestor)?;
    let ours_text = fs::read_to_string(ours)?;
    let theirs_text = fs::read_to_string(theirs)?;

    match merge_texts(
        file,
        package.as_deref(),
        &base_text,
        &ours_text,
        &theirs_text,
    )? {
        Some(merged) => {
            fs::write(ours, merged)?;
            Ok(true)
        }
        None => plain_merge(ancestor, ours, theirs),
    }
}

/// `git merge-file -p` over three in-memory sides: whether it merged cleanly,
/// and the result (conflict markers included when it did not).
fn merge_file(base: &str, ours: &str, theirs: &str) -> Result<(bool, Vec<u8>), RfError> {
    // Unique per call, not just per process: `ops` runs this for two files in
    // a row from the same process.
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let tag = format!(
        "rf-merge-driver-{}-{}",
        std::process::id(),
        CALLS.fetch_add(1, Ordering::Relaxed)
    );
    let dir = std::env::temp_dir();
    let paths = ["base", "ours", "theirs"].map(|side| dir.join(format!("{tag}-{side}")));
    let written = paths
        .iter()
        .zip([base, ours, theirs])
        .try_for_each(|(p, text)| fs::write(p, text));

    let output = written.and_then(|()| {
        Command::new("git")
            .arg("merge-file")
            .arg("-p")
            .arg(&paths[1])
            .arg(&paths[0])
            .arg(&paths[2])
            .output()
    });
    for p in &paths {
        let _ = fs::remove_file(p);
    }
    let output = output?;
    Ok((output.status.success(), output.stdout))
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

/// Which kind/role `path`'s file name is, if any.
pub fn for_path(path: &Path) -> Option<VersionFile> {
    ManifestKind::for_path(path)
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

    fn lock(own: &str, serde: &str) -> String {
        format!(
            "version = 4\n\n[[package]]\nname = \"roll-flow\"\nversion = \"{own}\"\n\
             dependencies = [\n \"serde\",\n]\n\n[[package]]\nname = \"serde\"\n\
             version = \"{serde}\"\nsource = \"registry+https://example\"\n"
        )
    }

    const CARGO_LOCK: VersionFile = (ManifestKind::Cargo, Role::Lockfile);

    #[test]
    fn the_lockfile_s_own_entry_follows_the_same_rule() {
        // The reported conflict: two rolls, each wearing its own marker in
        // the lockfile, merged into one another.
        let merged = merge_texts(
            CARGO_LOCK,
            Some("roll-flow"),
            &lock("0.2.7", "1.0.0"),
            &lock("0.2.7-roll35", "1.0.0"),
            &lock("0.2.8-roll11", "1.0.0"),
        )
        .unwrap();
        assert_eq!(merged, Some(lock("0.2.8-roll35", "1.0.0")));
    }

    #[test]
    fn a_dependency_conflict_in_the_lockfile_is_left_to_the_caller() {
        let merged = merge_texts(
            CARGO_LOCK,
            Some("roll-flow"),
            &lock("0.2.7", "1.0.0"),
            &lock("0.2.7-roll35", "1.0.1"),
            &lock("0.2.7-roll11", "1.0.2"),
        )
        .unwrap();
        assert_eq!(merged, None);
    }

    #[test]
    fn a_lockfile_without_a_package_name_is_not_guessed_at() {
        let merged = merge_texts(
            CARGO_LOCK,
            None,
            &lock("0.2.7", "1.0.0"),
            &lock("0.2.7-roll35", "1.0.0"),
            &lock("0.2.7-roll11", "1.0.0"),
        )
        .unwrap();
        assert_eq!(merged, None);
    }

    #[test]
    fn npm_lockfile_follows_the_same_rule_with_no_name_needed() {
        fn npm_lock(own: &str) -> String {
            format!(
                "{{\n  \"name\": \"fixture\",\n  \"version\": \"{own}\",\n  \
                 \"packages\": {{ \"\": {{ \"name\": \"fixture\", \"version\": \"{own}\" }} }}\n}}"
            )
        }
        let merged = merge_texts(
            (ManifestKind::Npm, Role::Lockfile),
            None,
            &npm_lock("0.2.7"),
            &npm_lock("0.2.7-roll35"),
            &npm_lock("0.2.8-roll11"),
        )
        .unwrap();
        assert_eq!(merged, Some(npm_lock("0.2.8-roll35")));
    }

    #[test]
    fn a_dependency_conflict_in_the_npm_lockfile_is_left_to_the_caller() {
        // Only the package's own root/`packages[""]` entry is the driver's
        // business. Two rolls that moved the same dependency differently are
        // a real conflict, the npm twin of
        // `a_dependency_conflict_in_the_lockfile_is_left_to_the_caller`.
        fn npm_lock(own: &str, dep: &str) -> String {
            format!(
                "{{\n  \"name\": \"fixture\",\n  \"version\": \"{own}\",\n  \
                 \"packages\": {{\n    \"\": {{ \"name\": \"fixture\", \"version\": \"{own}\" }},\n    \
                 \"node_modules/leftpad\": {{ \"version\": \"{dep}\" }}\n  }}\n}}"
            )
        }
        let merged = merge_texts(
            (ManifestKind::Npm, Role::Lockfile),
            None,
            &npm_lock("0.2.7", "1.0.0"),
            &npm_lock("0.2.7-roll35", "1.0.1"),
            &npm_lock("0.2.7-roll11", "1.0.2"),
        )
        .unwrap();
        assert_eq!(merged, None);
    }

    #[test]
    fn files_are_recognised_by_name_at_any_depth() {
        assert_eq!(
            for_path(Path::new("Cargo.lock")),
            Some((ManifestKind::Cargo, Role::Lockfile))
        );
        assert_eq!(
            for_path(Path::new("crates/x/Cargo.toml")),
            Some((ManifestKind::Cargo, Role::Manifest))
        );
        assert_eq!(
            for_path(Path::new("package-lock.json")),
            Some((ManifestKind::Npm, Role::Lockfile))
        );
        assert_eq!(for_path(Path::new("README.md")), None);
    }
}
