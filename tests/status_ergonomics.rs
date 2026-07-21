//! `rf status` ergonomics (issues #99/#100).
//!
//! Covers the bare-`rf` = `rf status` behavior (#100) and that clap still
//! intercepts `--help`/`--version` once the subcommand is optional. The
//! spacebar-switch behavior (#99) is an interactive TUI action and is not
//! driven here; its pure pieces (`git::parse_ahead_behind`,
//! `tui::rolls::format_ahead_behind`) are unit-tested in-crate.

mod harness;

use harness::Sandbox;

/// With no subcommand, `rf` runs the status dashboard. In the test harness
/// stdout is not a terminal, so both bare `rf` and `rf status` fall through to
/// the plain-text renderer and must produce identical output.
#[test]
fn bare_rf_matches_rf_status() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");
    assert!(sb.create_roll("feature", "0611").success, "create failed");

    let bare = sb.rf(&[]);
    assert!(bare.success, "bare rf failed: {}", bare.combined());

    let status = sb.rf(&["status"]);
    assert!(status.success, "rf status failed: {}", status.combined());

    // Same plain-text status output.
    assert_eq!(
        bare.stdout, status.stdout,
        "bare `rf` output should match `rf status`"
    );
    // Sanity: it really is the status view, including the created roll.
    assert!(
        bare.stdout.contains("Roll Flow Status"),
        "expected status header, got: {}",
        bare.stdout
    );
    assert!(
        bare.stdout.contains("roll/1-0611-feature"),
        "expected the roll row, got: {}",
        bare.stdout
    );
}

/// `--help` is still intercepted by clap before subcommand resolution: exits 0
/// and prints usage listing the subcommands.
#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let sb = Sandbox::plain();
    let out = sb.rf(&["--help"]);
    assert!(out.success, "--help should exit 0: {}", out.combined());
    let text = out.combined();
    // Usage banner plus a couple of known subcommands from the list.
    assert!(
        text.contains("Usage"),
        "expected a usage banner, got: {text}"
    );
    assert!(
        text.contains("status") && text.contains("graduate"),
        "expected the subcommand list, got: {text}"
    );
}

/// `--version` is likewise intercepted by clap and prints the crate version.
#[test]
fn version_flag_works() {
    let sb = Sandbox::plain();
    let out = sb.rf(&["--version"]);
    assert!(out.success, "--version should exit 0: {}", out.combined());
    assert!(
        out.combined().contains(env!("CARGO_PKG_VERSION")),
        "expected the version string, got: {}",
        out.combined()
    );
}
