//! `core::rebuild` — this machine's most recent local nixos/home-manager
//! rebuild, surfaced in `rf status --json` / `rf list --json`.
//!
//! `RF_HOST_IDENTIFIER` fakes "this machine" deterministically (see
//! [`harness::Sandbox::rf_with_env`]) — otherwise these tests would depend on
//! whatever host actually runs `cargo test`.

mod harness;

use harness::Sandbox;

#[test]
fn nixos_switch_on_rolling_reports_the_branch_name() {
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("rolling", "merlin", 300, "switch");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(status["local_rebuild"]["host"], "merlin");
    assert_eq!(status["local_rebuild"]["nixos"]["switch"], "rolling");
    assert!(status["local_rebuild"]["nixos"]["test"].is_null());
    assert!(status["local_rebuild"]["nixos"]["boot"].is_null());
}

#[test]
fn a_switch_directly_on_stable_reports_the_stable_branch() {
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("main", "merlin", 1, "switch");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(status["local_rebuild"]["nixos"]["switch"], "main");
}

#[test]
fn integrating_a_roll_with_a_test_marker_still_attributes_it_to_the_original_roll() {
    // The exact real-world shape this was checked against in `~/.dotfiles`:
    // roll B (`[i]`/`rf integrate`) folds roll A in, and A's own marker must
    // still name A, never B — `[i]` is also a `--no-ff` merge, so A's commits
    // sit on *its* second-parent side from B's perspective, the same
    // protection that makes a roll survive its own graduation.
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("alpha", "0101");
    sb.add_nixos_rebuild("roll/1-0101-alpha", "merlin", 10, "test");

    sb.create_roll("beta", "0102");
    assert!(
        sb.rf(&["integrate", "roll/1-0101-alpha"]).success,
        "integrate alpha into beta"
    );

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["test"], "roll/1-0101-alpha",
        "the test marker must still name the roll it was actually made on, not the one that integrated it: {status}"
    );

    let list = sb.list_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    let beta = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["branch"] == "roll/2-0102-beta")
        .expect("beta listed");
    assert_eq!(
        beta["recent_rebuilds"],
        serde_json::json!([]),
        "the integrating roll must not claim the dependency's marker: {beta}"
    );
}

#[test]
fn an_ancient_switch_on_stable_before_any_roll_existed_is_not_misattributed_to_a_newer_roll() {
    // Every roll forks from *stable*, so a commit made directly on stable
    // before the roll was created is part of the roll's own inherited
    // baseline too — checking stable's bucket before any roll's is what
    // keeps this from reading as the roll's own switch.
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("main", "merlin", 1, "switch");

    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["switch"], "main",
        "an old switch inherited as baseline must not be attributed to a newer roll: {status}"
    );
}

#[test]
fn a_graduated_rolls_own_switch_is_still_attributed_to_it_while_its_branch_still_exists() {
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_nixos_rebuild("roll/1-0611-feature", "merlin", 10, "switch");

    assert!(sb.rf(&["graduate"]).success, "graduate");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["switch"], "roll/1-0611-feature",
        "a graduated roll's own switch must still name it while its branch exists: {status}"
    );
}

#[test]
fn a_switch_on_rolling_before_a_later_roll_forked_is_not_misattributed_to_that_roll() {
    // The exact bug shape `docs/internals/algorithms.md` warns a plain
    // ancestor check produces: every roll forks from *stable*
    // (`ops::create`), so once rolling's content reaches stable, a switch
    // commit made on rolling earlier is an ancestor of every roll created
    // afterwards too — but it must still report as `rolling`, never as the
    // roll.
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("rolling", "merlin", 1, "switch");
    let s = sb.rev("rolling");

    // Promote so S actually reaches main before the roll forks from it.
    sb.git(&["checkout", "main"]);
    sb.git(&[
        "merge",
        "--no-ff",
        "-m",
        "Promote rolling to main",
        "rolling",
    ]);

    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");

    assert!(
        sb.is_ancestor(&s, "roll/1-0611-feature"),
        "precondition: S must be an ancestor of the roll for this test to mean anything"
    );

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["switch"], "rolling",
        "a switch made on rolling before the roll existed must not be attributed to the roll: {status}"
    );
}

#[test]
fn a_switch_on_a_graduated_promoted_and_pruned_roll_still_reports_its_tier() {
    // The commit was only ever introduced to rolling/stable through a
    // merge's *second* parent (the graduation, then the promotion), which
    // `--first-parent` never walks — and once the roll's own branch is
    // pruned, no bucket is left to claim it as that roll's own either. The
    // fallback in `core::rebuild::attribute_fallback` is what keeps this from
    // reporting as nothing at all.
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_nixos_rebuild("roll/1-0611-feature", "merlin", 10, "switch");

    assert!(sb.rf(&["graduate"]).success, "graduate");
    sb.git(&["checkout", "rolling"]);
    assert!(sb.rf(&["promote"]).success, "promote rolling -> main");
    sb.git(&["branch", "-D", "roll/1-0611-feature"]);

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["switch"], "main",
        "a pruned roll's own switch marker must still report its tier, not vanish: {status}"
    );
}

#[test]
fn nixos_test_on_an_active_roll_is_not_superseded_without_a_switch() {
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_nixos_rebuild("roll/1-0611-feature", "merlin", 10, "test");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["test"],
        "roll/1-0611-feature"
    );
    assert!(status["local_rebuild"]["nixos"]["switch"].is_null());
}

#[test]
fn a_later_switch_on_an_unrelated_branch_still_supersedes_an_older_test() {
    // The exact real-world shape reported against `~/.dotfiles`: a plain
    // `switch` on `main` with no ancestry relationship at all to an earlier
    // `test` on an active, unmerged roll. Ancestry alone could never relate
    // these two commits, which is why supersede is time-based.
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_nixos_rebuild_at("roll/1-0611-feature", "merlin", 10, "test", 1_000_000_000);
    sb.add_nixos_rebuild_at("main", "merlin", 1, "switch", 1_000_000_100);

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(status["local_rebuild"]["nixos"]["switch"], "main");
    assert!(
        status["local_rebuild"]["nixos"]["test"].is_null(),
        "a later switch, even on an unrelated branch, must hide the older test: {status}"
    );
}

#[test]
fn a_later_switch_supersedes_an_earlier_test_on_the_same_roll() {
    // Explicit, well-separated timestamps: supersede is time-based (see
    // `core::rebuild::supersede`'s doc comment), and a sandbox otherwise
    // builds its whole history inside one second, which would make this
    // test's ordering a matter of machine speed rather than a fact.
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_nixos_rebuild_at("roll/1-0611-feature", "merlin", 10, "test", 1_000_000_000);
    sb.add_nixos_rebuild_at("roll/1-0611-feature", "merlin", 11, "switch", 1_000_000_100);

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["switch"],
        "roll/1-0611-feature"
    );
    assert!(
        status["local_rebuild"]["nixos"]["test"].is_null(),
        "a later switch should hide the superseded test marker: {status}"
    );
}

#[test]
fn a_boot_ahead_of_the_switch_is_not_superseded() {
    // Explicit, well-separated timestamps — see the comment in
    // `a_later_switch_supersedes_an_earlier_test_on_the_same_roll`.
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_nixos_rebuild_at("roll/1-0611-feature", "merlin", 10, "switch", 1_000_000_000);
    sb.add_nixos_rebuild_at("roll/1-0611-feature", "merlin", 11, "boot", 1_000_000_100);

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["nixos"]["switch"],
        "roll/1-0611-feature"
    );
    assert_eq!(
        status["local_rebuild"]["nixos"]["boot"],
        "roll/1-0611-feature"
    );
}

#[test]
fn home_manager_switch_and_test_build_shapes_are_detected() {
    // `Sandbox::nixos()`'s `vars/default.nix` sets `username = "gig"`.
    let sb = Sandbox::nixos();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.add_home_test_build("roll/1-0611-feature", "gig", "merlin");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert_eq!(
        status["local_rebuild"]["home"]["test"],
        "roll/1-0611-feature"
    );
    assert!(status["local_rebuild"]["home"]["switch"].is_null());

    let list = sb.list_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    let roll = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["branch"] == "roll/1-0611-feature")
        .expect("roll listed");
    assert_eq!(roll["recent_rebuilds"], serde_json::json!(["home:test"]));
}

#[test]
fn a_known_but_inactive_host_still_shows_its_last_rebuild() {
    // `wsl` is `host_active = false` in the `nixos()` fixture — inactive, not
    // unknown, and this feature is informational rather than a verification
    // gate, so it must still answer for a host that is offline/reinstalling.
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("rolling", "wsl", 5, "switch");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "wsl")]);
    assert_eq!(status["local_rebuild"]["nixos"]["switch"], "rolling");
}

#[test]
fn an_unrecognized_host_shows_nothing() {
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("rolling", "merlin", 300, "switch");

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "some-other-box")]);
    assert!(status["local_rebuild"].is_null(), "{status}");
}

#[test]
fn a_repo_with_no_hosts_modeled_shows_nothing_even_with_a_host_override() {
    // The same shape roll-flow's own `.roll-flow.toml` is in (`hosts = []`,
    // `host_active = {}`) — this feature must stay invisible there.
    let sb = Sandbox::plain();
    sb.init();

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert!(status["local_rebuild"].is_null(), "{status}");
}

#[test]
fn list_no_tui_shows_the_summary_line_even_with_no_roll_branches() {
    // A plain switch directly on `rolling` has no roll row to attach the
    // `build` column to at all — the exact case the summary line exists for
    // — so it must still print even on the early "no roll branches" path
    // (e.g. right after `rf prune` clears every roll).
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_nixos_rebuild("rolling", "merlin", 1, "switch");

    let out = sb.rf_with_env(&["list", "--no-tui"], &[("RF_HOST_IDENTIFIER", "merlin")]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.stdout
            .contains("last rebuild (merlin): nixos switch→rolling"),
        "summary line missing with no rolls: {}",
        out.stdout
    );
    assert!(out.stdout.contains("(no roll branches)"));
}

#[test]
fn legacy_generation_bump_without_a_mode_suffix_is_not_treated_as_a_marker() {
    // `add_generation_bump`'s shape (`"{host}: generation {N}"`, no trailing
    // `(mode)`) predates this feature and must keep reading as nothing, the
    // same way `listing_tolerates_generation_bump_commits` only asserts
    // robustness, never detection, for it.
    let sb = Sandbox::nixos();
    sb.init();
    sb.add_generation_bump("rolling", "merlin", 42);

    let status = sb.status_json_with_env(&[("RF_HOST_IDENTIFIER", "merlin")]);
    assert!(status["local_rebuild"]["nixos"]["switch"].is_null());
    assert!(status["local_rebuild"]["nixos"]["test"].is_null());
    assert!(status["local_rebuild"]["nixos"]["boot"].is_null());
}
