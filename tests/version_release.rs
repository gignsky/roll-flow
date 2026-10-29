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
    let tags_before = sb.tags();

    // `--final` answers only the finalize prompt, so the gate this test is
    // actually about (the bump-vs-unchanged check, run against the now
    // finalized bare version) is still reached and still refuses.
    let out = sb.rf(&["promote", "--final"]);
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
    // Nothing landed and nothing new was tagged (graduate's own `-dev` tag
    // from `graduate_one` above is unrelated to this refusal).
    assert!(
        !sb.has_commit_subject("main", "Promote"),
        "main must not carry a promotion"
    );
    assert_eq!(sb.tags(), tags_before, "no new tag on a refused promote");
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

    let tags_before = sb.tags();
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
    assert_eq!(sb.tags(), tags_before, "no new tag on a refused promote");
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
    let tags_before = sb.tags();

    let out = sb.rf(&["promote", "--yes"]);
    assert!(
        out.success,
        "promote should pass with the gate off: {}",
        out.combined()
    );
    assert!(sb.has_commit_subject("main", "Promote"));
    // With no version comparison there is no version to tag (the finalize
    // step still ran, but it created a commit, not a tag — see
    // `rolling_is_finalized_even_with_the_version_gate_off`).
    assert_eq!(sb.tags(), tags_before, "gate off means no release tag");
}

#[test]
fn rolling_is_finalized_even_with_the_version_gate_off() {
    // `version_gate` only governs the bump requirement; the marker must still
    // never reach stable, so finalizing is unconditional on `dev_versions`.
    let sb = Sandbox::cargo();
    sb.init();
    sb.set_config_flag("version_gate", false);
    graduate_one(&sb, "feature", "0611");
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1-dev");

    let out = sb.rf(&["promote", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1");
    assert_eq!(sb.cargo_version_at("main"), "0.0.1");
}

#[test]
fn force_bypasses_the_version_gate_and_records_it() {
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    // `--final` answers only the finalize prompt; `--yes` would also take the
    // automatic patch-bump default, defeating the point of this test (forcing
    // past a bump that was never resolved at all).
    let out = sb.rf(&[
        "promote",
        "--force",
        "--reason",
        "hotfix already shipped",
        "--final",
    ]);
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

    let out = sb.rf(&["promote", "--yes"]);
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
    // `--final` answers only the finalize prompt, so the gate this test is
    // actually about (no bump level resolvable) is still reached.
    let sb = Sandbox::cargo();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--final"]);
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
    // `--no-tag` governs this promotion's own release tag; `graduate_one`'s
    // `-dev` tag on rolling is unrelated and still there.
    assert!(!sb.tag_exists("v0.0.2"), "--no-tag must not create a tag");
}

#[test]
fn tag_on_promote_can_be_disabled_in_config() {
    let sb = Sandbox::cargo();
    sb.init();
    sb.set_config_flag("tag_on_promote", false);
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(
        !sb.tag_exists("v0.0.2"),
        "tag_on_promote = false must not tag"
    );
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

    let out = sb.rf(&["promote", "--final"]);
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

    // `--final` answers only the finalize prompt, leaving the tag-push offer
    // below ungated (no tty, no `--yes`) so it still exercises the unattended
    // skip this test is about.
    let out = sb.rf(&["promote", "--bump", "patch", "--final"]);
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
    let tags_before = sb.tags();

    let out = sb.rf(&["promote", "--dry-run", "--bump", "patch", "--yes"]);
    assert!(out.success, "dry-run failed: {}", out.combined());

    assert_eq!(sb.rev("main"), before, "dry-run must not merge");
    assert_eq!(sb.tags(), tags_before, "dry-run must not tag");
    assert_eq!(
        sb.cargo_version_at("rolling"),
        "0.0.1-dev",
        "dry-run must not bump or finalize"
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
    let tags_before = sb.tags();

    let out = sb.rf(&["verify", "--bump", "minor", "--yes"]);
    assert!(out.success, "verify failed: {}", out.combined());
    // Verify also finalizes (strips `-dev`) before bumping, the same way it
    // already committed a bare bump before this feature existed.
    assert_eq!(sb.cargo_version_at("rolling"), "0.1.0");
    assert!(
        sb.has_commit_subject("rolling", "chore(release): finalize 0.0.1 for promotion"),
        "the finalize step should be committed on rolling"
    );
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
    assert_eq!(sb.tags(), tags_before, "verify must not tag");
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
fn a_new_roll_is_marked_with_its_number_and_graduation_resolves_it() {
    // The whole point of the marker: the checked-out version says which roll
    // you are on. Graduating doesn't need to strip it with a commit anymore —
    // `ops::graduate`'s own resolution (backed by the version merge driver for
    // the conflicting case) swaps the roll's `-roll<N>` for rolling's own
    // steady-state `-dev` marker as part of the ordinary graduation merge,
    // regardless of what the roll's said. The roll's own branch keeps its
    // marker; nothing rewrites it.
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

    // The roll branch itself is untouched; rolling's merge commit carries the
    // roll's marker swapped for rolling's own `-dev`.
    assert_eq!(sb.cargo_version_at(&branch), "0.0.1-roll1");
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1-dev");

    // And the gate then behaves exactly as it does without the marker, once
    // finalized (`--final` reaches the bump gate without also pre-accepting
    // a bump level the way `--yes` would).
    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote", "--final"]);
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

#[test]
fn verify_marks_a_roll_that_was_created_without_the_marker() {
    // A roll that predates the feature, or was started with --no-dev-version,
    // is brought in line by the first verify rather than needing a hand edit.
    let sb = Sandbox::cargo();
    sb.init();
    let out = sb.rf(&["create", "late", "--date", "0611", "--no-dev-version"]);
    assert!(out.success, "{}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1");
    sb.commit_file("work.txt", "work\n", "work");

    let out = sb.rf(&["verify"]);
    assert!(out.success, "verify failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");

    // Idempotent: a second verify has nothing to mark and makes no commit.
    let before = sb.rev("HEAD");
    let out = sb.rf(&["verify"]);
    assert!(out.success, "{}", out.combined());
    assert_eq!(sb.rev("HEAD"), before);
}

#[test]
fn graduate_refuses_a_dev_marker_that_belongs_to_another_roll() {
    // A roll should only ever wear its own `-roll<N>` marker. One naming a
    // different roll (here, simulating a scrambled version history) must stop
    // graduation before anything else, rather than being silently stripped as
    // if it had been legitimate.
    let sb = Sandbox::cargo();
    sb.init();
    sb.create_roll("alpha", "0611"); // takes roll number 1
    sb.git(&["checkout", "rolling"]);

    let out = sb.create_roll("beta", "0612");
    assert!(out.success, "create beta: {}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll2");

    sb.write_cargo_version("0.0.1-roll1");
    sb.git(&["add", "Cargo.toml"]);
    sb.git(&["commit", "-m", "oops: wrong roll's marker"]);
    sb.commit_file("work.txt", "work\n", "beta work");

    let out = sb.rf(&["graduate"]);
    assert!(
        !out.success,
        "graduate must refuse a foreign dev marker: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("roll 1's dev marker"),
        "{}",
        out.combined()
    );

    // Nothing was touched: still on beta, still carrying the foreign marker.
    assert_eq!(sb.current_branch(), "roll/2-0612-beta");
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
}

#[test]
fn graduate_dry_run_also_catches_a_foreign_dev_marker() {
    // The check is read-only, so a preview catches it too rather than only
    // discovering it on a real run.
    let sb = Sandbox::cargo();
    sb.init();
    sb.create_roll("alpha", "0611");
    sb.git(&["checkout", "rolling"]);
    sb.create_roll("beta", "0612");
    sb.write_cargo_version("0.0.1-roll1");
    sb.git(&["add", "Cargo.toml"]);
    sb.git(&["commit", "-m", "oops: wrong roll's marker"]);

    let out = sb.rf(&["graduate", "--dry-run"]);
    assert!(!out.success, "{}", out.combined());
    assert!(
        out.combined().contains("roll 1's dev marker"),
        "{}",
        out.combined()
    );
}

#[test]
fn promote_fallthrough_to_graduate_resolves_the_dev_marker() {
    // `rf promote` run from a roll branch redirects to graduate — a second
    // path into the same merge, distinct from `rf graduate` itself. Both go
    // through `ops::graduate`, so both get the version merge driver's
    // resolution for free.
    let sb = Sandbox::cargo();
    sb.init();
    let out = sb.create_roll("solo", "0611");
    assert!(out.success, "{}", out.combined());
    let branch = sb.current_branch();
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
    sb.commit_file("work.txt", "work\n", "work");

    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote fall-through: {}", out.combined());
    assert_eq!(sb.cargo_version_at(&branch), "0.0.1-roll1");
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1-dev");
}

#[test]
fn a_real_merge_conflict_on_an_unrelated_file_still_fails_normally() {
    // The version merge driver only resolves the version line; a genuine
    // conflict elsewhere in the file (or in another file) must surface
    // exactly as it would with no driver installed at all.
    let sb = Sandbox::cargo();
    sb.init();
    sb.git(&["checkout", "rolling"]);
    let out = sb.create_roll("conflict", "0611");
    assert!(out.success, "{}", out.combined());
    let branch = sb.current_branch();
    sb.commit_file("clash.txt", "roll side\n", "roll edit");

    sb.git(&["checkout", "rolling"]);
    sb.commit_file("clash.txt", "rolling side\n", "rolling edit");
    sb.git(&["checkout", &branch]);

    let out = sb.rf(&["graduate"]);
    assert!(
        !out.success,
        "conflicting graduation should fail: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("aborted"),
        "error should mention the abort: {}",
        out.combined()
    );

    assert_eq!(sb.current_branch(), branch);
    let leftover: Vec<String> = sb
        .git(&["status", "--porcelain"])
        .lines()
        .filter(|l| !l.ends_with(".roll-flow.toml") && !l.ends_with(".gitattributes"))
        .map(String::from)
        .collect();
    assert!(leftover.is_empty(), "working tree not clean: {leftover:?}");
}

#[test]
fn graduating_past_an_advanced_rolling_branch_does_not_conflict_on_the_marker() {
    // The scenario the version merge driver exists for: rolling moved (here,
    // simulating another roll's bump) since this roll branched, so the roll's
    // own `-rollN` marker and rolling's new version touch the same Cargo.toml
    // line. Without the driver this is a real conflict; with it, graduation
    // just succeeds, and rolling ends up with the higher of the two numbers,
    // wearing its own `-dev` marker rather than the roll's.
    let sb = Sandbox::cargo();
    sb.init();
    let out = sb.create_roll("solo", "0611");
    assert!(out.success, "{}", out.combined());
    let branch = sb.current_branch();
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
    sb.commit_file("work.txt", "work\n", "work");

    sb.git(&["checkout", "rolling"]);
    sb.write_cargo_version("0.0.2");
    sb.git(&["add", "Cargo.toml"]);
    sb.git(&["commit", "-m", "simulate an independent bump on rolling"]);
    sb.git(&["checkout", &branch]);

    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduate should not conflict: {}",
        out.combined()
    );
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.2-dev");
}

// ── npm arm ──────────────────────────────────────────────────────────────────

#[test]
fn npm_promote_refuses_an_unchanged_version() {
    let sb = Sandbox::npm();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--final"]);
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
}

#[test]
fn npm_promote_bumps_commits_merges_and_tags() {
    let sb = Sandbox::npm();
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());

    assert!(
        sb.has_commit_subject("main", "chore(release): bump version to 0.0.2"),
        "expected a chore(release) commit to reach main"
    );
    assert_eq!(sb.package_version_at("main"), "0.0.2");
    assert!(sb.has_commit_subject("main", "Promote"));
    assert!(sb.tag_exists("v0.0.2"), "tags: {:?}", sb.tags());
}

#[test]
fn npm_new_roll_is_marked_and_graduation_resolves_it() {
    // The npm twin of `a_new_roll_is_marked_with_its_number_and_graduation_resolves_it`:
    // the same marker lifecycle, read and written through `package.json`
    // (and its lockfile) instead of `Cargo.toml`.
    let sb = Sandbox::npm();
    sb.init();

    let out = sb.create_roll("dev-marked", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    let branch = sb.current_branch();
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1-roll1");

    sb.commit_file("work.txt", "work\n", "do the work");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    // The roll branch itself is untouched; rolling's merge commit carries the
    // roll's marker swapped for rolling's own `-dev`.
    assert_eq!(sb.package_version_at(&branch), "0.0.1-roll1");
    assert_eq!(sb.package_version_at("rolling"), "0.0.1-dev");

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote", "--final"]);
    assert!(
        !out.success,
        "promote should still demand a bump: {}",
        out.combined()
    );
}

#[test]
fn npm_promote_commits_both_lockfile_fields_matching_the_manifest() {
    let sb = Sandbox::npm();
    sb.init();
    sb.commit_package_lock("0.0.1");
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert_eq!(sb.package_version_at("main"), "0.0.2");
    assert_eq!(
        sb.package_lock_version_at("main"),
        "0.0.2",
        "the lockfile's root version must track the bump"
    );
}

#[test]
fn npm_lockfile_markers_from_two_rolls_merge_through_the_driver() {
    // The npm twin of the Cargo merge-driver coverage in `version_merge.rs`,
    // exercised here through the actual graduate path rather than unit-level
    // `merge_texts` calls: rolling moved (simulating another roll's bump)
    // since this roll branched, so the roll's own marker and rolling's new
    // version touch the same `package.json`/`package-lock.json` lines.
    let sb = Sandbox::npm();
    sb.init();
    sb.commit_package_lock("0.0.1");
    let out = sb.create_roll("solo", "0611");
    assert!(out.success, "{}", out.combined());
    let branch = sb.current_branch();
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1-roll1");
    sb.commit_file("work.txt", "work\n", "work");

    sb.git(&["checkout", "rolling"]);
    sb.commit_package_version("0.0.2");
    sb.commit_package_lock("0.0.2");
    sb.git(&["checkout", &branch]);

    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduate should not conflict: {}",
        out.combined()
    );
    assert_eq!(sb.package_version_at("rolling"), "0.0.2-dev");
    assert_eq!(sb.package_lock_version_at("rolling"), "0.0.2-dev");
}

/// A Cargo project that also carries a versioned `package.json` (Tauri-style
/// apps, a Rust backend released together with its JS front end).
fn dual() -> Sandbox {
    let sb = Sandbox::cargo();
    sb.write_package_version("0.0.1");
    sb.git(&["add", "package.json"]);
    sb.git(&["commit", "-m", "add package.json too"]);
    sb.init();
    sb
}

#[test]
fn a_repo_with_both_manifests_marks_both() {
    let sb = dual();

    let out = sb.create_roll("both", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1-roll1");
}

#[test]
fn pinning_version_manifest_keeps_the_other_manifest_out_of_it() {
    let sb = dual();
    sb.set_config_string("version_manifest", "cargo");

    let out = sb.create_roll("pinned", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1-roll1");
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1");
}

#[test]
fn both_manifests_are_graduated_bumped_and_promoted_together() {
    let sb = dual();
    graduate_one(&sb, "feature", "0611");
    assert_eq!(sb.cargo_version_at("rolling"), "0.0.1-dev");
    assert_eq!(sb.package_version_at("rolling"), "0.0.1-dev");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());

    assert_eq!(sb.cargo_version_at("main"), "0.0.2");
    assert_eq!(sb.package_version_at("main"), "0.0.2");
    assert!(sb.tag_exists("v0.0.2"), "tags: {:?}", sb.tags());
}

/// A site versioned through a root `package.json`, with a Rust service in
/// `keeper/` that carries its own, unrelated version and its own lockfile —
/// the shape of a repo whose crate is not at the root.
fn nested() -> Sandbox {
    let sb = Sandbox::npm();
    sb.write(
        "keeper/Cargo.toml",
        "[package]\nname = \"keeper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    sb.write(
        "keeper/Cargo.lock",
        "version = 4\n\n[[package]]\nname = \"keeper\"\nversion = \"0.1.0\"\n",
    );
    // A tooling-only manifest with no version: never versioned.
    sb.write("scripts/tool/package.json", "{\n  \"private\": true\n}\n");
    sb.git(&["add", "."]);
    sb.git(&["commit", "-m", "add keeper"]);
    sb.init();
    sb
}

/// The version `keeper/Cargo.toml` (or, with `lock`, `keeper/Cargo.lock`'s own
/// entry) carries at `refspec`.
fn keeper_version_at(sb: &Sandbox, refspec: &str, lock: bool) -> String {
    let file = if lock { "Cargo.lock" } else { "Cargo.toml" };
    let text = sb.git(&["show", &format!("{refspec}:keeper/{file}")]);
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("version = \""))
        .and_then(|l| l.split('"').nth(1))
        .unwrap_or_else(|| panic!("no version in keeper/{file} at {refspec}: {text}"))
        .to_string()
}

#[test]
fn a_crate_in_a_subdirectory_is_versioned_beside_the_root_manifest() {
    let sb = nested();

    let out = sb.create_roll("both", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1-roll1");
    assert_eq!(keeper_version_at(&sb, "HEAD", false), "0.1.0-roll1");
    assert_eq!(keeper_version_at(&sb, "HEAD", true), "0.1.0-roll1");
    assert!(
        !sb.git(&["show", "HEAD:scripts/tool/package.json"])
            .contains("version"),
        "a manifest with no version is left alone"
    );
}

#[test]
fn a_nested_crate_is_graduated_bumped_and_promoted_but_the_root_names_the_tag() {
    let sb = nested();
    graduate_one(&sb, "feature", "0611");
    assert_eq!(sb.package_version_at("rolling"), "0.0.1-dev");
    assert_eq!(keeper_version_at(&sb, "rolling", false), "0.1.0-dev");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());

    assert_eq!(sb.package_version_at("main"), "0.0.2");
    assert_eq!(keeper_version_at(&sb, "main", false), "0.1.1");
    assert_eq!(keeper_version_at(&sb, "main", true), "0.1.1");
    // The root manifest outranks the nested crate, Cargo or not.
    assert!(sb.tag_exists("v0.0.2"), "tags: {:?}", sb.tags());
    assert!(!sb.tag_exists("v0.1.1"), "tags: {:?}", sb.tags());
}

#[test]
fn listing_version_manifest_paths_keeps_a_nested_crate_out() {
    let sb = nested();
    sb.set_config_raw("version_manifest", "[\"package.json\"]");

    let out = sb.create_roll("listed", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1-roll1");
    assert_eq!(keeper_version_at(&sb, "HEAD", false), "0.1.0");
}

#[test]
fn a_version_manifest_path_that_is_not_a_manifest_is_refused() {
    let sb = nested();
    sb.set_config_raw("version_manifest", "[\"keeper/Cargo.lock\"]");

    let out = sb.rf(&["status", "--json"]);
    assert!(!out.success, "a bad path must not load: {}", out.combined());
    assert!(
        out.combined().contains("keeper/Cargo.lock"),
        "{}",
        out.combined()
    );
}

#[test]
fn promote_names_the_manifest_that_still_owes_a_bump() {
    let sb = dual();
    graduate_one(&sb, "feature", "0611");

    // Rolling already moved Cargo.toml past main by hand; package.json has not.
    sb.commit_cargo_version("0.0.2");
    let out = sb.rf(&["promote", "--final"]);
    assert!(
        !out.success,
        "promote should demand a bump: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("package.json"),
        "the failure should name the manifest that owes the bump: {}",
        out.combined()
    );
}

#[test]
fn a_tooling_only_package_json_with_no_version_key_is_unaffected() {
    // A package.json without a `version` field is unremarkable (prettier,
    // husky, commitlint configs often look like this) in a way a Cargo.toml
    // without one never is. It must read the same as no manifest at all —
    // `NotApplicable`, not `Unreadable` — or every rf-managed repo with a
    // tooling-only package.json would have promote start failing the moment
    // this feature landed.
    let sb = Sandbox::plain();
    sb.write("package.json", "{\"name\": \"tooling\", \"private\": true}");
    sb.git(&["add", "package.json"]);
    sb.git(&["commit", "-m", "tooling config, no version"]);
    sb.init();
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert!(sb.has_commit_subject("main", "Promote"));
    assert!(sb.tags().is_empty(), "nothing to tag without a version");
}

#[test]
fn a_roll_can_add_the_first_version_to_a_tooling_only_package_json() {
    // The normal way a Node app starts using rf: main's package.json has no
    // `version` field yet (most apps ship without one), and the roll is the
    // one introducing it. `check` must read that as "a manifest added by
    // this branch" (Ok, a bump from nothing) rather than `Unreadable` — the
    // base having no readable version must count the same as the base having
    // no file at all.
    let sb = Sandbox::plain();
    sb.write("package.json", "{\"name\": \"x\", \"private\": true}");
    sb.git(&["add", "package.json"]);
    sb.git(&["commit", "-m", "tooling config, no version"]);
    sb.init();
    sb.set_config_flag("dev_versions", false);

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_package_version("0.1.0");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert_eq!(sb.package_version_at("main"), "0.1.0");
    assert!(sb.tag_exists("v0.1.0"), "tags: {:?}", sb.tags());
}

#[test]
fn version_manifest_npm_forced_also_lets_a_roll_add_the_first_version() {
    // The forced-override twin: `version_manifest = "npm"` must not make
    // `detect_at_forced` claim the target has a readable npm version when it
    // doesn't — forcing a kind is not a promise that kind already has one.
    let sb = Sandbox::cargo();
    sb.write("package.json", "{\"name\": \"x\", \"private\": true}");
    sb.git(&["add", "package.json"]);
    sb.git(&["commit", "-m", "add tooling-only package.json too"]);
    sb.init();
    sb.set_config_string("version_manifest", "npm");

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_package_version("0.1.0-roll1");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert_eq!(sb.package_version_at("main"), "0.1.0");
    assert!(sb.tag_exists("v0.1.0"), "tags: {:?}", sb.tags());
}

#[test]
fn version_manifest_npm_forces_the_npm_arm_through_promote_too() {
    // The write-path override alone (covered by
    // `version_manifest_npm_forces_the_npm_arm_in_a_repo_with_both_manifests`)
    // is not enough: the gate's read and the bump's write must agree on the
    // same kind all the way through `rf promote`, including the per-ref
    // comparison against stable (`run_promote_step`'s `check_against`).
    let sb = Sandbox::cargo();
    sb.write_package_version("0.0.1");
    sb.git(&["add", "package.json"]);
    sb.git(&["commit", "-m", "add package.json too"]);
    sb.init();
    sb.set_config_string("version_manifest", "npm");
    graduate_one(&sb, "feature", "0611");

    let out = sb.rf(&["promote", "--bump", "patch", "--yes"]);
    assert!(out.success, "promote failed: {}", out.combined());
    assert_eq!(sb.package_version_at("main"), "0.0.2");
    assert_eq!(
        sb.cargo_version_at("main"),
        "0.0.1",
        "Cargo.toml must stay untouched when the override picks npm"
    );
    assert!(sb.tag_exists("v0.0.2"), "tags: {:?}", sb.tags());
    // If the gate's own comparison auto-detected Cargo instead of honouring
    // the override, it would see Cargo's 0.0.1 as Unchanged against rolling
    // (also 0.0.1) and bump package.json straight to a `-dev` marker inside
    // `run_promote_step`'s in-merge finalize — landing a leftover bump commit
    // that was never actually gated against npm's own value.
    assert!(
        !sb.has_commit_subject("main", "0.0.2-dev"),
        "a dev-marked bump commit means the gate compared the wrong manifest"
    );
}

#[test]
fn version_manifest_npm_forces_the_npm_arm_in_a_repo_with_both_manifests() {
    let sb = Sandbox::cargo();
    sb.write_package_version("0.0.1");
    sb.git(&["add", "package.json"]);
    sb.git(&["commit", "-m", "add package.json too"]);
    sb.init();
    sb.set_config_string("version_manifest", "npm");

    let out = sb.create_roll("both", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    assert_eq!(sb.package_version_at("HEAD"), "0.0.1-roll1");
    // Cargo.toml is untouched: the override picked npm as authoritative.
    assert_eq!(sb.cargo_version_at("HEAD"), "0.0.1");
}
