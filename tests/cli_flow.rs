use std::fs;
use std::path::Path;
use std::process::Command;

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "test@example.com"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
    git(dir.path(), &["add", "README.md"]);
    git(dir.path(), &["commit", "-m", "init"]);
    dir
}

fn rf(repo: &Path, args: &[&str]) -> (bool, String) {
    let exe = std::env::var("CARGO_BIN_EXE_rf").expect("binary path");
    let output = Command::new(exe)
        .current_dir(repo)
        .args(args)
        .output()
        .expect("run rf");
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
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

#[test]
fn init_create_and_promote_flow() {
    let repo = init_repo();

    let (ok, out) = rf(repo.path(), &["init"]);
    assert!(ok, "init failed: {out}");

    let (ok, out) = rf(repo.path(), &["create", "feature", "--date", "0611"]);
    assert!(ok, "create failed: {out}");

    fs::write(repo.path().join("work.txt"), "w\n").expect("write work");
    git(repo.path(), &["add", "work.txt"]);
    git(repo.path(), &["commit", "-m", "roll work"]);

    let (ok, out) = rf(repo.path(), &["verify"]);
    assert!(ok, "verify failed: {out}");

    let (ok, out) = rf(repo.path(), &["promote"]);
    assert!(ok, "promote roll->rolling failed: {out}");

    let roll_sha = git(repo.path(), &["rev-parse", "HEAD"]);
    let rolling_sha = git(repo.path(), &["rev-parse", "rolling"]);
    assert_eq!(roll_sha, rolling_sha);

    git(repo.path(), &["checkout", "rolling"]);
    let (ok, out) = rf(repo.path(), &["promote"]);
    assert!(ok, "promote rolling->main failed: {out}");

    let main_sha = git(repo.path(), &["rev-parse", "main"]);
    let rolling_sha = git(repo.path(), &["rev-parse", "rolling"]);
    assert_eq!(main_sha, rolling_sha);
}

#[test]
fn promote_requires_clean_tree() {
    let repo = init_repo();
    let (ok, out) = rf(repo.path(), &["init"]);
    assert!(ok, "init failed: {out}");

    let (ok, out) = rf(repo.path(), &["create", "dirty", "--date", "0611"]);
    assert!(ok, "create failed: {out}");

    fs::write(repo.path().join("dirty.txt"), "x\n").expect("write dirty");
    let (ok, out) = rf(repo.path(), &["promote"]);
    assert!(!ok, "promote should fail on dirty tree");
    assert!(
        out.contains("working tree must be clean"),
        "unexpected: {out}"
    );
}

#[test]
fn integrate_merges_branch_into_roll() {
    let repo = init_repo();

    let (ok, out) = rf(repo.path(), &["init"]);
    assert!(ok, "init: {out}");

    // create a feature branch with unique content
    git(repo.path(), &["checkout", "-b", "feature/foo"]);
    fs::write(repo.path().join("foo.txt"), "feature content\n").unwrap();
    git(repo.path(), &["add", "foo.txt"]);
    git(repo.path(), &["commit", "-m", "add foo feature"]);

    // create a roll (rf create checks out the new roll branch)
    let (ok, out) = rf(repo.path(), &["create", "myroll", "--date", "0101"]);
    assert!(ok, "create: {out}");

    // integrate the feature branch into the roll
    let (ok, out) = rf(repo.path(), &["integrate", "feature/foo"]);
    assert!(ok, "integrate: {out}");

    // feature content should now exist on the roll branch
    assert!(
        repo.path().join("foo.txt").exists(),
        "foo.txt missing after integrate"
    );

    // should have been a --no-ff merge commit
    let log = git(repo.path(), &["log", "--oneline", "-1"]);
    assert!(
        log.contains("Merge branch"),
        "expected merge commit, got: {log}"
    );
}

#[test]
fn promote_blocks_divergence() {
    let repo = init_repo();
    let (ok, out) = rf(repo.path(), &["init"]);
    assert!(ok, "init failed: {out}");

    git(repo.path(), &["checkout", "rolling"]);
    git(repo.path(), &["checkout", "-b", "roll/1-0611-diverge"]);
    fs::write(repo.path().join("roll.txt"), "roll\n").expect("write roll");
    git(repo.path(), &["add", "roll.txt"]);
    git(repo.path(), &["commit", "-m", "roll commit"]);

    git(repo.path(), &["checkout", "rolling"]);
    fs::write(repo.path().join("rolling.txt"), "rolling\n").expect("write rolling");
    git(repo.path(), &["add", "rolling.txt"]);
    git(repo.path(), &["commit", "-m", "rolling diverges"]);

    git(repo.path(), &["checkout", "roll/1-0611-diverge"]);
    let (ok, out) = rf(repo.path(), &["promote"]);
    assert!(!ok, "promote should fail when branches diverged");
    assert!(out.contains("fast-forward-only"), "unexpected: {out}");
}
