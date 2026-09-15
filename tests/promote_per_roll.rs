//! `rf promote --roll` — promoting one graduated roll at a time.
//!
//! Promotion used to be all-or-nothing: `rf promote` merged the entire rolling
//! branch into stable behind a single gate run, and there was no way to move one
//! graduated roll across without taking every other graduated roll with it.
//!
//! The per-roll form advances stable to a roll's *graduation commit on rolling*,
//! never to the roll branch itself, so `main` still only ever receives merges
//! from `rolling` and stays a prefix of it. Each roll is its own merge behind its
//! own gate run; the whole-branch form remains one merge behind one gate run.

mod harness;

use harness::Sandbox;

/// Two rolls, graduated in order, with nothing promoted yet. Leaves HEAD on
/// `rolling`.
fn two_graduated_rolls() -> Sandbox {
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");

    sb.git(&["checkout", "rolling"]);
    let out = sb.create_roll("beta", "0612");
    assert!(out.success, "create beta: {}", out.combined());
    sb.commit_file("beta.txt", "b\n", "beta work");
    assert!(sb.rf(&["graduate"]).success, "graduate beta");

    sb.git(&["checkout", "rolling"]);
    sb
}

/// How many times the counting gate ran, from the file it appends to.
fn gate_runs(sb: &Sandbox) -> usize {
    if !sb.exists("gate-log") {
        return 0;
    }
    std::fs::read_to_string(sb.path().join("gate-log"))
        .expect("read gate log")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

#[test]
fn promotes_one_roll_and_leaves_the_other_graduated() {
    let sb = two_graduated_rolls();

    let out = sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]);
    assert!(out.success, "promote alpha: {}", out.combined());

    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ promoted")
    );
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ graduated"),
        "beta must not ride along: {}",
        out.combined()
    );

    assert_eq!(sb.tip_subject("main"), "Promote roll/1-0611-alpha to main");
    assert!(sb.tip_is_merge("main"), "promotion must be a --no-ff merge");

    // What was merged is a commit on rolling — alpha's graduation — not the
    // alpha branch. That is what keeps main's history a prefix of rolling's.
    // (main's *tip* is the promotion merge itself, which lives only on main, so
    // `main` is deliberately not asserted to be an ancestor of `rolling`.)
    let merged_parent = sb.git(&["rev-parse", "main^2"]);
    let alpha_graduation = sb.git(&[
        "rev-list",
        "-1",
        "--grep=Graduate roll/1-0611-alpha",
        "rolling",
    ]);
    assert_eq!(
        merged_parent.trim(),
        alpha_graduation.trim(),
        "promotion should merge alpha's graduation commit on rolling"
    );
    assert!(
        !sb.is_ancestor("rolling", "main"),
        "beta's graduation must not have reached main"
    );
}

#[test]
fn promoting_the_second_roll_afterwards_completes_the_branch() {
    let sb = two_graduated_rolls();

    assert!(
        sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]).success,
        "promote alpha"
    );
    let out = sb.rf(&["promote", "--roll", "roll/2-0612-beta"]);
    assert!(out.success, "promote beta: {}", out.combined());

    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ promoted")
    );
    assert!(
        sb.is_ancestor("rolling", "main"),
        "both graduations should now be on main"
    );
}

#[test]
fn per_roll_promotion_runs_the_gates_once_per_roll() {
    let sb = two_graduated_rolls();
    // Appends one line per invocation, so the file length is the run count.
    sb.set_promote_gates(&["echo ran >> gate-log"]);

    let out = sb.rf(&[
        "promote",
        "--roll",
        "roll/1-0611-alpha",
        "--roll",
        "roll/2-0612-beta",
    ]);
    assert!(out.success, "promote both: {}", out.combined());

    assert_eq!(
        gate_runs(&sb),
        2,
        "two rolls promoted individually must be gated twice: {}",
        out.combined()
    );
    assert!(
        sb.is_ancestor("rolling", "main"),
        "both rolls should have landed"
    );
}

#[test]
fn whole_branch_promotion_runs_the_gates_once() {
    let sb = two_graduated_rolls();
    sb.set_promote_gates(&["echo ran >> gate-log"]);

    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote: {}", out.combined());

    assert_eq!(
        gate_runs(&sb),
        1,
        "promoting the whole branch is one merge, so one gate run suffices: {}",
        out.combined()
    );
    assert_eq!(sb.tip_subject("main"), "Promote rolling to main");
}

#[test]
fn rolls_are_promoted_in_graduation_order_whatever_the_argument_order() {
    let sb = two_graduated_rolls();

    let out = sb.rf(&[
        "promote",
        "--roll",
        "roll/2-0612-beta",
        "--roll",
        "roll/1-0611-alpha",
    ]);
    assert!(out.success, "promote reversed: {}", out.combined());

    // Beta graduated last, so its promotion must be the tip.
    assert_eq!(sb.tip_subject("main"), "Promote roll/2-0612-beta to main");
    let subjects = sb.git(&["log", "--format=%s", "main"]);
    let alpha = subjects
        .lines()
        .position(|l| l == "Promote roll/1-0611-alpha to main")
        .expect("alpha promotion on main");
    let beta = subjects
        .lines()
        .position(|l| l == "Promote roll/2-0612-beta to main")
        .expect("beta promotion on main");
    // git log is newest-first, so a larger index is older.
    assert!(
        alpha > beta,
        "alpha should have been promoted first:\n{subjects}"
    );
}

#[test]
fn a_failing_gate_rolls_the_step_back_and_keeps_earlier_ones() {
    let sb = two_graduated_rolls();
    // Passes for alpha, fails once beta's file is in the tree — so step 1
    // commits and step 2 must not.
    sb.set_promote_gates(&["test ! -f beta.txt"]);

    let out = sb.rf(&[
        "promote",
        "--roll",
        "roll/1-0611-alpha",
        "--roll",
        "roll/2-0612-beta",
    ]);
    assert!(!out.success, "second step should fail: {}", out.combined());

    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ promoted"),
        "the step that passed its gates must stay committed"
    );
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ graduated"),
        "the step that failed its gates must not have landed"
    );

    let (merge_in_progress, _, _) = sb.git_try(&["rev-parse", "-q", "--verify", "MERGE_HEAD"]);
    assert!(
        !merge_in_progress,
        "MERGE_HEAD should not exist after the rollback"
    );
    assert_eq!(
        sb.current_branch(),
        "rolling",
        "the original checkout should be restored"
    );
}

#[test]
fn a_gate_that_dirties_the_tree_aborts_rather_than_committing_unverified_content() {
    let sb = two_graduated_rolls();
    // Rewrites a tracked file mid-merge. `git commit` would drop the change,
    // producing a merge whose content the gate never validated.
    sb.set_promote_gates(&["echo tampered > alpha.txt"]);

    let out = sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]);
    assert!(
        !out.success,
        "a tree-mutating gate should abort: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("alpha.txt"),
        "the error should name the modified file: {}",
        out.combined()
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ graduated"),
        "nothing should have been promoted"
    );
}

#[test]
fn an_already_promoted_roll_is_skipped_not_an_error() {
    let sb = two_graduated_rolls();
    assert!(
        sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]).success,
        "promote alpha"
    );

    let out = sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]);
    assert!(
        out.success,
        "re-promoting a contained roll should be a no-op, not a failure: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("skipped"),
        "the skip should be reported: {}",
        out.combined()
    );
}

#[test]
fn an_ungraduated_roll_cannot_be_promoted() {
    let sb = two_graduated_rolls();
    sb.git(&["checkout", "main"]);
    assert!(sb.create_roll("gamma", "0613").success, "create gamma");
    sb.commit_file("gamma.txt", "g\n", "gamma work");
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--roll", "roll/3-0613-gamma"]);
    assert!(!out.success, "should refuse: {}", out.combined());
    assert!(
        out.combined().contains("has not graduated"),
        "the error should explain why: {}",
        out.combined()
    );
}

#[test]
fn an_unknown_roll_is_named_in_the_error() {
    let sb = two_graduated_rolls();
    let out = sb.rf(&["promote", "--roll", "roll/9-0101-nope"]);
    assert!(!out.success, "should refuse: {}", out.combined());
    assert!(
        out.combined().contains("no such roll"),
        "the error should say the roll is unknown: {}",
        out.combined()
    );
}

#[test]
fn promoting_a_roll_works_from_a_roll_branch_without_redirecting_to_graduate() {
    // Bare `rf promote` on a roll branch redirects to graduation. `--roll` names
    // what to do outright, so it must not be hijacked by that inference.
    let sb = two_graduated_rolls();
    sb.git(&["checkout", "main"]);
    assert!(sb.create_roll("gamma", "0613").success, "create gamma");
    sb.commit_file("gamma.txt", "g\n", "gamma work");

    let out = sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]);
    assert!(
        out.success,
        "--roll should work from a roll branch: {}",
        out.combined()
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ promoted")
    );
    assert_eq!(
        sb.roll_state("roll/3-0613-gamma").as_deref(),
        Some("active"),
        "gamma must not have been graduated by the redirect"
    );
}

#[test]
fn dry_run_promotes_nothing() {
    let sb = two_graduated_rolls();

    let out = sb.rf(&["promote", "--roll", "roll/1-0611-alpha", "--dry-run"]);
    assert!(out.success, "dry-run: {}", out.combined());
    assert!(
        out.combined().contains("Dry-run"),
        "dry-run should say so: {}",
        out.combined()
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ graduated"),
        "dry-run must not promote"
    );
}

#[test]
fn promotion_merges_the_graduation_on_rolling_not_an_integration_merge_elsewhere() {
    // Real-history regression. Once alpha has graduated, a later roll can still
    // `rf integrate` it — leaving two merges reachable from rolling that name
    // alpha: its own `Graduate roll/1 into rolling` on rolling's mainline, and
    // beta's newer `Merge branch 'roll/1-…'` buried inside beta's branch.
    //
    // Scanning by reachability alone picks whichever is newest, which is the
    // integration merge — so `--roll alpha` advanced stable to a point inside
    // beta's history instead of to where alpha actually landed on rolling.
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");

    // Beta integrates alpha *after* alpha graduated, so the integration merge is
    // the newer of the two commits naming it.
    sb.git(&["checkout", "main"]);
    assert!(sb.create_roll("beta", "0612").success, "create beta");
    sb.commit_file("beta.txt", "b\n", "beta work");
    // Dated explicitly: everything else in the sandbox lands within one second,
    // and without a strictly later date this merge does not actually sort ahead
    // of alpha's graduation — the test would pass whatever the scan picked.
    let later = [
        ("GIT_AUTHOR_DATE", "2030-01-01T00:00:00+00:00"),
        ("GIT_COMMITTER_DATE", "2030-01-01T00:00:00+00:00"),
    ];
    assert!(
        sb.rf_with_env(&["integrate", "roll/1-0611-alpha"], &later)
            .success,
        "integrate alpha into beta"
    );
    assert!(
        sb.rf_with_env(&["graduate"], &later).success,
        "graduate beta"
    );
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--roll", "roll/1-0611-alpha"]);
    assert!(out.success, "promote alpha: {}", out.combined());

    // The invariant, stated without depending on git's log ordering: whatever
    // was merged must sit on rolling's own first-parent line. The integration
    // merge does not.
    let merged = sb.git(&["rev-parse", "main^2"]);
    let merged = merged.trim();
    let mainline = sb.git(&["rev-list", "--first-parent", "rolling"]);
    assert!(
        mainline.lines().any(|c| c == merged),
        "promoted source {merged} should be a commit on rolling's mainline:\n{mainline}"
    );

    let graduation = sb.git(&[
        "rev-list",
        "-1",
        "--first-parent",
        "--grep=Graduate roll/1-0611-alpha",
        "rolling",
    ]);
    assert_eq!(
        merged,
        graduation.trim(),
        "must merge alpha's graduation on rolling, not beta's integration merge"
    );
    // Beta integrated alpha, so promoting alpha must not drag beta across.
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ graduated")
    );
}
