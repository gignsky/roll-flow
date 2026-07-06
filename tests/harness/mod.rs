//! Sandboxed test-repo harness for driving `rf` end-to-end (issue #8).
//!
//! A [`Sandbox`] is a throwaway git repo in a temp dir with helpers to run
//! `rf`/`git` against it and assert on the resulting git state. It supersedes
//! the ad-hoc `init_repo`/`rf`/`git` helpers that used to live inline in
//! `tests/cli_flow.rs`, and is shared across every integration-test binary via
//! `mod harness;`.
//!
//! Not every helper is exercised by every test binary, so the module carries a
//! blanket `dead_code` allow — Rust type-checks each test file as its own crate
//! and would otherwise warn about the helpers that file happens not to touch.
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Result of running the `rf` binary once.
pub struct RfOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RfOutput {
    /// stdout followed by stderr — convenient for `assert!(out.contains(..))`
    /// against user-facing messages regardless of which stream they land on.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// A throwaway git repository wired up to run `rf`.
pub struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    /// A plain (non-Nix) repo: `main` with a single initial commit. Models a
    /// normal software project — the "non-NixOS repo" configuration path.
    pub fn plain() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let sb = Sandbox { dir };
        sb.git(&["init", "-b", "main"]);
        sb.git(&["config", "user.email", "test@example.com"]);
        sb.git(&["config", "user.name", "roll-flow tests"]);
        sb.write("README.md", "hello\n");
        sb.git(&["add", "README.md"]);
        sb.git(&["commit", "-m", "init"]);
        sb
    }

    /// A NixOS-flavored fixture: a plain repo plus a minimal `flake.nix` and
    /// `vars/hosts.nix` so host/username auto-detection has something to read.
    pub fn nixos() -> Self {
        let sb = Sandbox::plain();
        sb.write("flake.nix", "{\n  description = \"test\";\n}\n");
        sb.write(
            "vars/hosts.nix",
            "{\n  hosts = [ \"ganoslal\" \"merlin\" \"wsl\" ];\n  \
             host_active = { ganoslal = true; merlin = true; wsl = false; };\n  \
             username = \"gig\";\n}\n",
        );
        sb.git(&["add", "flake.nix", "vars/hosts.nix"]);
        sb.git(&["commit", "-m", "nixos fixture"]);
        sb
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    // ── running rf / git ──────────────────────────────────────────────────

    /// Run `rf` against the sandbox, capturing stdout/stderr/exit code.
    pub fn rf(&self, args: &[&str]) -> RfOutput {
        let exe = std::env::var("CARGO_BIN_EXE_rf").expect("CARGO_BIN_EXE_rf");
        let output = Command::new(exe)
            .current_dir(self.path())
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

    /// Run `git`, asserting success, returning trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(self.path())
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Run `git` without asserting: `(success, stdout, stderr)`.
    pub fn git_try(&self, args: &[&str]) -> (bool, String, String) {
        let output = Command::new("git")
            .current_dir(self.path())
            .args(args)
            .output()
            .expect("run git");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        )
    }

    // ── file / commit helpers ─────────────────────────────────────────────

    /// Write a file (relative to the repo root), creating parent dirs.
    pub fn write(&self, rel: &str, contents: &str) {
        let full = self.path().join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(full, contents).expect("write file");
    }

    /// True if `rel` exists in the working tree.
    pub fn exists(&self, rel: &str) -> bool {
        self.path().join(rel).exists()
    }

    /// Write, stage, and commit a single file in one shot.
    pub fn commit_file(&self, rel: &str, contents: &str, msg: &str) {
        self.write(rel, contents);
        self.git(&["add", rel]);
        self.git(&["commit", "-m", msg]);
    }

    /// Commit an empty commit with an explicit subject (handy for building
    /// history like generation-bump commits).
    pub fn commit_empty(&self, msg: &str) {
        self.git(&["commit", "--allow-empty", "-m", msg]);
    }

    // ── convenience workflow steps ────────────────────────────────────────

    /// `rf init`.
    pub fn init(&self) -> RfOutput {
        self.rf(&["init"])
    }

    /// `rf create <slug> --date <mmdd>` — leaves HEAD on the new roll branch.
    pub fn create_roll(&self, slug: &str, date: &str) -> RfOutput {
        self.rf(&["create", slug, "--date", date])
    }

    // ── assertions on git state ───────────────────────────────────────────

    /// True if a local branch named `name` exists.
    pub fn branch_exists(&self, name: &str) -> bool {
        self.git_try(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .0
    }

    /// True if `ancestor` is an ancestor of `descendant`.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        self.git_try(&["merge-base", "--is-ancestor", ancestor, descendant])
            .0
    }

    /// Name of the currently checked-out branch.
    pub fn current_branch(&self) -> String {
        self.git(&["rev-parse", "--abbrev-ref", "HEAD"])
    }

    /// Full SHA of a ref.
    pub fn rev(&self, refspec: &str) -> String {
        self.git(&["rev-parse", refspec])
    }

    /// True if any commit reachable from `branch` has a subject containing
    /// `needle`. Used to assert on the structured `Graduate …` / `Promote …`
    /// merge subjects the state detector reads.
    pub fn has_commit_subject(&self, branch: &str, needle: &str) -> bool {
        self.git(&["log", "--format=%s", branch])
            .lines()
            .any(|s| s.contains(needle))
    }

    /// Subject line of the tip commit of `refspec`.
    pub fn tip_subject(&self, refspec: &str) -> String {
        self.git(&["log", "-1", "--format=%s", refspec])
    }

    /// True if the tip commit of `refspec` is a merge commit (2+ parents).
    pub fn tip_is_merge(&self, refspec: &str) -> bool {
        // `%P` is the space-separated parent list.
        self.git(&["log", "-1", "--format=%P", refspec])
            .split_whitespace()
            .count()
            >= 2
    }

    // ── assertions via rf's own JSON output ───────────────────────────────

    /// Parsed `rf list --json`.
    pub fn list_json(&self) -> serde_json::Value {
        let out = self.rf(&["list", "--json"]);
        assert!(out.success, "list --json failed: {}", out.combined());
        serde_json::from_str(&out.stdout).expect("parse list --json")
    }

    /// Parsed `rf status --json`.
    pub fn status_json(&self) -> serde_json::Value {
        let out = self.rf(&["status", "--json"]);
        assert!(out.success, "status --json failed: {}", out.combined());
        serde_json::from_str(&out.stdout).expect("parse status --json")
    }

    /// Reported state label for a roll branch (e.g. `"active"`,
    /// `"✓ graduated"`, `"✓ promoted"`), or `None` if `rf` doesn't list it.
    pub fn roll_state(&self, branch: &str) -> Option<String> {
        self.list_json()
            .as_array()?
            .iter()
            .find(|r| r["branch"] == branch)
            .and_then(|r| r["state"].as_str().map(str::to_string))
    }

    // ── messy-but-valid state builders ────────────────────────────────────

    /// Merge the stable branch into `roll_branch` with `--no-ff`, creating the
    /// divergence that used to make `rf promote` (fast-forward-only) bail:
    /// `main` is no longer an ancestor of the roll. Leaves HEAD on `roll_branch`.
    pub fn merge_stable_into_roll(&self, stable: &str, roll_branch: &str) {
        let here = self.current_branch();
        self.git(&["checkout", stable]);
        self.commit_file("stable-only.txt", "stable moved\n", "advance stable");
        self.git(&["checkout", roll_branch]);
        self.git(&[
            "merge",
            "--no-ff",
            "-m",
            &format!("Merge {stable} into roll"),
            stable,
        ]);
        if here != roll_branch {
            self.git(&["checkout", &here]);
        }
    }

    /// Append an auto-generated-looking commit (e.g. a NixOS generation bump)
    /// to `branch`, of the kind quasi-roll detection is expected to filter out.
    pub fn add_generation_bump(&self, branch: &str, host: &str, generation: u32) {
        let here = self.current_branch();
        self.git(&["checkout", branch]);
        self.commit_empty(&format!("{host}: generation {generation}"));
        if here != branch {
            self.git(&["checkout", &here]);
        }
    }
}
