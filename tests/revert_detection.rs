//! Detecting a graduation (or promotion) that was later undone by a
//! `git revert`, driven through the shared [`harness::Sandbox`].
//!
//! A reverted graduation shows up as `RollState::Reverted` (`"↩ reverted"`),
//! and `rf graduate` is the remediation: it must notice the revert and revert
//! *that* commit, rather than attempting its usual `--no-ff` merge of the
//! roll branch. An ordinary re-merge cannot undo a revert — the roll branch's
//! own tip remains an ancestor of rolling either way, since the revert adds a
//! new commit on top rather than removing anything from history, so the merge
//! has nothing new to bring in.
//!
//! A reverted *promotion* shows up as `RollState::Demoted` (`"↩ demoted"`).
//! That side is detect-and-display only this round — fixing it stays a manual
//! `git revert` on the stable branch, since automating it would mean teaching
//! the per-roll promotion pipeline (version gate, release tags, carried-rolls
//! disclosure) a second kind of step.

mod harness;

use harness::Sandbox;

#[test]
fn reverted_graduation_reports_reverted_state() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    let merge = sb.rev("rolling");
    sb.revert_merge("rolling", &merge);

    let (file_survives, _, _) = sb.git_try(&["show", "rolling:work.txt"]);
    assert!(
        !file_survives,
        "the revert should have removed work.txt from rolling"
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("↩ reverted"),
        "a reverted graduation should report as reverted, not graduated"
    );
}

#[test]
fn graduate_restores_a_reverted_graduation_by_reverting_the_revert() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    let merge = sb.rev("rolling");
    sb.revert_merge("rolling", &merge);
    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("↩ reverted")
    );

    // `revert_merge` leaves HEAD back on the roll branch, so this is exactly
    // what a user would type next.
    assert_eq!(sb.current_branch(), "roll/1-0611-feature");
    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduate should restore the reverted graduation: {}",
        out.combined()
    );

    assert_eq!(
        sb.git(&["show", "rolling:work.txt"]),
        "w",
        "the content should be back after un-reverting"
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("✓ graduated"),
        "once restored, the roll should report as graduated again"
    );
}

#[test]
fn manually_reverting_the_revert_also_clears_reverted_state() {
    // Confirms the detector tracks the full revert/un-revert chain itself,
    // not just rf's own remediation path.
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    let merge = sb.rev("rolling");
    sb.revert_merge("rolling", &merge);
    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("↩ reverted")
    );

    let revert_commit = sb.rev("rolling");
    sb.revert_commit("rolling", &revert_commit);

    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("✓ graduated"),
        "reverting the revert by hand should clear the reverted state too"
    );
}

#[test]
fn graduate_on_a_merely_diverged_roll_is_unaffected() {
    // A plain Diverged roll (new commits since graduation, no revert in
    // sight) must still go through the ordinary merge path.
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    sb.commit_file("more.txt", "m\n", "more roll work");
    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("⚠ diverged")
    );

    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "re-graduating a diverged roll: {}",
        out.combined()
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("✓ graduated")
    );
}

#[test]
fn reverted_promotion_reports_demoted_state() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    sb.git(&["checkout", "rolling"]);
    assert!(sb.rf(&["promote"]).success, "promote");

    let promotion = sb.rev("main");
    assert!(
        sb.tip_is_merge("main"),
        "promotion should be a merge commit"
    );
    sb.revert_merge("main", &promotion);

    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("↩ demoted"),
        "a reverted promotion should report as demoted"
    );
}
