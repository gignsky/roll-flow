//! Version gate and release tagging on `rf verify` / `rf promote`.
//!
//! These cover the behavior that previously only existed in CI: a promotion must
//! raise `Cargo.toml`'s version above the branch it targets
//! (`version-bump-check.yml`), and a promotion that lands gets an annotated
//! `vX.Y.Z` tag on its merge commit (`tag-on-main.yml`). Repos without a
//! `Cargo.toml` must be entirely unaffected, which is what keeps the dotfiles
//! repo — roll-flow's original target — working unchanged.

mod harness;

use harness::Sandbox;

/// Create a roll, do work on it, and graduate it into rolling. Leaves HEAD on
/// `rolling`, ready to promote.
fn graduate_one(sb: &Sandbox, slug: &str, date: &str) -> String {
    let out = sb.create_roll(slug, date);
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();
    sb.commit_file(&format!("{slug}.txt"), "work\n", &format!("{slug} work"));
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());
    sb.git(&["checkout", "rolling"]);
    branch
}

// ── the gate blocks ─────────────────────────────────────────────────────────

#[test]
fn promote_refuses_when_version_is_unchanged() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote"]);
    assert!(
        !out.success,
        "promote should refuse an unbumped version: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("unchanged from 'main'"),
        "error should name the unchanged version: {}",
        out.combined()
    );
    // Nothing landed and nothing was tagged.
    assert!(
        !sb.has_commit_subject("main", "Promote"),
        "main must not carry a promotion"
    );
    assert!(sb.tags().is_empty(), "no tag on a refused promote");
}

#[test]
fn promote_refuses_a_lower_version_even_with_bump_requested() {
    // main is ahead; the branch being promoted went backwards. No bump level can
    // make that safe, so it must fail outright rather than prompt.
    let sb = Sandbox::cargo();
    sb.init();
    sb.commit_cargo_version("0.5.0");
    graduate_one(&sb, "feature", "0611");
    // Drop rolling's version below main's.
    sb.commit_cargo_version("0.0.9");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(
        !out.success,
        "a lower version must be a hard error: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("lower than"),
        "error should say the version is lower: {}",
        out.combined()
    );
    assert!(sb.tags().is_empty(), "no tag on a refused promote");
}

#[test]
fn promote_is_unaffected_in_a_repo_without_cargo_toml() {
    // The dotfiles case: no manifest, so the gate is not applicable and the
    // command behaves exactly as it did before this feature existed.
    let sb = Sandbox::plain();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(sb.has_commit_subject("main", "Promote"));
    assert!(sb.tags().is_empty(), "nothing to tag without a manifest");
    assert!(
        !out.combined().contains("Version:"),
        "no version line in a repo that does not version this way: {}",
        out.combined()
    );
}

#[test]
fn version_gate_can_be_disabled_in_config() {
    let sb = Sandbox::cargo();
    sb.init();
    sb.set_config_flag("version_gate", false);
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote"]);
    assert!(
        out.success,
        "promote should pass with the gate off: {}",
        out.combined()
    );
    assert!(sb.has_commit_subject("main", "Promote"));
    // With no version comparison there is no version to tag.
    assert!(sb.tags().is_empty(), "gate off means no release tag");
}

#[test]
fn force_bypasses_the_version_gate_and_records_it() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--force", "--reason", "hotfix already shipped"]);
    assert!(out.success, "forced promote failed: {}", out.combined());

    let body = sb.git(&["log", "-1", "--format=%B", "main"]);
    assert!(
        body.contains("Forced-Bypass:") && body.contains("version bump check"),
        "the bypass must be recorded in the merge commit: {body}"
    );
    assert!(
        body.contains("Force-Reason: hotfix already shipped"),
        "the reason must be recorded: {body}"
    );
}

// ── bumping ─────────────────────────────────────────────────────────────────

#[test]
fn promote_bumps_commits_merges_and_tags() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());

    // The bump landed as its own commit on the branch being promoted.
    assert!(
        sb.has_commit_subject("main", "chore(release): bump version to 0.0.2"),
        "expected a chore(release) commit to reach main"
    );
    assert_eq!(sb.cargo_version_at("main"), "0.0.2");
    assert!(sb.has_commit_subject("main", "Promote"));

    // ...and the release tag points at the promotion merge commit.
    assert!(sb.tag_exists("v0.0.2"), "tags: {:?}", sb.tags());
    assert!(sb.tag_is_annotated("v0.0.2"), "the tag must be annotated");
    assert_eq!(
        sb.tag_target("v0.0.2"),
        sb.rev("main"),
        "the tag must point at the merge commit on main"
    );
    let msg = sb.tag_message("v0.0.2");
    assert!(
        msg.contains("Release v0.0.2"),
        "tag subject should match the CI format: {msg}"
    );
    assert!(
        msg.contains("roll/1-0611-feature"),
        "tag body should list the promoted roll: {msg}"
    );
}

#[test]
fn bump_levels_raise_the_right_field() {
    for (level, expected) in [("patch", "0.0.2"), ("minor", "0.1.0"), ("major", "1.0.0")] {
        let sb = Sandbox::cargo();
        sb.init();
        graduate_one(&sb, "feature", "0611");

        let out = sb.rf(&["promote", "--bump", level, "--yes"]);
        assert!(
            out.success,
            "promote --bump {level} failed: {}",
            out.combined()
        );
        assert_eq!(sb.cargo_version_at("main"), expected, "level {level}");
        assert!(sb.tag_exists(&format!("v{expected}")), "level {level}");
    }
}

#[test]
fn an_already_bumped_version_promotes_without_a_bump_commit() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");
    // Bump by hand on rolling, the way a roll would normally carry one.
    sb.commit_cargo_version("0.2.0");

    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(
        !sb.has_commit_subject("main", "chore(release)"),
        "rf must not add a second bump when one is already present"
    );
    assert_eq!(sb.cargo_version_at("main"), "0.2.0");
    assert!(sb.tag_exists("v0.2.0"), "tags: {:?}", sb.tags());
}

#[test]
fn bump_is_refused_non_interactively_without_a_level() {
    // No tty, no --bump, no --yes: there is no safe default to pick, so the
    // command must fail with the fix spelled out rather than guessing.
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote"]);
    assert!(!out.success);
    assert!(
        out.combined().contains("--bump"),
        "the error should point at --bump: {}",
        out.combined()
    );
}

// ── tagging behavior ────────────────────────────────────────────────────────

#[test]
fn no_tag_skips_tagging_but_still_promotes() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes", "--no-tag"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(sb.has_commit_subject("main", "Promote"));
    assert_eq!(sb.cargo_version_at("main"), "0.0.2");
    assert!(sb.tags().is_empty(), "--no-tag must not create a tag");
}

#[test]
fn tag_on_promote_can_be_disabled_in_config() {
    let sb = Sandbox::cargo();
    sb.init();
    sb.set_config_flag("tag_on_promote", false);
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(sb.tags().is_empty(), "tag_on_promote = false must not tag");
    // The version gate is still enforced.
    assert_eq!(sb.cargo_version_at("main"), "0.0.2");
}

#[test]
fn an_existing_tag_is_left_alone_rather_than_failing() {
    // Mirrors tag-on-main.yml's idempotency: a tag that is already there is a
    // no-op, not an error.
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");
    sb.commit_cargo_version("0.3.0");
    // Pre-create the tag somewhere unrelated.
    let preexisting = sb.rev("main");
    sb.git(&["tag", "-a", "v0.3.0", "-m", "made earlier", &preexisting]);

    let out = sb.rf(&["promote"]);
    assert!(
        out.success,
        "an existing tag must not fail the promote: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("already exists"),
        "the outcome should be reported: {}",
        out.combined()
    );
    // Still pointing where it was; not moved onto the merge commit.
    assert_eq!(sb.tag_target("v0.3.0"), preexisting);
}

#[test]
fn a_local_promote_does_not_push_the_tag_unattended() {
    // The tag push is the only remote write outside `rf prune`, so without a tty
    // and without --yes it must not happen.
    let sb = Sandbox::with_origin();
    sb.init();
    sb.commit_cargo_version("0.0.1");
    sb.git(&["push", "origin", "main"]);
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(sb.tag_exists("v0.0.2"), "the tag is still created locally");

    let (_, remote_tags, _) = sb.git_try(&["ls-remote", "--tags", "origin"]);
    assert!(
        !remote_tags.contains("v0.0.2"),
        "the tag must not be pushed unattended: {remote_tags}"
    );
    assert!(
        out.combined().contains("was not pushed"),
        "the skip should be reported: {}",
        out.combined()
    );
}

#[test]
fn yes_pushes_the_tag_to_origin() {
    let sb = Sandbox::with_origin();
    sb.init();
    sb.commit_cargo_version("0.0.1");
    sb.git(&["push", "origin", "main"]);
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());

    let (_, remote_tags, _) = sb.git_try(&["ls-remote", "--tags", "origin"]);
    assert!(
        remote_tags.contains("v0.0.2"),
        "--yes should push the tag: {remote_tags}"
    );
}

// ── dry-run ─────────────────────────────────────────────────────────────────

#[test]
fn dry_run_neither_bumps_nor_tags_nor_merges() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");
    let before = sb.rev("main");

    let out = sb.rf(&["promote", "--dry-run", "--bump", "patch", "--yes"]);
    assert!(out.success, "dry-run failed: {}", out.combined());

    assert_eq!(sb.rev("main"), before, "dry-run must not merge");
    assert!(sb.tags().is_empty(), "dry-run must not tag");
    assert_eq!(
        sb.cargo_version_at("rolling"),
        "0.0.1",
        "dry-run must not bump"
    );
    assert!(
        out.combined().contains("Dry-run"),
        "expected dry-run output: {}",
        out.combined()
    );
}

#[test]
fn version_gate_applies_when_stable_exists_only_on_origin() {
    // Regression: when stable has never been checked out locally, the bump
    // resolution used to skip the check, so `--bump` did nothing and the
    // promotion then failed on a gate that should already have been satisfied.
    let sb = Sandbox::with_origin();
    sb.init();
    sb.commit_cargo_version("0.0.1");
    sb.git(&["push", "origin", "main"]);
    graduate_one(&sb, "feature", "0611");

    // Drop the local stable branch so only `origin/main` remains.
    sb.git(&["branch", "-D", "main"]);
    assert!(!sb.branch_exists("main"), "local main should be gone");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("main"), "0.0.2", "the bump must apply");
    assert!(sb.tag_exists("v0.0.2"), "tags: {:?}", sb.tags());
}

// ── verify ──────────────────────────────────────────────────────────────────

#[test]
fn verify_fails_on_an_unbumped_version() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["verify"]);
    assert!(
        !out.success,
        "verify should fail an unbumped version: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("bump"),
        "the failure should mention bumping: {}",
        out.combined()
    );
}

#[test]
fn verify_bumps_and_then_passes() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["verify", "--bump", "minor", "--yes"]);
    assert!(out.success, "verify failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("rolling"), "0.1.0");
    assert!(
        sb.has_commit_subject("rolling", "chore(release): bump version to 0.1.0"),
        "the bump should be committed on rolling"
    );
    assert!(
        out.combined().contains("Verification passed"),
        "expected a passing verify: {}",
        out.combined()
    );
    // Verify never merges or tags.
    assert!(sb.tags().is_empty(), "verify must not tag");
    assert!(!sb.has_commit_subject("main", "Promote"));
}

#[test]
fn verify_on_a_roll_branch_ignores_the_version_gate() {
    // Graduation into rolling is explicitly out of scope for the version gate,
    // so a roll with an unchanged version still verifies clean.
    let sb = Sandbox::cargo();
    sb.init();
    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("work.txt", "w\n", "roll work");

    let out = sb.rf(&["verify"]);
    assert!(
        out.success,
        "a roll branch should verify without a bump: {}",
        out.combined()
    );
}

// ── dev versions ────────────────────────────────────────────────────────────

#[test]
fn a_new_roll_is_marked_with_its_number_and_graduation_strips_it() {
    // The whole point of the marker: the checked-out version says which roll
    // you are on, and by graduation time it is gone again — leaving exactly the
    // version the roll branched from, so promotion still demands a real bump.
    let sb = Sandbox::cargo();
    sb.init();

    let out = sb.create_roll("dev-marked", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
    assert!(out.combined().contains("0.0.1-roll1"), "{}", out.combined());

    sb.commit_file("work.txt", "work\n", "do the work");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    // Stripped on the roll branch itself, before the merge, so rolling carries
    // a clean version and so does the roll.
    assert_eq!(sb.cargo_version_at(&branch), "0.0.1");
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1");

    // And the gate then behaves exactly as it does without the marker.
    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(
        !out.success,
        "promote should still demand a bump: {}",
        out.combined()
    );
}

#[test]
fn a_dev_version_can_never_be_promoted() {
    // Numerically above its target and still refused: `0.9.9-roll1 > 0.0.1`,
    // but shipping a dev marker to stable — and tagging it `v0.9.9-roll1` —
    // is what this gate exists to stop.
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    sb.write_cargo_version("0.9.9-roll1");
    sb.git(&["add", "Cargo.toml"]);
    sb.git(&["commit", "-m", "hand-written dev version on rolling"]);

    let out = sb.rf(&["promote"]);
    assert!(
        !out.success,
        "a dev version must not promote: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("dev marker"),
        "the error should name the fix: {}",
        out.combined()
    );
}

#[test]
fn the_marker_can_be_declined_per_invocation() {
    let sb = Sandbox::cargo();
    sb.init();

    let out = sb.rf(&["create", "plain", "--date", "0611", "--no-dev-version"]);
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1");
}

#[test]
fn a_repo_without_a_manifest_is_untouched_by_the_marker() {
    // The rule that keeps the dotfiles repo working: no `Cargo.toml`, nothing
    // to mark, and no commit invented to say so.
    let sb = Sandbox::plain();
    sb.init();

    let before = sb.rev("HEAD");
    let out = sb.create_roll("no-manifest", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.rev("HEAD"), before, "a commit was made anyway");
    assert!(
        !out.combined().contains("version marked"),
        "{}",
        out.combined()
    );
}
