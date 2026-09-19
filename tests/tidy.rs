//! `rf tidy` — clearing local roll branches whose commits survive elsewhere.
//!
//! Tidy is the one deletion command that judges safety by *recoverability*
//! rather than by landing: it only ever deletes the local copy, so a branch
//! still on `origin` is one `git fetch` away. Everything worth testing follows
//! from that bargain — it must never touch the remote, and the fetch that keeps
//! `origin/<branch>` honest must actually happen, or the rule certifies as
//! recoverable exactly the branches whose only copy is local.

mod harness;

use harness::Sandbox;

/// Create a roll, commit on it, graduate it, and leave HEAD on `main`.
fn graduate_roll(sb: &Sandbox, slug: &str, date: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();

    sb.commit_file(&format!("{slug}.txt"), "work\n", &format!("{slug} work"));

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    sb.git(&["checkout", "main"]);
    branch
}

/// Drive one roll all the way to promoted. Leaves HEAD on `main`.
fn promote_roll(sb: &Sandbox, slug: &str, date: &str) -> String {
    let branch = graduate_roll(sb, slug, date);

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote failed: {}", out.combined());

    sb.git(&["checkout", "main"]);
    branch
}

/// Create a roll, commit on it, and leave it ungraduated. HEAD ends on `main`.
fn active_roll(sb: &Sandbox, slug: &str, date: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();
    sb.commit_file(&format!("{slug}.txt"), "wip\n", &format!("{slug} wip"));
    sb.git(&["checkout", "main"]);
    branch
}

#[test]
fn clears_graduated_and_promoted_rolls_and_leaves_active_ones_alone() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    let graduated = graduate_roll(&sb, "merged", "0612");
    let active = active_roll(&sb, "wip", "0613");
    for branch in [&promoted, &graduated, &active] {
        sb.push_branch(branch);
    }

    let out = sb.rf(&["tidy", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());

    assert!(
        !sb.branch_exists(&promoted),
        "promoted roll should be tidied: {}",
        out.combined()
    );
    assert!(
        !sb.branch_exists(&graduated),
        "graduated roll is on rolling, so its local copy is clutter: {}",
        out.combined()
    );
    assert!(
        sb.branch_exists(&active),
        "an active roll is outside the default states: {}",
        out.combined()
    );
}

#[test]
fn never_deletes_anything_on_origin() {
    // The whole safety argument is that origin still has it. If tidy could
    // delete the remote copy, deleting the local one would stop being cheap.
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    let graduated = graduate_roll(&sb, "merged", "0612");
    sb.push_branch(&promoted);
    sb.push_branch(&graduated);

    let out = sb.rf(&["tidy", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());

    for branch in [&promoted, &graduated] {
        assert!(!sb.branch_exists(branch), "local '{branch}' should be gone");
        assert!(
            sb.remote_branch_exists(branch),
            "origin '{branch}' must survive tidy: {}",
            out.combined()
        );
    }
}

#[test]
fn an_active_roll_goes_only_when_it_is_fully_pushed() {
    let sb = Sandbox::with_origin();
    sb.init();

    let pushed = active_roll(&sb, "shared", "0611");
    sb.push_branch(&pushed);
    let local_only = active_roll(&sb, "private", "0612");

    let out = sb.rf(&["tidy", "--state", "active", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());

    assert!(
        !sb.branch_exists(&pushed),
        "a fully pushed branch is refetchable: {}",
        out.combined()
    );
    assert!(
        sb.remote_branch_exists(&pushed),
        "...which is only true because origin still has it"
    );
    assert!(
        sb.branch_exists(&local_only),
        "an unpushed branch is the only copy of its commits: {}",
        out.combined()
    );
    assert!(
        out.combined().contains(&local_only) && out.combined().contains("--force"),
        "the refusal should name the branch and the override: {}",
        out.combined()
    );
}

#[test]
fn unpushed_commits_on_a_pushed_branch_still_block_it() {
    // The check is containment, not "has an upstream": a branch pushed once and
    // then committed to again holds commits that exist nowhere else.
    let sb = Sandbox::with_origin();
    sb.init();

    let branch = active_roll(&sb, "shared", "0611");
    sb.push_branch(&branch);

    sb.git(&["checkout", &branch]);
    sb.commit_file("later.txt", "unpushed\n", "work not yet pushed");
    sb.git(&["checkout", "main"]);

    let out = sb.rf(&["tidy", "--state", "active", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(
        sb.branch_exists(&branch),
        "the tip is ahead of origin, so it is not recoverable: {}",
        out.combined()
    );

    let out = sb.rf(&["tidy", "--state", "active", "--force", "--yes"]);
    assert!(out.success, "forced tidy failed: {}", out.combined());
    assert!(
        !sb.branch_exists(&branch),
        "--force is the documented override: {}",
        out.combined()
    );
}

#[test]
fn a_stale_remote_tracking_ref_does_not_certify_a_branch_as_recoverable() {
    // The load-bearing case for the pruning fetch. Another host deleted the
    // branch on origin; our cached `origin/<branch>` still names a tip. Without
    // the fetch, tidy would read that cache as proof the commits survive and
    // delete the only copy left.
    let sb = Sandbox::with_origin();
    sb.init();

    let branch = active_roll(&sb, "shared", "0611");
    sb.push_branch(&branch);
    sb.delete_on_origin(&branch);
    assert!(
        sb.tracking_ref_exists("origin", &branch),
        "the stale ref is the precondition this test reproduces"
    );

    let out = sb.rf(&["tidy", "--state", "active", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(
        sb.branch_exists(&branch),
        "the fetch should have pruned the ref that vouched for it: {}",
        out.combined()
    );
}

#[test]
fn keeps_the_checked_out_branch_and_says_why() {
    let sb = Sandbox::with_origin();
    sb.init();

    let graduated = graduate_roll(&sb, "merged", "0611");
    sb.push_branch(&graduated);
    sb.git(&["checkout", &graduated]);

    let out = sb.rf(&["tidy", "--force", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(
        sb.branch_exists(&graduated),
        "--force must not reach past the checked-out guard: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("checked out"),
        "the refusal should be stated: {}",
        out.combined()
    );
}

#[test]
fn keeps_a_branch_checked_out_in_another_worktree_and_names_the_path() {
    let sb = Sandbox::with_origin();
    sb.init();

    let graduated = graduate_roll(&sb, "merged", "0611");
    sb.push_branch(&graduated);
    let worktree = sb.add_worktree(&graduated);

    let out = sb.rf(&["tidy", "--force", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(
        sb.branch_exists(&graduated),
        "git refuses this delete anyway; --force must not attempt it: {}",
        out.combined()
    );
    assert!(
        out.combined().contains(&worktree),
        "the worktree path is the actionable part of the refusal: {}",
        out.combined()
    );
}

#[test]
fn state_selection_widens_and_narrows_what_is_considered() {
    let sb = Sandbox::with_origin();
    sb.init();

    let promoted = promote_roll(&sb, "done", "0611");
    let graduated = graduate_roll(&sb, "merged", "0612");
    let active = active_roll(&sb, "wip", "0613");
    for branch in [&promoted, &graduated, &active] {
        sb.push_branch(branch);
    }

    // Narrowed to promoted: the same set `rf prune` would consider.
    let out = sb.rf(&["tidy", "--state", "promoted", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(!sb.branch_exists(&promoted));
    assert!(sb.branch_exists(&graduated), "out of the selected states");
    assert!(sb.branch_exists(&active), "out of the selected states");

    // Widened to everything the gate allows.
    let out = sb.rf(&["tidy", "--state", "all", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(!sb.branch_exists(&graduated));
    assert!(
        !sb.branch_exists(&active),
        "an active roll that is fully pushed is still recoverable: {}",
        out.combined()
    );
}

#[test]
fn dry_run_deletes_nothing() {
    let sb = Sandbox::with_origin();
    sb.init();

    let graduated = graduate_roll(&sb, "merged", "0611");
    sb.push_branch(&graduated);

    let out = sb.rf(&["tidy", "--dry-run"]);
    assert!(out.success, "tidy --dry-run failed: {}", out.combined());
    assert!(
        out.combined().contains(&graduated),
        "dry-run should list the candidate: {}",
        out.combined()
    );
    assert!(sb.branch_exists(&graduated), "dry-run must delete nothing");
}

#[test]
fn non_interactive_without_yes_deletes_nothing() {
    let sb = Sandbox::with_origin();
    sb.init();

    let graduated = graduate_roll(&sb, "merged", "0611");
    sb.push_branch(&graduated);

    let out = sb.rf(&["tidy"]);
    assert!(
        out.success,
        "an unattended run reports and exits 0: {}",
        out.combined()
    );
    assert!(
        sb.branch_exists(&graduated),
        "nothing may be deleted without a confirmation: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("--yes"),
        "it should say how to proceed: {}",
        out.combined()
    );
}

#[test]
fn works_in_a_repo_with_no_origin() {
    let sb = Sandbox::plain();
    sb.init();

    let graduated = graduate_roll(&sb, "merged", "0611");

    let out = sb.rf(&["tidy", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(
        !sb.branch_exists(&graduated),
        "rolling containment stands on its own without a remote: {}",
        out.combined()
    );
}

#[test]
fn reports_when_there_is_nothing_to_tidy() {
    let sb = Sandbox::with_origin();
    sb.init();

    active_roll(&sb, "wip", "0611");

    let out = sb.rf(&["tidy", "--yes"]);
    assert!(out.success, "tidy failed: {}", out.combined());
    assert!(
        out.combined().contains("no local roll branches to tidy"),
        "an empty plan should say so: {}",
        out.combined()
    );
}
