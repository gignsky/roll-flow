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

// ── per-roll promotion and the version gate ─────────────────────────────────

/// A cargo repo with one roll graduated and no bump anywhere, which is the
/// shape the version gate used to dead-end on: UNCHANGED against stable, with
/// `--bump` ignored for `--roll`.
fn one_graduated_cargo_roll() -> (Sandbox, String) {
    let sb = Sandbox::cargo();
    sb.init();
    let out = sb.create_roll("alpha", "0611");
    assert!(out.success, "create alpha: {}", out.combined());
    let alpha = sb.current_branch();
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");
    sb.git(&["checkout", "rolling"]);
    (sb, alpha)
}

#[test]
fn a_per_roll_promotion_bumps_inside_its_own_merge() {
    let (sb, alpha) = one_graduated_cargo_roll();
    assert_eq!(sb.cargo_version_at("main"), "0.0.1");

    let out = sb.rf(&["promote", "--roll", &alpha, "--bump", "minor", "--yes"]);
    assert!(out.success, "promote --roll failed: {}", out.combined());
    assert!(
        out.combined()
            .contains("Bumped version 0.0.1 -> 0.1.0 inside the promotion merge"),
        "{}",
        out.combined()
    );

    // The bump is in the merge commit, not a commit beside it: main's tip is a
    // merge, and stable still only ever receives merges.
    assert_eq!(sb.cargo_version_at("main"), "0.1.0");
    let parents = sb.git(&["rev-list", "--parents", "-1", "main"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "main's tip is not a merge: {parents}"
    );
    assert!(sb.tag_exists("v0.1.0"), "tag missing: {}", sb.git(&["tag"]));

    // Rolling would otherwise sit *below* stable and fail the next promotion as
    // LOWER, so stable was merged back into it.
    assert!(
        out.combined()
            .contains("Reintegrated 'main' into 'rolling'"),
        "{}",
        out.combined()
    );
    assert_eq!(sb.cargo_version_at("rolling"), "0.1.0");
    assert!(
        sb.is_ancestor("main", "rolling"),
        "main not reintegrated into rolling"
    );
}

#[test]
fn yes_takes_the_patch_default_for_a_per_roll_promotion() {
    let (sb, alpha) = one_graduated_cargo_roll();
    let out = sb.rf(&["promote", "--roll", &alpha, "--yes"]);
    assert!(out.success, "{}", out.combined());
    assert_eq!(sb.cargo_version_at("main"), "0.0.2");
}

#[test]
fn an_unattended_per_roll_promotion_names_the_roll_not_a_sha() {
    // No --bump, no --yes, no terminal: refused — but the refusal must name
    // the roll and a fix that actually works.
    let (sb, alpha) = one_graduated_cargo_roll();
    let out = sb.rf(&["promote", "--roll", &alpha]);
    assert!(!out.success, "should refuse: {}", out.combined());
    assert!(
        out.combined().contains(&alpha),
        "roll not named:\n{}",
        out.combined()
    );
    assert!(out.combined().contains("--bump"), "{}", out.combined());
    assert_eq!(
        sb.cargo_version_at("main"),
        "0.0.1",
        "nothing should have moved"
    );
}

#[test]
fn a_graduation_that_already_bumped_is_not_bumped_again() {
    let sb = Sandbox::cargo();
    sb.init();
    let out = sb.create_roll("alpha", "0611");
    assert!(out.success, "{}", out.combined());
    let alpha = sb.current_branch();
    sb.write_cargo_version("0.5.0");
    sb.git(&["add", "Cargo.toml"]);
    sb.git(&["commit", "-m", "bump on the roll"]);
    assert!(sb.rf(&["graduate"]).success);
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--roll", &alpha, "--bump", "major", "--yes"]);
    assert!(out.success, "{}", out.combined());
    // The gate was already satisfied, so the level in hand is not applied.
    assert_eq!(sb.cargo_version_at("main"), "0.5.0");
    assert!(
        !out.combined().contains("inside the promotion merge"),
        "{}",
        out.combined()
    );
}

#[test]
fn after_a_per_roll_promotion_yes_updates_the_active_rolls() {
    let (sb, alpha) = one_graduated_cargo_roll();
    // An active roll that will trail what lands on main.
    let out = sb.create_roll("beta", "0612");
    assert!(out.success, "{}", out.combined());
    let beta = sb.current_branch();
    sb.commit_file("beta.txt", "b\n", "beta work");
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--roll", &alpha, "--yes"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined()
            .contains(&format!("updated '{beta}' with 'main'")),
        "{}",
        out.combined()
    );
    assert!(
        sb.is_ancestor("main", &beta),
        "beta was not updated from main"
    );
}

// ── disclosing the rolls a step carries ─────────────────────────────────────
//
// Advancing stable to a roll's graduation commit lands everything that
// graduated ahead of it. That is inherent to the route, so the fix is
// disclosure: say which rolls come along, and get an answer before merging.

#[test]
fn promoting_a_later_roll_alone_refuses_unattended_and_names_what_it_would_carry() {
    let sb = two_graduated_rolls();

    let out = sb.rf(&["promote", "--roll", "roll/2-0612-beta"]);
    assert!(
        !out.success,
        "landing alpha unasked must not happen silently: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("roll/1-0611-alpha"),
        "the carried roll should be named: {}",
        out.combined()
    );
    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ graduated"),
        "nothing should have been promoted"
    );
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ graduated")
    );
}

#[test]
fn yes_accepts_the_carried_rolls_and_reports_them() {
    let sb = two_graduated_rolls();

    let out = sb.rf(&["promote", "--roll", "roll/2-0612-beta", "--yes"]);
    assert!(out.success, "promote beta: {}", out.combined());
    assert!(
        out.combined().contains("also landed: roll/1-0611-alpha"),
        "the carried roll should be reported: {}",
        out.combined()
    );
    // Both are on main — which is the point of the disclosure, not a bug: the
    // merge source is beta's graduation, and alpha's is its ancestor.
    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ promoted")
    );
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ promoted")
    );
}

#[test]
fn a_dry_run_lists_the_carried_rolls_instead_of_asking() {
    let sb = two_graduated_rolls();

    let out = sb.rf(&["promote", "--roll", "roll/2-0612-beta", "--dry-run"]);
    assert!(
        out.success,
        "a dry-run previews rather than refusing: {}",
        out.combined()
    );
    assert!(
        out.combined()
            .contains("would also land: roll/1-0611-alpha"),
        "{}",
        out.combined()
    );
}

#[test]
fn naming_both_rolls_carries_neither_behind_the_users_back() {
    // Alpha is promoted by its own step, so beta's step must not report it as
    // something it dragged along — the baseline is the previous step's source,
    // not stable's tip when the command started.
    let sb = two_graduated_rolls();

    let out = sb.rf(&[
        "promote",
        "--roll",
        "roll/1-0611-alpha",
        "--roll",
        "roll/2-0612-beta",
    ]);
    assert!(out.success, "promote both: {}", out.combined());
    assert!(
        !out.combined().contains("also landed"),
        "nothing was carried unasked: {}",
        out.combined()
    );
}
