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

/// Integrating the rolling branch itself (`[I]` / `rf integrate rolling`) must
/// surface every roll already graduated onto it as a dependency, even though
/// the merge's own subject names rolling, not a roll — the gap this test
/// guards against is `rf integrate rolling` reporting no new dependencies at
/// all.
#[test]
fn integrating_rolling_reports_every_graduated_roll_as_a_dependency() {
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");

    sb.git(&["checkout", "main"]);
    sb.create_roll("beta", "0612");
    sb.commit_file("beta.txt", "b\n", "beta work");
    assert!(sb.rf(&["graduate"]).success, "graduate beta");

    sb.git(&["checkout", "main"]);
    sb.create_roll("gamma", "0613");
    sb.commit_file("gamma.txt", "c\n", "gamma work");

    // Bring rolling (with alpha and beta already graduated onto it) into
    // gamma directly, the way `[I]` does.
    let out = sb.rf(&["integrate", "rolling"]);
    assert!(
        out.success,
        "integrate rolling into gamma: {}",
        out.combined()
    );

    let mut got = deps(&sb, "roll/3-0613-gamma");
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
    assert_eq!(dependants(&sb, "roll/1-0611-alpha"), vec![3]);
    assert_eq!(dependants(&sb, "roll/2-0612-beta"), vec![3]);
}

/// The false positive the ancestry fallback must not reintroduce: a roll that
/// forks from stable *after* an old roll was promoted already has that roll's
/// commits as ancestors through stable, with no `[I]` merge involved at all.
/// A later, unrelated `[I]` merge on a *different* roll must not read that
/// pre-existing ancestry as a dependency it just acquired — only graduations
/// genuinely new to the merge's own `base..roll` range count.
#[test]
fn integrating_rolling_does_not_claim_a_dependency_already_baked_in_via_stable() {
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");
    sb.git(&["checkout", "rolling"]);
    assert!(sb.rf(&["promote"]).success, "promote alpha to stable");

    // beta forks from stable *after* the promotion, so alpha's commit is
    // already its ancestor — unrelated to anything beta merges from rolling.
    sb.git(&["checkout", "main"]);
    sb.create_roll("beta", "0612");
    sb.commit_file("beta.txt", "b\n", "beta work");

    let out = sb.rf(&["integrate", "rolling"]);
    assert!(
        out.success,
        "integrate rolling into beta: {}",
        out.combined()
    );

    assert!(
        deps(&sb, "roll/2-0612-beta").is_empty(),
        "beta's ancestry already contains alpha via stable; the [I] merge introduced nothing new"
    );
    assert!(dependants(&sb, "roll/1-0611-alpha").is_empty());
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
