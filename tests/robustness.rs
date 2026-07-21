//! Robustness / error-handling sweep for messy-but-valid states (issue #32).
//!
//! Covers the three concrete failure modes the first release must handle
//! gracefully — no panics, a clear message, a non-zero exit:
//!  (a) running a command outside any git repository,
//!  (b) running a command before `rf init` (no `.roll-flow.toml`),
//!  (c) graduating when the target branch exists only as `origin/<target>`.

mod harness;

use std::path::Path;
use std::process::Command;

use harness::{RfOutput, Sandbox};

/// Run `rf` in an arbitrary directory (not necessarily a git repo).
fn rf_in(dir: &Path, args: &[&str]) -> RfOutput {
    let exe = std::env::var("CARGO_BIN_EXE_rf").expect("CARGO_BIN_EXE_rf");
    let output = Command::new(exe)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run rf");
    RfOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

// ── (a) outside a git repository ──────────────────────────────────────────────

#[test]
fn status_outside_git_repo_fails_cleanly() {
    // A plain temp dir that is NOT a git repo.
    let dir = tempfile::tempdir().expect("temp dir");
    let out = rf_in(dir.path(), &["status"]);
    assert!(!out.success, "expected non-zero exit outside a git repo");
    assert_eq!(out.code, Some(1), "expected exit code 1");
    assert!(
        out.combined().contains("not inside a git repository"),
        "expected a clear 'not inside a git repository' message, got: {}",
        out.combined()
    );
    // The error belongs on stderr, not stdout.
    assert!(
        out.stdout.trim().is_empty(),
        "error must not be printed to stdout: {:?}",
        out.stdout
    );
}

#[test]
fn list_outside_git_repo_fails_cleanly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let out = rf_in(dir.path(), &["list", "--no-tui"]);
    assert!(!out.success, "expected non-zero exit outside a git repo");
    assert!(
        out.combined().contains("not inside a git repository"),
        "expected a clear message, got: {}",
        out.combined()
    );
}

// ── (b) before `rf init` (missing config) ─────────────────────────────────────

#[test]
fn status_before_init_reports_missing_config() {
    // A git repo, but `rf init` has not been run: no `.roll-flow.toml`.
    let sb = Sandbox::plain();
    let out = sb.rf(&["status"]);
    assert!(!out.success, "expected non-zero exit before init");
    assert_eq!(out.code, Some(1), "expected exit code 1");
    let msg = out.combined();
    assert!(
        msg.contains("no roll-flow config found"),
        "expected a missing-config message, got: {msg}"
    );
    assert!(
        msg.contains("rf init"),
        "message should tell the user to run `rf init`, got: {msg}"
    );
}

#[test]
fn list_before_init_reports_missing_config() {
    let sb = Sandbox::plain();
    let out = sb.rf(&["list", "--no-tui"]);
    assert!(!out.success, "expected non-zero exit before init");
    assert!(
        out.combined().contains("no roll-flow config found"),
        "got: {}",
        out.combined()
    );
}

#[test]
fn create_before_init_reports_missing_config() {
    let sb = Sandbox::plain();
    let out = sb.rf(&["create", "feature", "--date", "0611"]);
    assert!(!out.success, "expected non-zero exit before init");
    assert!(
        out.combined().contains("no roll-flow config found"),
        "got: {}",
        out.combined()
    );
}

// ── (c) target branch only on origin/<target> ─────────────────────────────────

#[test]
fn graduate_creates_local_target_from_origin() {
    // Set up a sandbox with a real `origin` remote, then delete the local
    // rolling branch so it exists only as `origin/rolling`. `rf graduate` must
    // recreate it from the remote and merge — the bug from issue #32.
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init failed");

    // A bare repo to act as origin.
    let origin = tempfile::tempdir().expect("origin dir");
    let origin_path = origin.path().to_str().unwrap();
    Command::new("git")
        .args(["init", "--bare", "-b", "main", origin_path])
        .output()
        .expect("init bare");

    sb.git(&["remote", "add", "origin", origin_path]);
    sb.git(&["push", "origin", "main", "rolling"]);
    sb.git(&["fetch", "origin"]);

    // Create a roll with a commit rolling doesn't have.
    assert!(sb.create_roll("feature", "0611").success, "create failed");
    let roll = sb.current_branch();
    assert!(roll.starts_with("roll/"), "expected to be on a roll branch");
    sb.commit_file("feature.txt", "work\n", "feat: work");

    // Remove the local rolling branch: it now lives only on origin.
    sb.git(&["branch", "-D", "rolling"]);
    assert!(
        !sb.branch_exists("rolling"),
        "precondition: local rolling should be gone"
    );

    // Graduate: should auto-create local rolling from origin/rolling and merge.
    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduate should recreate the local target and succeed: {}",
        out.combined()
    );
    assert!(
        sb.branch_exists("rolling"),
        "graduate should have created a local rolling branch"
    );
    assert!(
        sb.has_commit_subject("rolling", &format!("Graduate {roll}")),
        "rolling should carry the graduation merge"
    );
}
