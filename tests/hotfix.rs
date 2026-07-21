//! Integration tests for the `hotfix/*` tier and `rf hotfix` (issue #37).
//!
//! Covers the approved design from epic #36: a hotfix branches off the stable
//! branch with its own numbering, and `--land` merges it into stable then
//! reintegrates stable into rolling so the tiers never silently diverge.

mod harness;

use harness::Sandbox;

#[test]
fn hotfix_creates_branch_off_stable() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");

    // Advance rolling beyond stable so the two tips genuinely differ.
    sb.git(&["checkout", "rolling"]);
    sb.commit_file("rolling-only.txt", "r\n", "rolling advances");
    sb.git(&["checkout", "main"]);

    let out = sb.rf(&["hotfix", "urgent", "--date", "0720"]);
    assert!(out.success, "hotfix create failed: {}", out.combined());

    let branch = "hotfix/1-0720-urgent";
    assert!(sb.branch_exists(branch), "hotfix branch should exist");
    assert_eq!(
        sb.current_branch(),
        branch,
        "hotfix create should check out the new branch"
    );

    // Branched off stable: main is an ancestor; rolling's extra commit is not.
    assert!(
        sb.is_ancestor("main", branch),
        "stable should be an ancestor of the hotfix"
    );
    assert!(
        !sb.is_ancestor("rolling", branch),
        "rolling's extra commits must NOT be in the hotfix"
    );
}

#[test]
fn hotfix_numbering_is_independent_of_rolls() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");

    // A roll exists at number 1; the first hotfix should still be number 1.
    assert!(sb.create_roll("feature", "0611").success, "create roll");
    sb.git(&["checkout", "main"]);

    let out = sb.rf(&["hotfix", "urgent", "--date", "0720"]);
    assert!(out.success, "hotfix create failed: {}", out.combined());
    assert!(
        sb.branch_exists("hotfix/1-0720-urgent"),
        "hotfix numbering is independent of rolls"
    );
}

#[test]
fn hotfix_land_merges_stable_then_reintegrates_rolling() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");

    let out = sb.rf(&["hotfix", "urgent", "--date", "0720"]);
    assert!(out.success, "hotfix create failed: {}", out.combined());
    let hotfix = "hotfix/1-0720-urgent";
    sb.commit_file("fix.txt", "patch\n", "urgent fix");

    let out = sb.rf(&["hotfix", "--land"]);
    assert!(out.success, "hotfix land failed: {}", out.combined());

    // Landing merge on stable.
    assert!(sb.tip_is_merge("main"), "main tip should be a merge");
    assert_eq!(
        sb.tip_subject("main"),
        "Hotfix hotfix/1-urgent into main",
        "unexpected landing subject",
    );

    // Reintegration merge on rolling.
    assert!(sb.tip_is_merge("rolling"), "rolling tip should be a merge");
    assert_eq!(
        sb.tip_subject("rolling"),
        "Reintegrate main into rolling (hotfix hotfix/1-urgent)",
        "unexpected reintegration subject",
    );

    // The fix reached both tiers.
    assert!(
        sb.is_ancestor(hotfix, "main"),
        "hotfix should be merged into stable"
    );
    assert!(
        sb.is_ancestor("main", "rolling"),
        "stable should be reintegrated into rolling"
    );

    // Returned to where we started (the hotfix branch).
    assert_eq!(
        sb.current_branch(),
        hotfix,
        "should end on the hotfix branch"
    );
}

#[test]
fn hotfix_dry_run_makes_no_commits() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");

    // Create dry-run: no branch created.
    let out = sb.rf(&["hotfix", "urgent", "--date", "0720", "--dry-run"]);
    assert!(
        out.success,
        "hotfix create dry-run failed: {}",
        out.combined()
    );
    assert!(
        !sb.branch_exists("hotfix/1-0720-urgent"),
        "create --dry-run must not create a branch"
    );

    // Really create + add a fix, then land dry-run.
    assert!(
        sb.rf(&["hotfix", "urgent", "--date", "0720"]).success,
        "hotfix create failed"
    );
    sb.commit_file("fix.txt", "patch\n", "urgent fix");
    let stable_before = sb.rev("main");
    let rolling_before = sb.rev("rolling");

    let out = sb.rf(&["hotfix", "--land", "--dry-run"]);
    assert!(
        out.success,
        "hotfix land dry-run failed: {}",
        out.combined()
    );
    assert_eq!(
        sb.rev("main"),
        stable_before,
        "land --dry-run must not move stable"
    );
    assert_eq!(
        sb.rev("rolling"),
        rolling_before,
        "land --dry-run must not move rolling"
    );
}

#[test]
fn hotfix_land_errors_off_hotfix_branch() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");

    // Still on `main` after init.
    let out = sb.rf(&["hotfix", "--land"]);
    assert!(!out.success, "land should fail off a hotfix branch");
    assert!(
        out.combined().contains("must be run from a hotfix branch"),
        "unexpected: {}",
        out.combined()
    );
}
