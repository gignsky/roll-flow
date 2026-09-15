//! `rf clean` — the repo-wide branch janitor.
//!
//! The motivating case has two halves that fail independently, so they are
//! asserted independently throughout: the *local* branch must go, and the
//! *stale remote-tracking ref* must go. Only the second is what tools like
//! lazygit read for their remote view, and a regression there is invisible to
//! any assertion that only checks `git branch`.

mod harness;

use harness::Sandbox;

// ── The motivating case ───────────────────────────────────────────────────────

/// Host A pushes `roll/101`; host B merges it and deletes it upstream. Back on
/// host A, both the local branch and the stale tracking ref must disappear.
#[test]
fn deletes_local_branch_whose_upstream_vanished() {
    let sb = Sandbox::with_origin();
    sb.init();

    sb.git(&["switch", "-c", "roll/101-0912-audio"]);
    sb.commit_empty("audio work");
    sb.git(&["push", "-u", "origin", "roll/101-0912-audio"]);
    sb.git(&["switch", "main"]);
    // Merge it locally so the containment gate is satisfied — this test is
    // about the gone-upstream detection, not the safety check.
    sb.git(&[
        "merge",
        "--no-ff",
        "-m",
        "merge roll",
        "roll/101-0912-audio",
    ]);

    sb.delete_on_origin("roll/101-0912-audio");

    // Precondition: the stale ref still claims the branch is live, which is
    // precisely why detection has to run *after* the prune.
    assert!(
        sb.tracking_ref_exists("origin", "roll/101-0912-audio"),
        "stale tracking ref should still be present before clean"
    );

    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());

    assert!(
        !sb.branch_exists("roll/101-0912-audio"),
        "local branch should be gone: {}",
        out.combined()
    );
    assert!(
        !sb.tracking_ref_exists("origin", "roll/101-0912-audio"),
        "stale remote-tracking ref should be pruned — this is the half lazygit \
         reads: {}",
        out.combined()
    );
}

#[test]
fn reports_the_pruned_refs_it_dropped() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.git(&["switch", "-c", "feature/old"]);
    sb.commit_empty("old work");
    sb.git(&["push", "-u", "origin", "feature/old"]);
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/old"]);
    sb.delete_on_origin("feature/old");

    let out = sb.rf(&["clean", "--dry-run"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        out.stdout.contains("origin/feature/old"),
        "should name the pruned ref: {}",
        out.stdout
    );
}

// ── Multi-remote ──────────────────────────────────────────────────────────────

#[test]
fn prunes_every_configured_remote_not_just_origin() {
    let sb = Sandbox::with_origin();
    sb.init();
    let other = sb.add_remote("upstream");

    sb.git(&["switch", "-c", "feature/a"]);
    sb.commit_empty("a");
    sb.git(&["push", "origin", "feature/a"]);
    sb.git(&["push", "upstream", "feature/a"]);
    sb.git(&["fetch", "--all"]);
    sb.git(&["switch", "main"]);

    sb.delete_on_origin("feature/a");
    sb.delete_on_remote(&other, "feature/a");

    assert!(sb.tracking_ref_exists("origin", "feature/a"));
    assert!(sb.tracking_ref_exists("upstream", "feature/a"));

    let out = sb.rf(&["clean", "--dry-run"]);
    assert!(out.success, "clean failed: {}", out.combined());

    // Both sides must be pruned. `rf prune` hardcodes origin; this is the
    // difference.
    assert!(
        !sb.tracking_ref_exists("origin", "feature/a"),
        "origin should be pruned: {}",
        out.combined()
    );
    assert!(
        !sb.tracking_ref_exists("upstream", "feature/a"),
        "upstream should be pruned too: {}",
        out.combined()
    );
}

#[test]
fn with_remote_deletes_on_the_branch_s_own_remote() {
    let sb = Sandbox::with_origin();
    sb.init();
    let other = sb.add_remote("upstream");

    sb.git(&["switch", "-c", "feature/b"]);
    sb.commit_empty("b");
    // Tracks `upstream`, not `origin`.
    sb.git(&["push", "-u", "upstream", "feature/b"]);
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/b"]);
    sb.git(&["push", "upstream", "main"]);

    let out = sb.rf(&["clean", "--with-remote", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());

    assert!(
        !sb.branch_exists_on(&other, "feature/b"),
        "should delete on the tracked remote, not assume origin: {}",
        out.combined()
    );
}

#[test]
fn leaves_the_remote_alone_without_with_remote() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.git(&["switch", "-c", "feature/c"]);
    sb.commit_empty("c");
    sb.git(&["push", "-u", "origin", "feature/c"]);
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/c"]);
    sb.git(&["push", "origin", "main"]);

    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());

    assert!(!sb.branch_exists("feature/c"), "local copy should go");
    assert!(
        sb.remote_branch_exists("feature/c"),
        "remote copy must survive without --with-remote: {}",
        out.combined()
    );
}

// ── Working without a config ──────────────────────────────────────────────────

#[test]
fn works_in_a_repo_with_no_roll_flow_config() {
    // Deliberately no `sb.init()` — this is a plain project that never heard of
    // roll-flow.
    let sb = Sandbox::plain();
    sb.git(&["switch", "-c", "feature/x"]);
    sb.commit_empty("x");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/x"]);

    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        out.stdout.contains("no .roll-flow.toml"),
        "should say promoted-roll cleanup was skipped: {}",
        out.stdout
    );
    assert!(
        !sb.branch_exists("feature/x"),
        "merged branch should still be cleaned without a config: {}",
        out.combined()
    );
}

#[test]
fn fails_cleanly_outside_a_git_repo() {
    let dir = tempfile::tempdir().expect("temp dir");
    let exe = std::env::var("CARGO_BIN_EXE_rf").expect("CARGO_BIN_EXE_rf");
    let out = std::process::Command::new(exe)
        .current_dir(dir.path())
        .arg("clean")
        .output()
        .expect("run rf");

    assert_eq!(out.status.code(), Some(1), "expected exit code 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not inside a git repository"),
        "got: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "errors must not go to stdout"
    );
}

// ── Merged-into-base ──────────────────────────────────────────────────────────

#[test]
fn deletes_merged_branches_and_keeps_unmerged_ones() {
    let sb = Sandbox::plain();
    sb.git(&["switch", "-c", "feature/merged"]);
    sb.commit_empty("merged work");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/merged"]);

    sb.git(&["switch", "-c", "feature/unmerged"]);
    sb.commit_empty("unmerged work");
    sb.git(&["switch", "main"]);

    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());

    assert!(!sb.branch_exists("feature/merged"), "merged should go");
    assert!(
        sb.branch_exists("feature/unmerged"),
        "unmerged must survive: {}",
        out.combined()
    );
}

// ── Safety ────────────────────────────────────────────────────────────────────

/// A deleted upstream is not proof the tip is contained anywhere. This is the
/// data-loss guard.
#[test]
fn unmerged_branch_with_a_gone_upstream_needs_force() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.git(&["switch", "-c", "feature/wip"]);
    sb.commit_empty("wip");
    sb.git(&["push", "-u", "origin", "feature/wip"]);
    // Local-only work on top of what was pushed.
    sb.commit_empty("more wip, never pushed");
    sb.git(&["switch", "main"]);
    sb.delete_on_origin("feature/wip");

    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        sb.branch_exists("feature/wip"),
        "unmerged work must not be deleted by default: {}",
        out.combined()
    );
    assert!(
        out.stdout.contains("--force"),
        "skip reason should point at --force: {}",
        out.stdout
    );

    let out = sb.rf(&["clean", "--force", "--yes"]);
    assert!(out.success, "forced clean failed: {}", out.combined());
    assert!(
        !sb.branch_exists("feature/wip"),
        "--force should delete it: {}",
        out.combined()
    );
}

#[test]
fn force_states_how_many_commits_would_be_lost() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.git(&["switch", "-c", "feature/wip"]);
    sb.commit_empty("wip");
    sb.git(&["push", "-u", "origin", "feature/wip"]);
    sb.commit_empty("lost one");
    sb.commit_empty("lost two");
    sb.git(&["switch", "main"]);
    sb.delete_on_origin("feature/wip");

    let out = sb.rf(&["clean", "--force", "--dry-run"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        out.stdout.contains("3 commits"),
        "should quantify the loss before the prompt: {}",
        out.stdout
    );
}

#[test]
fn never_deletes_protected_branches() {
    let sb = Sandbox::plain();
    sb.init();
    // `rf init` creates the rolling branch. Merge it into main so it would
    // otherwise qualify as merged-into-base, then assert protection wins.
    sb.git(&["switch", "rolling"]);
    sb.commit_empty("rolling work");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "rolling"]);

    let out = sb.rf(&["clean", "--force", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(sb.branch_exists("main"), "main must survive");
    assert!(
        sb.branch_exists("rolling"),
        "the rolling branch must survive: {}",
        out.combined()
    );
}

#[test]
fn honors_clean_protect_from_config() {
    let sb = Sandbox::plain();
    sb.init();
    sb.git(&["switch", "-c", "staging"]);
    sb.commit_empty("staging work");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "staging"]);

    sb.set_clean_protect(&["staging"]);

    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        sb.branch_exists("staging"),
        "clean_protect should shield it: {}",
        out.combined()
    );
}

#[test]
fn skips_a_branch_checked_out_in_another_worktree() {
    let sb = Sandbox::plain();
    sb.git(&["switch", "-c", "feature/busy"]);
    sb.commit_empty("busy work");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/busy"]);
    let wt = sb.add_worktree("feature/busy");

    let out = sb.rf(&["clean", "--yes"]);
    assert!(
        out.success,
        "clean should not fail over this: {}",
        out.combined()
    );
    assert!(sb.branch_exists("feature/busy"), "must survive");
    assert!(
        out.stdout.contains(&wt),
        "should name the worktree path: {}",
        out.stdout
    );

    // The guard is not a containment question, so --force must not override it.
    let out = sb.rf(&["clean", "--force", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        sb.branch_exists("feature/busy"),
        "--force must not delete a checked-out branch: {}",
        out.combined()
    );
}

#[test]
fn never_deletes_the_current_branch() {
    let sb = Sandbox::plain();
    sb.git(&["switch", "-c", "feature/here"]);
    sb.commit_empty("work");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/here"]);
    sb.git(&["switch", "feature/here"]);

    let out = sb.rf(&["clean", "--force", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        sb.branch_exists("feature/here"),
        "the checked-out branch must survive: {}",
        out.combined()
    );
}

// ── Dry-run and unattended ────────────────────────────────────────────────────

#[test]
fn dry_run_deletes_nothing_but_still_refreshes_tracking_refs() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.git(&["switch", "-c", "feature/d"]);
    sb.commit_empty("d");
    sb.git(&["push", "-u", "origin", "feature/d"]);
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/d"]);
    sb.delete_on_origin("feature/d");

    let out = sb.rf(&["clean", "--dry-run"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(out.stdout.contains("Dry-run"), "got: {}", out.stdout);
    assert!(
        sb.branch_exists("feature/d"),
        "branch must survive a dry run"
    );
    // The prune is what makes the dry run's report accurate in the first
    // place — without it nothing would ever report as gone.
    assert!(
        !sb.tracking_ref_exists("origin", "feature/d"),
        "dry-run still refreshes remote-tracking refs: {}",
        out.combined()
    );
}

#[test]
fn unattended_without_yes_deletes_nothing_and_exits_zero() {
    let sb = Sandbox::plain();
    sb.git(&["switch", "-c", "feature/e"]);
    sb.commit_empty("e");
    sb.git(&["switch", "main"]);
    sb.git(&["merge", "--no-ff", "-m", "merge", "feature/e"]);

    let out = sb.rf(&["clean"]);
    assert_eq!(out.code, Some(0), "must exit 0: {}", out.combined());
    assert!(sb.branch_exists("feature/e"), "nothing should be deleted");
    assert!(
        out.stdout.contains("--yes"),
        "should explain how to apply: {}",
        out.stdout
    );
}

#[test]
fn reports_nothing_to_clean_in_a_tidy_repo() {
    let sb = Sandbox::plain();
    let out = sb.rf(&["clean", "--dry-run"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        out.stdout.contains("nothing to clean"),
        "got: {}",
        out.stdout
    );
}

#[test]
fn no_fetch_leaves_stale_tracking_refs_alone() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.git(&["switch", "-c", "feature/f"]);
    sb.commit_empty("f");
    sb.git(&["push", "-u", "origin", "feature/f"]);
    sb.git(&["switch", "main"]);
    sb.delete_on_origin("feature/f");

    let out = sb.rf(&["clean", "--no-fetch", "--dry-run"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        sb.tracking_ref_exists("origin", "feature/f"),
        "--no-fetch must not prune: {}",
        out.combined()
    );
}

// ── Promoted rolls ────────────────────────────────────────────────────────────

#[test]
fn deletes_promoted_rolls_when_a_config_is_present() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("audio", "0912");
    let roll = sb.current_branch();
    sb.commit_empty("audio work");
    assert!(sb.rf(&["graduate"]).success, "graduate");
    sb.git(&["switch", "rolling"]);
    assert!(sb.rf(&["promote"]).success, "promote");
    sb.git(&["switch", "main"]);

    assert_eq!(
        sb.roll_state(&roll).as_deref(),
        Some("✓ promoted"),
        "fixture precondition"
    );
    let out = sb.rf(&["clean", "--yes"]);
    assert!(out.success, "clean failed: {}", out.combined());
    assert!(
        !sb.branch_exists(&roll),
        "promoted roll should be cleaned: {}",
        out.combined()
    );
}
