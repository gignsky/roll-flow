//! Per-host verification gate tests (issue #106).
//!
//! Driven through the shared [`harness::Sandbox`] using the `nixos()` fixture,
//! whose active hosts are `ganoslal` + `merlin` (`wsl` is inactive). These
//! assert that `rf verify`/`rf promote` run one gate per active host, report
//! per-host pass/fail, exclude inactive hosts, block on failure, and are a clean
//! no-op when no host gates are configured.

mod harness;

use harness::Sandbox;

/// init the nixos fixture, create a roll with a commit, and leave HEAD on the
/// new roll branch — the common prelude for the verify tests.
fn roll_with_work() -> Sandbox {
    let sb = Sandbox::nixos();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());
    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("work.txt", "w\n", "roll work");
    sb
}

#[test]
fn verify_reports_each_active_host_and_skips_inactive() {
    let sb = roll_with_work();
    // A trivially-passing gate for every active host.
    sb.set_host_gates(&["true"]);

    let out = sb.rf(&["verify"]);
    assert!(out.success, "verify should pass: {}", out.combined());

    let combined = out.combined();
    assert!(
        combined.contains("ganoslal: PASSED"),
        "expected ganoslal PASSED: {combined}"
    );
    assert!(
        combined.contains("merlin: PASSED"),
        "expected merlin PASSED: {combined}"
    );
    // wsl is inactive and must never appear in host verification output.
    assert!(
        !combined.contains("wsl"),
        "inactive host wsl should be excluded: {combined}"
    );
}

#[test]
fn verify_fails_and_names_the_failing_host() {
    let sb = roll_with_work();
    // Every host but `merlin` passes; `merlin` fails its gate.
    sb.set_host_gates(&["test {host} != merlin"]);

    let out = sb.rf(&["verify"]);
    assert!(
        !out.success,
        "verify should fail when a host gate fails: {}",
        out.combined()
    );

    let combined = out.combined();
    assert!(
        combined.contains("host verification failed") && combined.contains("merlin"),
        "error should name the failing host merlin: {combined}"
    );
    // The passing host is still surfaced in the summary.
    assert!(
        combined.contains("ganoslal: PASSED"),
        "ganoslal should still show PASSED: {combined}"
    );
}

#[test]
fn verify_no_host_gates_is_a_clean_no_op() {
    let sb = roll_with_work();
    // No host gates configured (the plain fixture default) — no host section.
    let out = sb.rf(&["verify"]);
    assert!(out.success, "verify should pass: {}", out.combined());
    assert!(
        !out.combined().contains("Host verification"),
        "no host section should print without host gates: {}",
        out.combined()
    );
}

#[test]
fn promote_blocks_on_failing_host_and_force_bypasses() {
    let sb = Sandbox::nixos();
    assert!(sb.init().success, "init failed");
    assert!(sb.create_roll("feature", "0611").success, "create failed");
    sb.commit_file("work.txt", "w\n", "roll work");

    // Graduate first (host gates do NOT run on graduate).
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    // A failing host gate for merlin blocks promotion.
    sb.set_host_gates(&["test {host} != merlin"]);
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote"]);
    assert!(
        !out.success,
        "promote should block on a failing active host: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("host verification failed") && out.combined().contains("merlin"),
        "promote error should name merlin: {}",
        out.combined()
    );
    assert!(
        !sb.is_ancestor("rolling", "main"),
        "promotion must not have happened"
    );

    // `--force --reason` bypasses the failing host gate and records it.
    let out = sb.rf(&["promote", "--force", "--reason", "host offline"]);
    assert!(
        out.success,
        "forced promote should proceed: {}",
        out.combined()
    );
    assert!(
        sb.is_ancestor("rolling", "main"),
        "forced promotion should merge rolling into main"
    );

    let body = sb.git(&["log", "-1", "--format=%B", "main"]);
    assert!(
        body.contains("Forced-Bypass") && body.contains("merlin"),
        "merge trailer should record the bypassed host gate: {body}"
    );
    assert!(
        body.contains("Force-Reason: host offline"),
        "merge trailer should record the reason: {body}"
    );
}

#[test]
fn graduate_does_not_run_host_gates() {
    let sb = roll_with_work();
    // A gate that would fail every host; graduate must ignore it entirely.
    sb.set_host_gates(&["false"]);

    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduate must not run host gates: {}",
        out.combined()
    );
    assert!(
        !out.combined().contains("Host verification"),
        "graduate should print no host section: {}",
        out.combined()
    );
}
