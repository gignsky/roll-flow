//! CLI flow integration tests, driven through the shared [`harness::Sandbox`].
//!
//! Post-epic-#2 semantics: `graduate` (roll→rolling) and `promote`
//! (rolling→main) are explicit `--no-ff`, divergence-tolerant merges that write
//! the structured subjects the state detector reads.

mod harness;

use harness::Sandbox;

#[test]
fn init_create_graduate_promote_flow() {
    let sb = Sandbox::plain();

    assert!(sb.init().success, "init");
    assert!(sb.create_roll("feature", "0611").success, "create");
    sb.commit_file("work.txt", "w\n", "roll work");
    let roll_tip = sb.rev("HEAD");

    // graduate roll -> rolling as a --no-ff merge with a structured subject.
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());
    assert!(
        sb.tip_is_merge("rolling"),
        "expected a --no-ff graduation merge"
    );
    assert!(
        sb.tip_subject("rolling").starts_with("Graduate roll/1"),
        "unexpected graduation subject: {}",
        sb.tip_subject("rolling")
    );
    // Second parent is the roll tip, so divergence detection's `^2` works.
    assert_eq!(sb.rev("rolling^2"), roll_tip);
    // The command leaves you back on the roll, not on rolling.
    assert_eq!(sb.current_branch(), "roll/1-0611-feature");

    // promote rolling -> main, also a --no-ff merge.
    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(
        sb.tip_is_merge("main"),
        "expected a --no-ff promotion merge"
    );
    assert!(
        sb.tip_subject("main").starts_with("Promote rolling"),
        "unexpected promotion subject: {}",
        sb.tip_subject("main")
    );
    // The roll's content is now on the stable branch.
    assert_eq!(sb.git(&["show", "main:work.txt"]), "w");
}

#[test]
fn graduate_requires_clean_tree() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init");
    assert!(sb.create_roll("dirty", "0611").success, "create");

    sb.write("dirty.txt", "x\n"); // uncommitted change
    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "graduate should fail on a dirty tree");
    assert!(
        out.combined().contains("working tree must be clean"),
        "unexpected: {}",
        out.combined()
    );
}

#[test]
fn integrate_merges_branch_into_roll() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init");

    // A feature branch with unique content.
    sb.git(&["checkout", "-b", "feature/foo"]);
    sb.commit_file("foo.txt", "feature content\n", "add foo feature");

    // Create a roll (rf create checks out the new roll branch).
    assert!(sb.create_roll("myroll", "0101").success, "create");

    // Integrate the feature branch into the roll.
    let out = sb.rf(&["integrate", "feature/foo"]);
    assert!(out.success, "integrate: {}", out.combined());

    assert!(sb.exists("foo.txt"), "foo.txt missing after integrate");
    assert!(
        sb.tip_subject("HEAD").contains("Merge branch"),
        "expected merge commit, got: {}",
        sb.tip_subject("HEAD")
    );
}

#[test]
fn promote_from_main_errors_clearly() {
    // `main` is not promotable — this must be a clear error, not a panic and
    // not the old fast-forward rejection. (Replaces the former
    // `promote_blocks_divergence`; divergence is now supported — see
    // `e2e_lifecycle::graduation_tolerates_diverged_rolling`.)
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init");

    let out = sb.rf(&["promote"]); // HEAD is on main after init
    assert!(!out.success, "promote from main should error");
    assert!(
        out.combined().contains("not promotable"),
        "unexpected: {}",
        out.combined()
    );
}
