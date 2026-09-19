//! Dependency and dependant reporting across the whole roll lifecycle.
//!
//! The regression these guard: `list_rolls` used to compute `deps` only for
//! rolls in state `Active`, so the moment a roll graduated its dependencies —
//! and, by extension, every dependant link pointing at it — silently vanished
//! from `rf status`. A second, quieter bug sat behind it: the dependency scan
//! was bounded by `<stable>..<roll>`, a range that goes empty once the roll is
//! promoted and its tip is contained in stable.
//!
//! So the same two assertions are made three times — active, graduated, and
//! promoted — because each state broke for a different reason.

mod harness;

use harness::Sandbox;

/// Reported `deps` for a roll branch, from `rf list --json`.
fn deps(sb: &Sandbox, branch: &str) -> Vec<u64> {
    numbers(sb, branch, "deps")
}

/// Reported `dependants` for a roll branch, from `rf list --json`.
fn dependants(sb: &Sandbox, branch: &str) -> Vec<u64> {
    numbers(sb, branch, "dependants")
}

fn numbers(sb: &Sandbox, branch: &str, field: &str) -> Vec<u64> {
    let json = sb.list_json();
    let roll = json
        .as_array()
        .expect("list --json is an array")
        .iter()
        .find(|r| r["branch"] == branch)
        .unwrap_or_else(|| panic!("roll {branch} not listed in {json}"));
    roll[field]
        .as_array()
        .unwrap_or_else(|| panic!("{branch} has no {field} array in {roll}"))
        .iter()
        .map(|n| n.as_u64().expect("roll number"))
        .collect()
}

/// Two rolls where the second integrates the first, so roll 2 depends on roll 1
/// and roll 1 has roll 2 as a dependant. Leaves HEAD on `rolling`.
fn sandbox_with_integration() -> Sandbox {
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");

    sb.git(&["checkout", "main"]);
    let out = sb.create_roll("beta", "0612");
    assert!(out.success, "create beta: {}", out.combined());
    sb.commit_file("beta.txt", "b\n", "beta work");

    // roll 2 pulls roll 1 in — the one dependency signal roll-flow trusts.
    let out = sb.rf(&["integrate", "roll/1-0611-alpha"]);
    assert!(out.success, "integrate alpha into beta: {}", out.combined());

    sb.git(&["checkout", "rolling"]);
    sb
}

#[test]
fn active_roll_reports_dependency_and_dependant() {
    let sb = sandbox_with_integration();

    assert_eq!(deps(&sb, "roll/2-0612-beta"), vec![1]);
    assert_eq!(dependants(&sb, "roll/1-0611-alpha"), vec![2]);

    // The relation is not symmetric: alpha integrated nothing, and nothing
    // integrated beta.
    assert!(deps(&sb, "roll/1-0611-alpha").is_empty());
    assert!(dependants(&sb, "roll/2-0612-beta").is_empty());
}

#[test]
fn dependency_and_dependant_survive_graduation() {
    let sb = sandbox_with_integration();

    sb.git(&["checkout", "roll/1-0611-alpha"]);
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");
    sb.git(&["checkout", "roll/2-0612-beta"]);
    assert!(sb.rf(&["graduate"]).success, "graduate beta");

    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ graduated")
    );
    // This is the reported bug: graduated rolls used to report nothing at all.
    assert_eq!(deps(&sb, "roll/2-0612-beta"), vec![1]);
    assert_eq!(dependants(&sb, "roll/1-0611-alpha"), vec![2]);
}

#[test]
fn dependency_and_dependant_survive_promotion() {
    let sb = sandbox_with_integration();

    sb.git(&["checkout", "roll/1-0611-alpha"]);
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");
    sb.git(&["checkout", "roll/2-0612-beta"]);
    assert!(sb.rf(&["graduate"]).success, "graduate beta");

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote: {}", out.combined());

    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ promoted")
    );
    // `<stable>..<roll>` is empty now that the roll is contained in stable, so
    // this only holds because the scan is anchored at the graduation merge.
    assert_eq!(deps(&sb, "roll/2-0612-beta"), vec![1]);
    assert_eq!(dependants(&sb, "roll/1-0611-alpha"), vec![2]);
}

#[test]
fn status_table_shows_both_columns_by_default() {
    let sb = sandbox_with_integration();

    let out = sb.rf(&["status", "--no-tui"]);
    assert!(out.success, "status: {}", out.combined());
    assert!(
        out.stdout.contains("deps") && out.stdout.contains("dependants"),
        "status should show both columns by default: {}",
        out.combined()
    );

    let out = sb.rf(&["status", "--no-tui", "--no-deps"]);
    assert!(out.success, "status --no-deps: {}", out.combined());
    assert!(
        !out.stdout.contains("dependants"),
        "--no-deps should hide the columns: {}",
        out.combined()
    );
}

// ── outdated ─────────────────────────────────────────────────────────────────

/// `outdated` for a roll branch, from `rf list --json`.
fn outdated(sb: &Sandbox, branch: &str) -> Vec<u64> {
    numbers(sb, branch, "outdated")
}

#[test]
fn a_dependency_that_moves_after_integration_marks_the_dependant_outdated() {
    let sb = sandbox_with_integration();
    assert!(
        outdated(&sb, "roll/2-0612-beta").is_empty(),
        "fresh integration"
    );

    // Real work lands on alpha after beta integrated it.
    sb.git(&["checkout", "roll/1-0611-alpha"]);
    sb.commit_file("alpha.txt", "a\nmore\n", "alpha follow-up");

    assert_eq!(outdated(&sb, "roll/2-0612-beta"), vec![1]);
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("⛔ blocked"),
        "the lifecycle state is untouched; outdated is reported beside it"
    );
    let out = sb.rf(&["status", "--no-tui"]);
    assert!(out.stdout.contains("⟳ ⛔ blocked"), "{}", out.combined());
    assert!(
        out.stdout.contains("⟳ a dependency has changed"),
        "{}",
        out.combined()
    );

    // Re-integrating catches up, and the marker clears.
    sb.git(&["checkout", "roll/2-0612-beta"]);
    let out = sb.rf(&["integrate", "roll/1-0611-alpha"]);
    assert!(out.success, "re-integrate: {}", out.combined());
    assert!(
        outdated(&sb, "roll/2-0612-beta").is_empty(),
        "after re-integration"
    );
}

#[test]
fn a_version_only_change_on_a_dependency_does_not_outdate_anyone() {
    // The dev-version marker and a bump touch the manifest and lockfile and
    // nothing else; a dependant has not fallen behind any work.
    let sb = Sandbox::cargo();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    sb.git(&["checkout", "main"]);
    sb.create_roll("beta", "0612");
    let out = sb.rf(&["integrate", "roll/1-0611-alpha"]);
    assert!(out.success, "integrate: {}", out.combined());

    sb.git(&["checkout", "roll/1-0611-alpha"]);
    sb.commit_cargo_version("0.0.2");

    assert!(
        outdated(&sb, "roll/2-0612-beta").is_empty(),
        "a bump alone must not read as outdated: {:?}",
        sb.list_json()
    );

    // But a bump *plus* real work does.
    sb.commit_file("alpha.txt", "a\nb\n", "alpha work 2");
    assert_eq!(outdated(&sb, "roll/2-0612-beta"), vec![1]);
}
