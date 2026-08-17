//! `rf prune` — deleting roll branches that have already been promoted to the
//! stable branch, locally and on origin.
//!
//! Prune is the only command that mutates the remote, and the only destructive
//! one, so these tests lean on the safety rules rather than the happy path:
//! promoted-only, contained-in-stable, never the checked-out branch, and never
//! anything at all when run unattended without `--yes`.

mod harness;

use harness::Sandbox;

/// Drive one roll all the way to promoted: create → work → graduate → promote.
/// Returns the roll's branch name and leaves HEAD on `main`.
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
fn prunes_promoted_roll_from_local_and_origin() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    sb.push_branch(&promoted);
    let active = active_roll(&sb, "wip", "0612");
    sb.push_branch(&active);

    assert!(sb.remote_branch_exists(&promoted));

    let out = sb.rf(&["prune", "--yes"]);
    assert!(out.success, "prune failed: {}", out.combined());

    // The promoted roll is gone from both sides...
    assert!(
        !sb.branch_exists(&promoted),
        "local '{promoted}' should be deleted: {}",
        out.combined()
    );
    assert!(
        !sb.remote_branch_exists(&promoted),
        "origin '{promoted}' should be deleted: {}",
        out.combined()
    );

    // ...and the still-active roll is untouched on both.
    assert!(sb.branch_exists(&active), "active roll must survive prune");
    assert!(
        sb.remote_branch_exists(&active),
        "active roll must survive prune on origin"
    );
}

#[test]
fn dry_run_deletes_nothing() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    sb.push_branch(&promoted);

    let out = sb.rf(&["prune", "--dry-run"]);
    assert!(out.success, "prune --dry-run failed: {}", out.combined());
    assert!(
        out.combined().contains(&promoted),
        "dry-run should list the candidate: {}",
        out.combined()
    );
    assert!(sb.branch_exists(&promoted), "dry-run must not delete local");
    assert!(
        sb.remote_branch_exists(&promoted),
        "dry-run must not delete on origin"
    );
}

#[test]
fn keeps_local_copy_of_checked_out_branch_but_prunes_origin() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    sb.push_branch(&promoted);

    // Stand on the very branch being pruned.
    sb.git(&["checkout", &promoted]);

    let out = sb.rf(&["prune", "--yes"]);
    assert!(out.success, "prune failed: {}", out.combined());
    assert!(
        out.combined().contains("checked out"),
        "should say why the local copy was kept: {}",
        out.combined()
    );
    assert!(
        sb.branch_exists(&promoted),
        "must not delete the checked-out branch"
    );
    assert!(
        !sb.remote_branch_exists(&promoted),
        "origin copy is still safe to delete"
    );
}

#[test]
fn local_and_remote_flags_narrow_the_scope() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    sb.push_branch(&promoted);

    let out = sb.rf(&["prune", "--local", "--yes"]);
    assert!(out.success, "prune --local failed: {}", out.combined());
    assert!(!sb.branch_exists(&promoted), "--local should delete local");
    assert!(
        sb.remote_branch_exists(&promoted),
        "--local must leave origin alone"
    );

    let out = sb.rf(&["prune", "--remote", "--yes"]);
    assert!(out.success, "prune --remote failed: {}", out.combined());
    assert!(
        !sb.remote_branch_exists(&promoted),
        "--remote should delete on origin"
    );
}

#[test]
fn skips_promoted_branch_holding_commits_not_in_stable() {
    let sb = Sandbox::plain();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");

    // A commit added after promotion: the roll still *reports* promoted (that is
    // read off stable's commit subjects), but its tip is no longer contained in
    // main, so deleting it would lose work.
    sb.git(&["checkout", &promoted]);
    sb.commit_file("late.txt", "after promotion\n", "late work");
    sb.git(&["checkout", "main"]);

    let out = sb.rf(&["prune", "--yes"]);
    assert!(out.success, "prune failed: {}", out.combined());
    assert!(
        sb.branch_exists(&promoted),
        "branch with unmerged commits must survive: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("--force"),
        "skip reason should point at --force: {}",
        out.combined()
    );

    // --force is the documented override.
    let out = sb.rf(&["prune", "--yes", "--force"]);
    assert!(out.success, "prune --force failed: {}", out.combined());
    assert!(
        !sb.branch_exists(&promoted),
        "--force should delete it anyway: {}",
        out.combined()
    );
}

#[test]
fn works_in_a_repo_with_no_origin() {
    let sb = Sandbox::plain();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");

    let out = sb.rf(&["prune", "--yes"]);
    assert!(
        out.success,
        "prune without a remote should succeed: {}",
        out.combined()
    );
    assert!(!sb.branch_exists(&promoted), "local copy should be deleted");
    assert!(
        out.combined().contains("no 'origin' remote"),
        "should note the absent remote: {}",
        out.combined()
    );
}

#[test]
fn non_interactive_without_yes_deletes_nothing() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    sb.push_branch(&promoted);

    // Tests run with stdin not a terminal — the unattended path. Exits 0 and
    // changes nothing rather than deleting on an unanswered prompt.
    let out = sb.rf(&["prune"]);
    assert!(out.success, "prune should exit 0: {}", out.combined());
    assert!(
        out.combined().contains("--yes"),
        "should say how to apply: {}",
        out.combined()
    );
    assert!(sb.branch_exists(&promoted), "nothing should be deleted");
    assert!(
        sb.remote_branch_exists(&promoted),
        "nothing should be deleted on origin"
    );
}

#[test]
fn reports_nothing_to_prune_when_no_rolls_are_promoted() {
    let sb = Sandbox::plain();
    sb.init();
    active_roll(&sb, "wip", "0611");

    let out = sb.rf(&["prune", "--yes"]);
    assert!(out.success, "prune failed: {}", out.combined());
    assert!(
        out.combined().contains("no promoted roll branches"),
        "unexpected output: {}",
        out.combined()
    );
}
