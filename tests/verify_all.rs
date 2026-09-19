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
