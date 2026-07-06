//! End-to-end lifecycle + divergence scenarios (issue #9), driven through the
//! shared [`harness::Sandbox`].
//!
//! Two kinds of tests live here:
//!
//! * **Active** tests exercise the pipeline as it behaves today and gate every
//!   PR via CI.
//! * **`#[ignore]`d** tests encode the *correct* post-fix semantics from
//!   `CLAUDE.md` (structured `--no-ff` merges, divergence tolerance, a real
//!   `status --json` reason). They fail against today's fast-forward-only
//!   promotion and are un-ignored by epic #2 as each piece lands — the `ignore`
//!   reason names the issue that unblocks them.

mod harness;

use harness::Sandbox;

// ── active coverage (passes today) ────────────────────────────────────────

#[test]
fn happy_path_content_reaches_main() {
    let sb = Sandbox::plain();
    assert!(sb.init().success, "init");
    assert!(sb.create_roll("feature", "0611").success, "create");
    sb.commit_file("work.txt", "w\n", "roll work");

    assert!(sb.rf(&["promote"]).success, "graduate roll->rolling");
    sb.git(&["checkout", "rolling"]);
    assert!(sb.rf(&["promote"]).success, "promote rolling->main");

    // The roll's content is now reachable on the stable branch.
    assert_eq!(sb.git(&["show", "main:work.txt"]), "w");
}

#[test]
fn list_and_status_json_contract() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("alpha", "0101");

    let list = sb.list_json();
    let rolls = list.as_array().expect("list --json is an array");
    assert_eq!(rolls.len(), 1, "one roll expected: {list}");
    assert_eq!(rolls[0]["branch"], "roll/1-0101-alpha");
    assert_eq!(rolls[0]["number"].as_u64(), Some(1));
    assert_eq!(rolls[0]["state"], "active");

    let st = sb.status_json();
    assert_eq!(st["current_branch"], "roll/1-0101-alpha");
    assert_eq!(st["tier"], "roll");
    assert_eq!(st["clean_working_tree"].as_bool(), Some(true));
}

#[test]
fn status_json_reason_present_on_main() {
    // On the stable branch nothing is promotable — and `rf` must say *why*
    // rather than leaving `reason` null.
    let sb = Sandbox::plain();
    sb.init();

    let st = sb.status_json();
    assert_eq!(st["current_branch"], "main");
    assert_eq!(st["promotion"]["ready"].as_bool(), Some(false));
    assert!(
        st["promotion"]["reason"].is_string(),
        "reason should explain non-promotability, got {}",
        st["promotion"]["reason"]
    );
}

#[test]
fn listing_tolerates_generation_bump_commits() {
    // Auto-generated commits on a roll must not break listing (quasi-roll
    // *filtering* is epic #6; here we only assert robustness).
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    sb.add_generation_bump("roll/1-0611-feature", "ganoslal", 42);

    let list = sb.list_json();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|r| r["branch"] == "roll/1-0611-feature"),
        "roll should still be listed: {list}"
    );
}

// ── forward-looking coverage (un-ignored by epic #2) ──────────────────────

#[test]
#[ignore = "requires epic #2 / issue #11: --no-ff graduation with structured subject"]
fn graduate_creates_noff_merge_with_subject() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    let roll_tip = sb.rev("HEAD");

    assert!(sb.rf(&["graduate"]).success, "graduate");

    assert!(
        sb.tip_is_merge("rolling"),
        "rolling tip should be a --no-ff merge"
    );
    assert!(
        sb.tip_subject("rolling").starts_with("Graduate roll/1"),
        "unexpected graduation subject: {}",
        sb.tip_subject("rolling")
    );
    // Second parent of the merge is the roll tip, so `check_diverged`'s `^2` works.
    assert_eq!(sb.rev("rolling^2"), roll_tip);
}

#[test]
#[ignore = "requires epic #2 / issue #11: rolling->main promotion writes a 'Promote …' subject"]
fn promote_writes_promote_subject_on_main() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    sb.git(&["checkout", "rolling"]);
    assert!(sb.rf(&["promote"]).success, "promote");

    assert!(
        sb.has_commit_subject("main", "Promote"),
        "main should carry a 'Promote …' subject: {}",
        sb.tip_subject("main")
    );
}

#[test]
#[ignore = "requires epic #2 / issue #12: divergence-tolerant graduation"]
fn graduation_tolerates_diverged_rolling() {
    // rolling advances independently after the roll is branched, so it is no
    // longer an ancestor of the roll — today's fast-forward-only path bails.
    let sb = Sandbox::plain();
    sb.init();
    sb.git(&["checkout", "rolling"]);
    sb.git(&["checkout", "-b", "roll/1-0611-diverge"]);
    sb.commit_file("roll.txt", "roll\n", "roll commit");

    sb.git(&["checkout", "rolling"]);
    sb.commit_file("rolling.txt", "rolling\n", "rolling diverges");

    sb.git(&["checkout", "roll/1-0611-diverge"]);
    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduation should tolerate a diverged rolling: {}",
        out.combined()
    );
    assert!(
        sb.tip_is_merge("rolling"),
        "expected a --no-ff merge commit"
    );
}

#[test]
#[ignore = "requires epic #2 / issue #13: status --json explains a non-promotable diverged roll"]
fn status_json_reason_on_diverged_roll() {
    let sb = Sandbox::plain();
    sb.init();
    sb.git(&["checkout", "rolling"]);
    sb.git(&["checkout", "-b", "roll/1-0611-diverge"]);
    sb.commit_file("roll.txt", "roll\n", "roll commit");
    sb.git(&["checkout", "rolling"]);
    sb.commit_file("rolling.txt", "rolling\n", "rolling diverges");
    sb.git(&["checkout", "roll/1-0611-diverge"]);

    let st = sb.status_json();
    assert_eq!(st["clean_working_tree"].as_bool(), Some(true));
    assert_eq!(st["promotion"]["ready"].as_bool(), Some(false));
    assert!(
        st["promotion"]["reason"].is_string(),
        "a diverged roll should have a reason, got {}",
        st["promotion"]["reason"]
    );
}

#[test]
#[ignore = "requires epic #2: state detection sees the --no-ff graduation merge"]
fn roll_reports_graduated_after_graduate() {
    let sb = Sandbox::plain();
    sb.init();
    sb.create_roll("feature", "0611");
    sb.commit_file("work.txt", "w\n", "roll work");
    assert!(sb.rf(&["graduate"]).success, "graduate");

    assert_eq!(
        sb.roll_state("roll/1-0611-feature").as_deref(),
        Some("✓ graduated")
    );
}
