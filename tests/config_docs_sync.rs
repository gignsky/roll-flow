//! Keeps `docs/config.md` honest about the config surface.
//!
//! The CLI has `docs_sync.rs`; the config file had nothing, which is how
//! `host_gates` went undocumented and `mode`, `host_active` and the release
//! flags drifted out of the reference. Every key `Config` knows must be named
//! in the key table of `docs/config.md`, and every key the table names must be
//! one the struct knows. The known-key list is `Config::KEYS`, asserted equal
//! to the struct's rendered keys by a unit test, so this cannot pass on a stale
//! list.

use std::process::Command;

/// `Config::KEYS`, read from the source so this test binary does not need to
/// link the crate. The list is a literal `&[...]` of string literals.
fn known_keys() -> Vec<String> {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/core/config.rs"))
        .expect("read config.rs");
    let start = src.find("pub const KEYS").expect("KEYS in config.rs");
    let body = &src[start..];
    let end = body.find("];").expect("KEYS terminator");
    body[..end]
        .split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, k)| k.to_string())
        .collect()
}

/// Keys named as `` `key` `` in the first column of the docs table.
fn documented_keys() -> Vec<String> {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/config.md"))
        .expect("read docs/config.md");
    doc.lines()
        .filter(|l| l.starts_with("| `"))
        .filter_map(|l| l.split('`').nth(1))
        .map(String::from)
        .collect()
}

#[test]
fn every_config_key_is_in_the_reference_table_and_vice_versa() {
    let mut known = known_keys();
    let mut documented = documented_keys();
    known.sort();
    documented.sort();
    assert!(!known.is_empty(), "no keys read from config.rs");
    assert_eq!(
        known, documented,
        "docs/config.md key table and Config::KEYS disagree\nknown:      {known:?}\ndocumented: {documented:?}"
    );
}

#[test]
fn the_documented_example_loads_without_warnings() {
    // The example block in docs/config.md is what people copy. It must parse
    // and earn no warning, checked by running the real binary against it.
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/config.md"))
        .expect("read docs/config.md");
    let example = doc
        .split("```toml")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("a toml example block");

    let dir = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.path().join(".roll-flow.toml"), example).expect("write example");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);

    let out = Command::new(env!("CARGO_BIN_EXE_rf"))
        .current_dir(dir.path())
        .args(["list", "--no-tui"])
        .output()
        .expect("run rf");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "example config failed to load: {stderr}"
    );
    assert!(
        !stderr.contains("warning"),
        "example config warns: {stderr}"
    );
}
