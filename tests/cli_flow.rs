//! CLI flow integration tests, driven through the shared [`harness::Sandbox`].
//!
//! These assert the merge-based (`--no-ff`) graduate/promote workflow from
//! epic #2: structured merge subjects, divergence tolerance, and clear errors
//! for genuinely-bad states. The broader lifecycle scenarios live in
//! `e2e_lifecycle.rs`.

mod harness;

use harness::Sandbox;

#[test]
fn init_create_and_promote_flow() {
    let sb = Sandbox::plain();

    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    sb.commit_file("work.txt", "w\n", "roll work");

    let out = sb.rf(&["verify"]);
    assert!(out.success, "verify failed: {}", out.combined());

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    // Graduation is a --no-ff merge with a structured subject, and we are
    // returned to the roll branch afterward.
    assert!(sb.tip_is_merge("rolling"), "rolling tip should be a merge");
    assert!(
        sb.tip_subject("rolling").starts_with("Graduate roll/1"),
        "unexpected graduation subject: {}",
        sb.tip_subject("rolling")
    );
    assert_eq!(sb.current_branch(), "roll/1-0611-feature");

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(
        out.success,
        "promote rolling->main failed: {}",
        out.combined()
    );

    assert!(sb.tip_is_merge("main"), "main tip should be a merge");
    assert!(
        sb.tip_subject("main").starts_with("Promote "),
        "unexpected promotion subject: {}",
        sb.tip_subject("main")
    );
    assert!(
        sb.is_ancestor("rolling", "main"),
        "rolling should be merged into main"
    );
}

#[test]
fn promote_requires_clean_tree() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("dirty", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    sb.write("dirty.txt", "x\n");
    let out = sb.rf(&["promote"]);
    assert!(!out.success, "promote should fail on dirty tree");
    assert!(
        out.combined().contains("working tree must be clean"),
        "unexpected: {}",
        out.combined()
    );
}

#[test]
fn integrate_merges_branch_into_roll() {
    let sb = Sandbox::plain();

    let out = sb.init();
    assert!(out.success, "init: {}", out.combined());

    // A feature branch with unique content.
    sb.git(&["checkout", "-b", "feature/foo"]);
    sb.commit_file("foo.txt", "feature content\n", "add foo feature");

    // Create a roll (rf create checks out the new roll branch).
    let out = sb.create_roll("myroll", "0101");
    assert!(out.success, "create: {}", out.combined());

    // Integrate the feature branch into the roll.
    let out = sb.rf(&["integrate", "feature/foo"]);
    assert!(out.success, "integrate: {}", out.combined());

    // Feature content should now exist on the roll branch.
    assert!(sb.exists("foo.txt"), "foo.txt missing after integrate");

    // Should have been a --no-ff merge commit.
    assert!(
        sb.tip_subject("HEAD").contains("Merge branch"),
        "expected merge commit, got: {}",
        sb.tip_subject("HEAD")
    );
}

#[test]
fn promote_tolerates_divergence() {
    // Divergence is a supported state (epic #2): `rf promote` on a roll branch
    // falls through to graduation and merges with --no-ff even when rolling
    // has advanced independently.
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    sb.git(&["checkout", "rolling"]);
    sb.git(&["checkout", "-b", "roll/1-0611-diverge"]);
    sb.commit_file("roll.txt", "roll\n", "roll commit");

    sb.git(&["checkout", "rolling"]);
    sb.commit_file("rolling.txt", "rolling\n", "rolling diverges");

    sb.git(&["checkout", "roll/1-0611-diverge"]);
    let out = sb.rf(&["promote"]);
    assert!(
        out.success,
        "promote should tolerate divergence: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("use rf graduate"),
        "expected the graduate redirect note: {}",
        out.combined()
    );
    assert!(
        sb.tip_is_merge("rolling"),
        "expected a --no-ff merge on rolling"
    );
    assert!(
        sb.tip_subject("rolling").starts_with("Graduate roll/1"),
        "unexpected subject: {}",
        sb.tip_subject("rolling")
    );
}

#[test]
fn graduate_errors_off_roll_branch() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    // Still on `main` after init.
    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "graduate should fail off a roll branch");
    assert!(
        out.combined().contains("must be run from a roll branch"),
        "unexpected: {}",
        out.combined()
    );
}

#[test]
fn graduate_errors_with_nothing_to_merge() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    // Fresh roll with no commits of its own.
    let out = sb.create_roll("empty", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "graduate should fail with nothing to merge");
    assert!(
        out.combined().contains("nothing to graduate"),
        "unexpected: {}",
        out.combined()
    );
}
