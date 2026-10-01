//! `rf verify` pre-checks the graduation/promotion merge for conflicts.
//!
//! The trial merge runs in the object store (`git merge-tree --write-tree`), so
//! these also assert that a conflicted verify leaves no merge in progress and
//! the checkout where it was.

mod harness;

use harness::Sandbox;

/// Two rolls that edit the same line of `shared.txt`; the first is graduated,
/// and HEAD is left on the second — whose graduation would now conflict.
fn conflicting_rolls() -> Sandbox {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("first", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("shared.txt", "first\n", "first edit");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    sb.git(&["checkout", "main"]);
    let out = sb.create_roll("second", "0612");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("shared.txt", "second\n", "second edit");
    sb
}

fn assert_no_merge_in_progress(sb: &Sandbox) {
    let (merging, _, _) = sb.git_try(&["rev-parse", "-q", "--verify", "MERGE_HEAD"]);
    assert!(!merging, "verify must not leave a merge in progress");
}

#[test]
fn verify_fails_when_graduation_would_conflict() {
    let sb = conflicting_rolls();
    let before = sb.current_branch();

    let out = sb.rf(&["verify"]);
    assert!(!out.success, "verify should fail: {}", out.combined());
    let combined = out.combined();
    assert!(
        combined.contains("would conflict in 1 file"),
        "expected a conflict report: {combined}"
    );
    assert!(
        combined.contains("shared.txt"),
        "expected the path: {combined}"
    );
    assert!(
        combined.contains("git merge rolling"),
        "expected resolution advice: {combined}"
    );

    assert_eq!(sb.current_branch(), before);
    assert_no_merge_in_progress(&sb);
}

#[test]
fn verify_dry_run_warns_about_a_conflict_but_passes() {
    let sb = conflicting_rolls();

    let out = sb.rf(&["verify", "--dry-run"]);
    assert!(out.success, "dry-run should preview: {}", out.combined());
    let combined = out.combined();
    assert!(
        combined.contains("warning: merging") && combined.contains("shared.txt"),
        "expected a conflict warning: {combined}"
    );
    assert_no_merge_in_progress(&sb);
}

#[test]
fn verify_passes_once_the_conflict_is_resolved_on_the_roll() {
    let sb = conflicting_rolls();

    let (merged, _, _) = sb.git_try(&["merge", "--no-edit", "rolling"]);
    assert!(!merged, "the setup should conflict");
    sb.write("shared.txt", "both\n");
    sb.git(&["add", "shared.txt"]);
    sb.git(&["commit", "--no-edit"]);

    let out = sb.rf(&["verify"]);
    assert!(out.success, "verify should pass: {}", out.combined());
    assert!(
        !out.combined().contains("conflict"),
        "no conflict expected: {}",
        out.combined()
    );
}

#[test]
fn verify_fails_when_promotion_would_conflict() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());
    let out = sb.create_roll("work", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("shared.txt", "roll\n", "roll edit");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    // A change landed on stable that rolling never received (e.g. a hotfix
    // whose reintegration was skipped), touching the same file.
    sb.git(&["checkout", "main"]);
    sb.commit_file("shared.txt", "stable\n", "direct stable edit");
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["verify"]);
    assert!(!out.success, "verify should fail: {}", out.combined());
    let combined = out.combined();
    assert!(
        combined.contains("merging 'rolling' into 'main' would conflict")
            && combined.contains("shared.txt"),
        "expected a promotion conflict report: {combined}"
    );
    assert_eq!(sb.current_branch(), "rolling");
    assert_no_merge_in_progress(&sb);
}
