//! End-to-end lifecycle + divergence scenarios (issue #9), driven through the
//! shared [`harness::Sandbox`].
//!
//! These encode the merge-based semantics from `CLAUDE.md` and epic #2:
//! structured `--no-ff` graduation/promotion merges, divergence tolerance,
//! conflict abort-and-restore, and a real `status --json` reason on every
//! not-ready path.

mod harness;

use harness::Sandbox;

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

#[test]
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
fn graduation_tolerates_diverged_rolling() {
    // rolling advances independently after the roll is branched, so it is no
    // longer an ancestor of the roll — the --no-ff merge handles it.
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

#[test]
fn graduation_conflict_aborts_and_restores() {
    // A conflicting merge must be aborted cleanly: no half-merged state, no
    // MERGE_HEAD left behind, and we end up back on the roll branch.
    let sb = Sandbox::plain();
    sb.init();
    sb.git(&["checkout", "rolling"]);
    sb.git(&["checkout", "-b", "roll/1-0611-conflict"]);
    sb.commit_file("clash.txt", "roll side\n", "roll edit");

    sb.git(&["checkout", "rolling"]);
    sb.commit_file("clash.txt", "rolling side\n", "rolling edit");

    sb.git(&["checkout", "roll/1-0611-conflict"]);
    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "conflicting graduation should fail");
    assert!(
        out.combined().contains("aborted"),
        "error should mention the abort: {}",
        out.combined()
    );

    assert_eq!(sb.current_branch(), "roll/1-0611-conflict");
    // Only the untracked config may remain — no conflict/merge entries.
    let leftover: Vec<String> = sb
        .git(&["status", "--porcelain"])
        .lines()
        .filter(|l| !l.ends_with(".roll-flow.toml"))
        .map(|l| l.to_string())
        .collect();
    assert!(
        leftover.is_empty(),
        "working tree should be clean after abort: {leftover:?}"
    );
    let (merge_in_progress, _, _) = sb.git_try(&["rev-parse", "-q", "--verify", "MERGE_HEAD"]);
    assert!(
        !merge_in_progress,
        "MERGE_HEAD should not exist after abort"
    );
}

#[test]
fn multi_roll_promote_marks_all_promoted() {
    // One promotion merge can carry several graduated rolls; all of them must
    // be detected as promoted afterward.
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");
    assert!(sb.rf(&["graduate"]).success, "graduate alpha");

    sb.git(&["checkout", "rolling"]);
    let out = sb.create_roll("beta", "0612");
    assert!(out.success, "create beta: {}", out.combined());
    sb.commit_file("beta.txt", "b\n", "beta work");
    assert!(sb.rf(&["graduate"]).success, "graduate beta");

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(out.success, "promote: {}", out.combined());

    assert_eq!(sb.tip_subject("main"), "Promote rolling to main");
    assert_eq!(
        sb.roll_state("roll/1-0611-alpha").as_deref(),
        Some("✓ promoted")
    );
    assert_eq!(
        sb.roll_state("roll/2-0612-beta").as_deref(),
        Some("✓ promoted")
    );
}

#[test]
fn a_conflicting_graduation_names_the_roll_that_got_there_first() {
    // Roll A graduates touching F. Roll B, off stable, touches F differently.
    // Graduating B conflicts — and the diagnosis must say the other side came
    // in with A, so the user knows which roll to integrate.
    let sb = Sandbox::plain();
    sb.init();

    let out = sb.create_roll("first", "0611");
    assert!(out.success, "{}", out.combined());
    let a = sb.current_branch();
    sb.commit_file("clash.txt", "roll A's line\n", "A edits clash");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate A: {}", out.combined());

    let out = sb.create_roll("second", "0612");
    assert!(out.success, "{}", out.combined());
    let b = sb.current_branch();
    sb.commit_file("clash.txt", "roll B's line\n", "B edits clash");

    // Unattended: no terminal, no --yes — must report and change nothing.
    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "conflicting graduation should fail");
    let text = out.combined();
    assert!(text.contains("clash.txt"), "path not named: {text}");
    assert!(text.contains(&a), "culprit roll not named: {text}");
    assert!(text.contains("rf integrate"), "no by-hand commands: {text}");
    assert_eq!(sb.current_branch(), b);
    assert!(!sb.exists(".git/MERGE_HEAD"), "left mid-merge unattended");
    assert!(
        !sb.has_commit_subject("rolling", &format!("Graduate {b}")),
        "B must not have graduated"
    );

    // --yes takes the recommended way: integrate A into B, which reproduces
    // the conflict on B's own branch, where it is resolved and committed.
    let out = sb.rf(&["graduate", "--yes"]);
    assert!(
        !out.success,
        "integrate should stop on the conflict: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("mid-merge"),
        "should say B is mid-merge: {}",
        out.combined()
    );
    assert_eq!(sb.current_branch(), b);
    assert!(
        sb.exists(".git/MERGE_HEAD"),
        "the conflict should now be on B"
    );

    sb.write("clash.txt", "resolved\n");
    sb.git(&["add", "clash.txt"]);
    sb.git(&["commit", "--no-edit"]);
    assert!(
        sb.has_commit_subject(&b, &format!("Merge branch '{a}'")),
        "B should now carry A as an integrate merge"
    );

    // With the conflict resolved on B, graduation goes through.
    let out = sb.rf(&["graduate"]);
    assert!(
        out.success,
        "graduate B after resolving: {}",
        out.combined()
    );
}

#[test]
fn a_conflict_with_a_direct_edit_on_rolling_offers_nothing_to_integrate() {
    // Not every conflict has a roll behind it. A change made on rolling
    // directly is reported as exactly that, and --yes then falls back to the
    // by-hand commands rather than integrating an unrelated roll.
    let sb = Sandbox::plain();
    sb.init();

    let out = sb.create_roll("first", "0611");
    assert!(out.success, "{}", out.combined());
    let a = sb.current_branch();
    sb.commit_file("clash.txt", "line 1\nline 2\nline 3\n", "A adds clash");
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate A: {}", out.combined());

    // A is on rolling but never touched other.txt; B and a direct commit on
    // rolling both add it. The attribution for other.txt must therefore name
    // no roll at all — A is a bystander.
    let out = sb.create_roll("second", "0612");
    assert!(out.success, "{}", out.combined());
    let b = sb.current_branch();
    sb.commit_file("other.txt", "B's version\n", "B adds other");
    sb.git(&["checkout", "rolling"]);
    sb.commit_file(
        "other.txt",
        "rolling's version\n",
        "rolling edits other directly",
    );
    sb.git(&["checkout", &b]);

    let out = sb.rf(&["graduate", "--yes"]);
    assert!(!out.success, "{}", out.combined());
    let text = out.combined();
    assert!(text.contains("other.txt"), "{text}");
    assert!(
        text.contains("made there directly") || text.contains("directly"),
        "should say no roll is attributable: {text}"
    );
    // Nothing to integrate, so --yes falls back to reporting; A is unrelated.
    assert!(!text.contains(&format!("integrating {a}")), "{text}");
    assert!(!sb.exists(".git/MERGE_HEAD"));
    assert_eq!(sb.current_branch(), b);
}
