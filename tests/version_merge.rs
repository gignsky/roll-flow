//! Version-only merge conflicts between dev-marked branches.
//!
//! Every roll carries its own `-roll<N>` marker in `Cargo.toml` *and* in the
//! package's own `Cargo.lock` entry, so any merge between two of them —
//! `rf integrate`, `[i]`/`[I]`, `rf update`, graduation — touches the same
//! version line on both sides of both files. That must never stop a merge:
//! the result keeps ours's marker at the higher of the two numbers (see
//! `core::merge_driver`). These tests reproduce the real failure (`rf
//! integrate 11` from roll 35 stopping on both files) and pin down the three
//! things that had to change for it not to happen again: the lockfile is
//! covered, the driver is wired whenever `rf` merges rather than only after
//! `rf init`, and `rf`'s own merge path resolves a version-only conflict even
//! when git could not run the driver at all. Anything else that conflicts must
//! still stop the merge exactly as git left it.

mod harness;

use harness::Sandbox;

/// `Sandbox::cargo()` plus a committed `Cargo.lock` at the same version, then
/// `rf init` — the shape of this very repo.
fn locked_sandbox() -> Sandbox {
    let sb = Sandbox::cargo();
    sb.commit_cargo_lock("0.0.1", "1.0.0");
    sb.init();
    sb
}

/// `rf create` a roll, then make sure both version files say `version`
/// (whatever `rf create` already wrote is kept when it matches), and give it a
/// commit of its own. Leaves HEAD on the new roll and returns its branch.
fn marked_roll(sb: &Sandbox, slug: &str, date: &str, version: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create {slug}: {}", out.combined());
    if sb.cargo_version_at("HEAD") != version {
        sb.commit_cargo_version(version);
    }
    sb.commit_cargo_lock(version, "1.0.0");
    sb.commit_file(&format!("{slug}.txt"), "work\n", &format!("{slug} work"));
    sb.current_branch()
}

/// Roll 1 (`alpha`, at the higher numbers) and roll 2 (`beta`, checked out),
/// each wearing its own marker in both files — the reported scenario, with
/// the numbers deliberately apart so "take the higher" is observable.
fn two_marked_rolls(sb: &Sandbox) -> (String, String) {
    let alpha = marked_roll(sb, "alpha", "0611", "0.0.3-roll1");
    sb.git(&["checkout", "main"]);
    let beta = marked_roll(sb, "beta", "0612", "0.0.1-roll2");
    (alpha, beta)
}

fn assert_merged_cleanly(sb: &Sandbox, branch: &str) {
    assert_eq!(sb.current_branch(), branch);
    assert!(sb.tip_is_merge("HEAD"), "no merge commit was made");
    assert!(sb.unmerged_paths().is_empty(), "{:?}", sb.unmerged_paths());
    assert!(
        !sb.git_try(&["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
            .0,
        "a merge was left in progress"
    );
}

// ── creating ────────────────────────────────────────────────────────────────

#[test]
fn create_marks_the_lockfile_entry_too() {
    // The lockfile's own entry is derived from `Cargo.toml`, not left to a
    // `cargo update` that may not run (offline, no cargo, a fixture that is
    // not a buildable crate). If it stayed behind, the first gate run with
    // `--locked` would fail on it, and so would the marker-vs-marker merges
    // below in a way no driver ever sees.
    let sb = locked_sandbox();
    let out = sb.create_roll("alpha", "0611");
    assert!(out.success, "{}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
    assert_eq!(sb.lock_version_at("HEAD"), "0.0.1-roll1");
    // Only the package's own entry: a dependency is never touched.
    assert_eq!(sb.lock_entry_at("HEAD", "serde"), "1.0.0");
}

// ── wiring ──────────────────────────────────────────────────────────────────

#[test]
fn init_wires_the_driver_for_both_files_without_touching_the_tree() {
    let sb = locked_sandbox();
    let attrs = sb.git(&["check-attr", "merge", "Cargo.toml", "Cargo.lock"]);
    assert!(
        attrs.contains("Cargo.toml: merge: rf-version"),
        "Cargo.toml not attributed: {attrs}"
    );
    assert!(
        attrs.contains("Cargo.lock: merge: rf-version"),
        "Cargo.lock not attributed: {attrs}"
    );
    // Clone-local, like the driver command it names: nothing for the user to
    // commit, and nothing that depends on which branch is checked out.
    let status = sb.git(&["status", "--porcelain"]);
    let stray: Vec<&str> = status
        .lines()
        .filter(|l| !l.ends_with(".roll-flow.toml"))
        .collect();
    assert!(stray.is_empty(), "init dirtied the tree: {stray:?}");
}

// ── rf integrate ────────────────────────────────────────────────────────────

#[test]
fn integrating_another_marked_roll_keeps_ours_marker_in_both_files() {
    // The reported bug: `rf integrate 11` from roll 35 stopped with
    // `CONFLICT (content)` in both Cargo.toml and Cargo.lock.
    let sb = locked_sandbox();
    let (_alpha, beta) = two_marked_rolls(&sb);

    let out = sb.rf(&["integrate", "1"]);
    assert!(out.success, "integrate stopped: {}", out.combined());
    assert_merged_cleanly(&sb, &beta);
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.3-roll2");
    assert_eq!(sb.lock_version_at("HEAD"), "0.0.3-roll2");
    assert_eq!(sb.lock_entry_at("HEAD", "serde"), "1.0.0");
}

#[test]
fn integrate_wires_the_driver_itself_on_a_clone_that_never_ran_init() {
    // How it really happened: the git-config half of the driver is local to
    // a clone, and nothing but `rf init` ever set it.
    let sb = locked_sandbox();
    let (_alpha, beta) = two_marked_rolls(&sb);
    let _ = sb.git_try(&["config", "--local", "--unset", "merge.rf-version.driver"]);
    let attrs = sb.git(&["rev-parse", "--git-path", "info/attributes"]);
    let _ = std::fs::remove_file(sb.path().join(attrs));
    let _ = std::fs::remove_file(sb.path().join(".gitattributes"));

    let out = sb.rf(&["integrate", "1"]);
    assert!(out.success, "integrate stopped: {}", out.combined());
    assert_merged_cleanly(&sb, &beta);
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.3-roll2");
    assert_eq!(sb.lock_version_at("HEAD"), "0.0.3-roll2");

    // And it stays wired for the next merge, `rf`'s or a hand-run one.
    let (ok, driver, _) = sb.git_try(&["config", "--get", "merge.rf-version.driver"]);
    assert!(ok, "the driver was not configured");
    assert!(driver.contains("__merge-driver-version"), "{driver}");
}

#[test]
fn integrate_resolves_a_version_only_conflict_even_when_git_cannot_run_the_driver() {
    // `rf` not on PATH: git's driver invocation fails, the merge stops on both
    // files, and `rf`'s own merge path has to finish it by the same rule.
    let sb = locked_sandbox();
    let (_alpha, beta) = two_marked_rolls(&sb);

    let out = sb.rf_without_driver_on_path(&["integrate", "1"]);
    assert!(out.success, "integrate stopped: {}", out.combined());
    assert_merged_cleanly(&sb, &beta);
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.3-roll2");
    assert_eq!(sb.lock_version_at("HEAD"), "0.0.3-roll2");
    // The merge commit is git's ordinary one, with no `# Conflicts:` residue.
    let subject = sb.tip_subject("HEAD");
    assert!(subject.starts_with("Merge branch 'roll/1-"), "{subject}");
    let body = sb.git(&["log", "-1", "--format=%B"]);
    assert!(!body.contains("Conflicts"), "{body}");
}

#[test]
fn a_real_conflict_still_stops_integrate_and_leaves_the_merge_as_git_left_it() {
    let sb = locked_sandbox();
    let alpha = marked_roll(&sb, "alpha", "0611", "0.0.3-roll1");
    sb.commit_file("clash.txt", "alpha side\n", "alpha clash");
    sb.git(&["checkout", "main"]);
    let beta = marked_roll(&sb, "beta", "0612", "0.0.1-roll2");
    sb.commit_file("clash.txt", "beta side\n", "beta clash");

    // Without the driver, so nothing has pre-resolved the version files
    // either: the fallback must decline *all* of them, not finish the parts
    // it understands and leave the rest.
    let out = sb.rf_without_driver_on_path(&["integrate", &alpha]);
    assert!(!out.success, "integrate should stop: {}", out.combined());
    assert_eq!(sb.current_branch(), beta);
    assert!(
        sb.git_try(&["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
            .0
    );
    assert_eq!(
        sb.unmerged_paths(),
        vec!["Cargo.lock", "Cargo.toml", "clash.txt"]
    );
}

#[test]
fn a_dependency_conflict_in_the_lockfile_is_not_swallowed() {
    // Only the package's own entry is the driver's business. Two rolls that
    // moved the same dependency differently are a real conflict.
    let sb = locked_sandbox();
    let alpha = marked_roll(&sb, "alpha", "0611", "0.0.3-roll1");
    sb.commit_cargo_lock("0.0.3-roll1", "1.0.1");
    sb.git(&["checkout", "main"]);
    marked_roll(&sb, "beta", "0612", "0.0.1-roll2");
    sb.commit_cargo_lock("0.0.1-roll2", "1.0.2");

    let out = sb.rf(&["integrate", &alpha]);
    assert!(!out.success, "integrate should stop: {}", out.combined());
    assert!(
        sb.unmerged_paths().contains(&"Cargo.lock".to_string()),
        "{:?}",
        sb.unmerged_paths()
    );
    let lock = std::fs::read_to_string(sb.path().join("Cargo.lock")).unwrap();
    assert!(lock.contains("<<<<<<<"), "{lock}");
}

// ── rf update ───────────────────────────────────────────────────────────────

#[test]
fn update_brings_in_a_stable_bump_without_conflicting_on_either_file() {
    let sb = locked_sandbox();
    let roll = marked_roll(&sb, "alpha", "0611", "0.0.1-roll1");
    sb.git(&["checkout", "main"]);
    sb.commit_cargo_version("0.0.2");
    sb.commit_cargo_lock("0.0.2", "1.0.0");
    sb.git(&["checkout", &roll]);

    let out = sb.rf_without_driver_on_path(&["update"]);
    assert!(out.success, "update failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at(&roll), "0.0.2-roll1");
    assert_eq!(sb.lock_version_at(&roll), "0.0.2-roll1");
}

// ── rf graduate ─────────────────────────────────────────────────────────────

#[test]
fn graduation_still_forces_dev_in_both_files_through_a_version_conflict() {
    // The second graduation is the conflicting one: rolling already wears
    // `-dev` in both files, the roll its own `-roll2`. Graduation's own rule
    // (force `-dev`, see `reconcile_staged_version`) must still win over the
    // generic "keep ours" one, and the lockfile has to agree with it.
    let sb = locked_sandbox();
    let alpha = marked_roll(&sb, "alpha", "0611", "0.0.1-roll1");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate alpha: {}", out.combined());
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1-dev");
    assert_eq!(sb.lock_version_at("rolling"), "0.0.1-dev");
    assert_eq!(sb.cargo_version_at(&alpha), "0.0.1-roll1");

    sb.git(&["checkout", "main"]);
    marked_roll(&sb, "beta", "0612", "0.0.2-roll2");
    let out = sb.rf_without_driver_on_path(&["graduate"]);
    assert!(out.success, "graduate beta: {}", out.combined());
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.2-dev");
    assert_eq!(sb.lock_version_at("rolling"), "0.0.2-dev");
}
