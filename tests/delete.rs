//! `rf delete <branch>` — removing one named roll branch from existence,
//! locally, on origin, or both.
//!
//! The CLI twin of the TUI's `[d]elete`, and the only scriptable way to reach
//! the shared deletion rules in `core::ops`. Like `tests/prune.rs` these lean on
//! the safety rules rather than the happy path — but delete deliberately relaxes
//! one of prune's: the roll need not be promoted, because the user named the
//! branch instead of the tool inferring it. Everything else still holds: never
//! the checked-out branch, never an uncontained copy without `--force`, never a
//! workflow branch, and never anything at all when run unattended without
//! `--yes`.

mod harness;

use harness::Sandbox;

/// Drive one roll all the way to promoted. Returns its branch, HEAD on `main`.
fn promote_roll(sb: &Sandbox, slug: &str, date: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();

    sb.commit_file(&format!("{slug}.txt"), "work\n", &format!("{slug} work"));

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote failed: {}", out.combined());

    sb.git(&["checkout", "main"]);
    branch
}

/// Create a roll and leave it active (graduated nowhere). Returns its branch.
fn active_roll(sb: &Sandbox, slug: &str, date: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();
    sb.commit_file(&format!("{slug}.txt"), "wip\n", &format!("{slug} wip"));
    sb.git(&["checkout", "main"]);
    branch
}

#[test]
fn deletes_a_promoted_roll_from_local_and_origin() {
    let sb = Sandbox::with_origin();
    sb.init();

    let branch = promote_roll(&sb, "done", "0611");
    sb.push_branch(&branch);
    assert!(sb.remote_branch_exists(&branch), "fixture should push");

    let out = sb.rf(&["delete", &branch, "--yes"]);
    assert!(out.success, "delete failed: {}", out.combined());
    assert!(!sb.branch_exists(&branch), "local copy should be gone");
    assert!(
        !sb.remote_branch_exists(&branch),
        "origin copy should be gone"
    );
}

#[test]
fn deletes_an_unpromoted_roll_the_user_named() {
    // The rule delete relaxes relative to prune: an abandoned roll is a
    // legitimate target because the user pointed at it. It still holds commits
    // stable lacks, so it needs `--force`.
    let sb = Sandbox::plain();
    sb.init();

    let branch = active_roll(&sb, "abandoned", "0611");

    let out = sb.rf(&["delete", &branch, "--yes", "--force"]);
    assert!(out.success, "delete failed: {}", out.combined());
    assert!(
        !sb.branch_exists(&branch),
        "an explicitly named active roll should be deletable"
    );
}

#[test]
fn local_and_remote_flags_narrow_the_scope() {
    let sb = Sandbox::with_origin();
    sb.init();

    let branch = promote_roll(&sb, "done", "0611");
    sb.push_branch(&branch);

    let out = sb.rf(&["delete", &branch, "--local", "--yes"]);
    assert!(out.success, "delete --local failed: {}", out.combined());
    assert!(!sb.branch_exists(&branch), "local copy should be gone");
    assert!(
        sb.remote_branch_exists(&branch),
        "--local must leave origin alone"
    );

    let out = sb.rf(&["delete", &branch, "--remote", "--yes"]);
    assert!(out.success, "delete --remote failed: {}", out.combined());
    assert!(
        !sb.remote_branch_exists(&branch),
        "--remote should delete the origin copy"
    );
}

#[test]
fn keeps_local_copy_of_checked_out_branch_but_deletes_origin() {
    let sb = Sandbox::with_origin();
    sb.init();

    let branch = promote_roll(&sb, "done", "0611");
    sb.push_branch(&branch);

    // Stand on the very branch being deleted.
    sb.git(&["checkout", &branch]);

    let out = sb.rf(&["delete", &branch, "--yes"]);
    assert!(out.success, "delete failed: {}", out.combined());
    assert!(
        out.combined().contains("checked out"),
        "should say why the local copy was kept: {}",
        out.combined()
    );
    assert!(
        sb.branch_exists(&branch),
        "must not delete the checked-out branch"
    );
    assert!(
        !sb.remote_branch_exists(&branch),
        "origin copy is still safe to delete"
    );
}

#[test]
fn force_never_deletes_the_checked_out_branch() {
    // `--force` widens *what* may be deleted, but the checked-out guard sits
    // ahead of it and is not an override target.
    let sb = Sandbox::plain();
    sb.init();

    let branch = active_roll(&sb, "wip", "0611");
    sb.git(&["checkout", &branch]);

    let out = sb.rf(&["delete", &branch, "--yes", "--force"]);
    assert!(out.success, "delete failed: {}", out.combined());
    assert!(
        sb.branch_exists(&branch),
        "--force must not defeat the checked-out guard"
    );
    assert!(
        out.combined().contains("checked out"),
        "should still explain itself: {}",
        out.combined()
    );
}

#[test]
fn refuses_a_branch_holding_commits_not_in_stable_without_force() {
    let sb = Sandbox::plain();
    sb.init();

    let branch = promote_roll(&sb, "done", "0611");

    // Add a commit after promotion — the tip is no longer contained in stable.
    sb.git(&["checkout", &branch]);
    sb.commit_file("late.txt", "late\n", "late work");
    sb.git(&["checkout", "main"]);

    let out = sb.rf(&["delete", &branch, "--yes"]);
    assert!(out.success, "delete failed: {}", out.combined());
    assert!(
        sb.branch_exists(&branch),
        "uncontained branch must survive without --force"
    );
    assert!(
        out.combined().contains("--force"),
        "should point at the override: {}",
        out.combined()
    );

    let out = sb.rf(&["delete", &branch, "--yes", "--force"]);
    assert!(out.success, "forced delete failed: {}", out.combined());
    assert!(!sb.branch_exists(&branch), "--force should delete it");
}

#[test]
fn refuses_the_stable_and_rolling_branches() {
    let sb = Sandbox::plain();
    sb.init();

    // Give `rolling` something to exist for.
    promote_roll(&sb, "done", "0611");

    for protected in ["main", "rolling"] {
        let out = sb.rf(&["delete", protected, "--yes", "--force"]);
        assert!(
            !out.success,
            "deleting '{protected}' should fail: {}",
            out.combined()
        );
        assert!(
            sb.branch_exists(protected),
            "'{protected}' must still exist"
        );
    }
}

#[test]
fn non_interactive_without_yes_deletes_nothing() {
    let sb = Sandbox::plain();
    sb.init();

    let branch = promote_roll(&sb, "done", "0611");

    let out = sb.rf(&["delete", &branch]);
    assert!(
        out.success,
        "unattended run should exit 0: {}",
        out.combined()
    );
    assert!(sb.branch_exists(&branch), "nothing should be deleted");
    assert!(
        out.combined().contains("--yes"),
        "should say how to opt in: {}",
        out.combined()
    );
}

#[test]
fn dry_run_deletes_nothing() {
    let sb = Sandbox::with_origin();
    sb.init();

    let branch = promote_roll(&sb, "done", "0611");
    sb.push_branch(&branch);

    let out = sb.rf(&["delete", &branch, "--dry-run", "--yes"]);
    assert!(out.success, "dry-run failed: {}", out.combined());
    assert!(sb.branch_exists(&branch), "local copy should survive");
    assert!(
        sb.remote_branch_exists(&branch),
        "origin copy should survive"
    );
}

#[test]
fn reports_a_branch_that_does_not_exist() {
    let sb = Sandbox::plain();
    sb.init();

    let out = sb.rf(&["delete", "roll/9-0611-ghost", "--yes"]);
    assert!(
        out.success,
        "a missing branch is reported, not an error: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("nothing to delete"),
        "should say there was nothing there: {}",
        out.combined()
    );
}
