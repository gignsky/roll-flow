//! `rf verify --all`: verifying many rolls in one pass.
//!
//! The thing worth testing here is not the verdicts — `ops::verify` is covered
//! elsewhere — but the discipline around them: the pass must check out each
//! roll in turn and **always** land back on the branch it started from, whatever
//! any single roll's gates said.

mod harness;

use harness::Sandbox;

/// Create a roll with one commit on it, then return to `main`.
fn roll_with_work(sb: &Sandbox, slug: &str, date: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();
    sb.commit_file(&format!("{slug}.txt"), "work\n", &format!("{slug} work"));
    sb.git(&["checkout", "main"]);
    branch
}

#[test]
fn verify_all_reports_every_roll_and_returns_to_the_starting_branch() {
    let sb = Sandbox::plain();
    sb.init();
    let alpha = roll_with_work(&sb, "alpha", "0611");
    let beta = roll_with_work(&sb, "beta", "0612");
    assert_eq!(sb.current_branch(), "main");

    let out = sb.rf(&["verify", "--all"]);
    assert!(out.success, "{}", out.combined());
    let text = out.combined();
    assert!(text.contains(&format!("── {alpha} ──")), "{text}");
    assert!(text.contains(&format!("── {beta} ──")), "{text}");
    assert!(text.contains("2 passed, 0 failed, 0 skipped"), "{text}");
    assert_eq!(sb.current_branch(), "main", "HEAD did not come back");
}

#[test]
fn a_failing_roll_is_named_and_head_still_comes_back() {
    let sb = Sandbox::plain();
    sb.init();
    // A gate that fails only on the second roll, keyed on the file it adds.
    sb.set_graduate_gates(&["test ! -e beta.txt"]);
    sb.git(&["add", ".roll-flow.toml"]);
    sb.git(&["commit", "-m", "gate"]);
    let alpha = roll_with_work(&sb, "alpha", "0611");
    let beta = roll_with_work(&sb, "beta", "0612");
    // Start from the *first* roll rather than main, so "back where it started"
    // is not satisfied by accident.
    sb.git(&["checkout", &alpha]);

    let out = sb.rf(&["verify", "--all"]);
    assert!(
        !out.success,
        "a failed roll must fail the pass: {}",
        out.combined()
    );
    let text = out.combined();
    assert!(text.contains("1 passed, 1 failed, 0 skipped"), "{text}");
    assert!(
        text.contains(&format!("verification failed for: {beta}")),
        "{text}"
    );
    assert_eq!(sb.current_branch(), alpha, "HEAD did not come back");
}

#[test]
fn a_state_filter_narrows_the_pass_and_a_dirty_tree_is_refused_first() {
    let sb = Sandbox::plain();
    sb.init();
    roll_with_work(&sb, "alpha", "0611");

    // Nothing is graduated yet, so the set is empty and nothing runs.
    let out = sb.rf(&["verify", "--all", "--state", "graduated"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined().contains("no rolls to verify"),
        "{}",
        out.combined()
    );

    // A dirty tree is refused before any switch, with HEAD untouched.
    sb.write("scratch.txt", "unsaved\n");
    let out = sb.rf(&["verify", "--all"]);
    assert!(!out.success, "{}", out.combined());
    assert!(out.combined().contains("clean"), "{}", out.combined());
    assert_eq!(sb.current_branch(), "main");
}

#[test]
fn verify_all_marks_each_roll_that_lacks_its_dev_marker() {
    // The same marker `rf verify` applies to one roll, applied to every roll in
    // the pass — so the batch and the single verify never disagree about what a
    // verified roll's version looks like.
    let sb = Sandbox::cargo();
    sb.init();
    let mut rolls = Vec::new();
    for (slug, date) in [("alpha", "0611"), ("beta", "0612")] {
        let out = sb.rf(&["create", slug, "--date", date, "--no-dev-version"]);
        assert!(out.success, "{}", out.combined());
        rolls.push(sb.current_branch());
        sb.commit_file(&format!("{slug}.txt"), "work\n", &format!("{slug} work"));
        sb.git(&["checkout", "main"]);
    }
    assert_eq!(sb.cargo_version_at(&rolls[0]), "0.0.1");

    let out = sb.rf(&["verify", "--all"]);
    assert!(out.success, "{}", out.combined());
    let text = out.combined();
    assert!(text.contains("version marked 0.0.1-roll1"), "{text}");
    assert!(text.contains("version marked 0.0.1-roll2"), "{text}");
    assert_eq!(sb.cargo_version_at(&rolls[0]), "0.0.1-roll1");
    assert_eq!(sb.cargo_version_at(&rolls[1]), "0.0.1-roll2");
    assert_eq!(sb.current_branch(), "main", "HEAD did not come back");

    // Idempotent: a second pass has nothing left to mark.
    let tips: Vec<String> = rolls.iter().map(|r| sb.rev(r)).collect();
    let out = sb.rf(&["verify", "--all"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        !out.combined().contains("version marked"),
        "{}",
        out.combined()
    );
    assert_eq!(rolls.iter().map(|r| sb.rev(r)).collect::<Vec<_>>(), tips);
}

#[test]
fn a_roll_whose_version_is_behind_stable_fails_before_its_gates() {
    // A roll still on 0.0.1 after stable moved to 0.0.2 has missed stable's
    // release: graduating it as-is would carry a version stable already left
    // behind. It fails with the remedy, and its gates never run — a version
    // problem should cost milliseconds, not a full gate run.
    let sb = Sandbox::cargo();
    sb.init();
    sb.set_graduate_gates(&["echo GATE-RAN-ON-$(git branch --show-current)"]);
    sb.git(&["add", ".roll-flow.toml"]);
    sb.git(&["commit", "-m", "gate"]);
    let stale = roll_with_work(&sb, "stale", "0611");
    sb.commit_cargo_version("0.0.2");
    let fresh = roll_with_work(&sb, "fresh", "0612");

    let out = sb.rf(&["verify", "--all"]);
    assert!(
        !out.success,
        "a stale version must fail the pass: {}",
        out.combined()
    );
    let text = out.combined();
    assert!(text.contains("1 passed, 1 failed, 0 skipped"), "{text}");
    assert!(
        text.contains(&format!("verification failed for: {stale}")),
        "{text}"
    );
    assert!(text.contains("behind 'main' (0.0.2)"), "{text}");
    assert!(text.contains("rf update"), "{text}");
    assert!(!text.contains(&format!("GATE-RAN-ON-{stale}")), "{text}");
    assert!(text.contains(&format!("GATE-RAN-ON-{fresh}")), "{text}");
    assert_eq!(sb.current_branch(), "main", "HEAD did not come back");
}
