//! Dependency cycles: two rolls that each integrated the other.
//!
//! Reproduces what happened to rolls 14 and 15 in this repo's own history. 14
//! was built on 15 (`rf integrate` of 15 into 14), and later 15 folded 14's
//! work in (`rf integrate` of 14 into 15). Each then depended on the other,
//! both read `⛔ blocked`, and neither could graduate without `--force`: the
//! chain planner refused the cycle and the state rule waited on each forever.
//!
//! The way out is containment. The ordering constraint exists so a
//! dependency's commits reach rolling before or *with* the roll that
//! integrated them; once 15 contains 14's tip, graduating 15 lands all of 14
//! too. So inside a cycle the member that contains every other member's tip is
//! the *carrier*: it graduates and takes the others with it, while the rest
//! stay blocked waiting on it. When no member contains the others, nothing can
//! land whole, and the refusal says exactly which `rf integrate` makes one.

mod harness;

use harness::Sandbox;

/// Plays roll 8 in the real case: an ordinary dependency both members share.
const BASE: &str = "roll/1-0918-base";
/// Plays roll 14: built on [`SHOW`], later folded into it.
const MENU: &str = "roll/2-0919-add-hotfix-to-menu";
/// Plays roll 15: folds [`MENU`] in, so it ends up containing it.
const SHOW: &str = "roll/3-0919-show-hotfixes";

/// The two-member cycle on its own, with [`SHOW`] (3) containing [`MENU`]'s
/// (2) tip. Numbered 2 and 3 so the shape matches 14/15, where the carrier is
/// the higher number. Leaves HEAD on `SHOW`.
fn cycle_pair() -> Sandbox {
    let sb = Sandbox::plain();
    sb.init();

    // Roll 1 exists but neither member integrates it here.
    assert!(sb.create_roll("base", "0918").success);
    sb.commit_file("base.txt", "base\n", "base work");
    sb.git(&["checkout", "main"]);

    assert!(sb.create_roll("add-hotfix-to-menu", "0919").success);
    sb.commit_file("menu.txt", "menu\n", "menu work");
    sb.git(&["checkout", "main"]);

    assert!(sb.create_roll("show-hotfixes", "0919").success);
    sb.commit_file("show.txt", "show\n", "show work");

    // 14 built on 15 …
    sb.git(&["checkout", MENU]);
    let out = sb.rf(&["integrate", SHOW]);
    assert!(out.success, "integrate show into menu: {}", out.combined());
    sb.commit_file("menu2.txt", "more menu\n", "menu work on top of show");

    // … then 15 folded 14 in.
    sb.git(&["checkout", SHOW]);
    let out = sb.rf(&["integrate", MENU]);
    assert!(out.success, "integrate menu into show: {}", out.combined());
    sb
}

/// The full real shape: both members also integrated a shared ungraduated
/// dependency ([`BASE`], roll 8 in the real case) before the cycle formed.
fn cycle_with_shared_dependency() -> Sandbox {
    let sb = Sandbox::plain();
    sb.init();

    assert!(sb.create_roll("base", "0918").success);
    sb.commit_file("base.txt", "base\n", "base work");
    sb.git(&["checkout", "main"]);

    assert!(sb.create_roll("add-hotfix-to-menu", "0919").success);
    sb.commit_file("menu.txt", "menu\n", "menu work");
    assert!(sb.rf(&["integrate", BASE]).success);
    sb.git(&["checkout", "main"]);

    assert!(sb.create_roll("show-hotfixes", "0919").success);
    sb.commit_file("show.txt", "show\n", "show work");
    assert!(sb.rf(&["integrate", BASE]).success);

    sb.git(&["checkout", MENU]);
    assert!(sb.rf(&["integrate", SHOW]).success);
    sb.git(&["checkout", SHOW]);
    assert!(sb.rf(&["integrate", MENU]).success);
    sb
}

/// The `rf list --json` row for `branch`.
fn row(sb: &Sandbox, branch: &str) -> serde_json::Value {
    sb.list_json()
        .as_array()
        .expect("list --json is an array")
        .iter()
        .find(|r| r["branch"] == branch)
        .cloned()
        .unwrap_or_else(|| panic!("{branch} not listed"))
}

/// Subjects of the graduation merges on rolling, oldest first.
fn graduations(sb: &Sandbox) -> Vec<String> {
    sb.git(&[
        "log",
        "--first-parent",
        "--merges",
        "--reverse",
        "--format=%s",
        "rolling",
    ])
    .lines()
    .filter(|l| l.starts_with("Graduate "))
    .map(str::to_string)
    .collect()
}

#[test]
fn the_member_that_contains_the_other_is_the_carrier_and_is_not_blocked() {
    // Before the fix both rows read `⛔ blocked`, each waiting on the other.
    let sb = cycle_pair();

    let menu = row(&sb, MENU);
    let show = row(&sb, SHOW);
    assert_eq!(menu["deps"], serde_json::json!([3]));
    assert_eq!(show["deps"], serde_json::json!([2]));

    // Both rows describe the same cycle, and name the same carrier.
    for r in [&menu, &show] {
        assert_eq!(r["cycle"]["members"], serde_json::json!([2, 3]), "{r}");
        assert_eq!(r["cycle"]["carrier"], 3, "{r}");
    }
    // The carrier graduates; the member it carries waits on it.
    assert_eq!(show["state"], "active", "{show}");
    assert_eq!(menu["state"], "⛔ blocked", "{menu}");
    // And the advice says which one, by branch name.
    let advice = show["cycle"]["advice"].as_str().unwrap();
    assert!(advice.contains(&format!("graduate {SHOW}")), "{advice}");
}

#[test]
fn graduating_the_carrier_lands_the_whole_cycle_in_one_merge() {
    // Before the fix: "dependency cycle among rolls 3 -> 2 -> 3".
    let sb = cycle_pair();

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate: {}", out.combined());

    // One structured merge — the carrier's — and both rolls are on rolling.
    assert_eq!(
        graduations(&sb),
        vec![format!("Graduate {SHOW} into rolling")]
    );
    assert_eq!(sb.roll_state(SHOW).as_deref(), Some("✓ graduated"));
    // The carried roll reads graduated through the carrier's integrate merge,
    // the non-first-parent pass of the graduated scan.
    assert_eq!(sb.roll_state(MENU).as_deref(), Some("✓ graduated"));
    assert!(sb.is_ancestor(MENU, "rolling"));
    // Said out loud, since it lands a roll that was not asked for by name.
    assert!(out.combined().contains("carries"), "{}", out.combined());
    // Graduated, the cycle is history and no longer reported.
    assert!(row(&sb, MENU)["cycle"].is_null());
}

#[test]
fn graduating_the_carried_member_plans_the_carrier_and_confirms_it() {
    // Asking for 14 graduates 15, which takes 14 with it. That merges a branch
    // other than the one checked out, so it is shown and confirmed first.
    let sb = cycle_pair();
    sb.git(&["checkout", MENU]);

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined().contains("Re-run with --yes"),
        "{}",
        out.combined()
    );
    assert!(graduations(&sb).is_empty(), "{:?}", graduations(&sb));

    let out = sb.rf(&["graduate", "--yes"]);
    assert!(out.success, "graduate --yes: {}", out.combined());
    let text = out.combined();
    assert!(
        text.contains(&format!("{SHOW}  (dependency of 2")),
        "{text}"
    );
    assert!(text.contains("carries 2"), "{text}");
    assert_eq!(
        graduations(&sb),
        vec![format!("Graduate {SHOW} into rolling")]
    );
    assert_eq!(sb.roll_state(MENU).as_deref(), Some("✓ graduated"));
    assert_eq!(sb.roll_state(SHOW).as_deref(), Some("✓ graduated"));
    assert_eq!(sb.current_branch(), MENU);
}

#[test]
fn the_real_14_15_shape_graduates_the_shared_dependency_then_the_carrier() {
    // 14: deps [8, 15]; 15: deps [8, 14]; 15 contains 14. The carrier still
    // waits on 8 — a dependency outside the cycle orders as it always did.
    let sb = cycle_with_shared_dependency();

    let menu = row(&sb, MENU);
    let show = row(&sb, SHOW);
    assert_eq!(menu["deps"], serde_json::json!([1, 3]));
    assert_eq!(show["deps"], serde_json::json!([1, 2]));
    assert_eq!(show["cycle"]["carrier"], 3, "{show}");
    assert_eq!(show["state"], "⛔ blocked", "blocked on roll 1: {show}");

    let out = sb.rf(&["graduate", "--yes"]);
    assert!(out.success, "graduate: {}", out.combined());
    assert_eq!(
        graduations(&sb),
        vec![
            format!("Graduate {BASE} into rolling"),
            format!("Graduate {SHOW} into rolling"),
        ]
    );
    for branch in [BASE, MENU, SHOW] {
        assert_eq!(
            sb.roll_state(branch).as_deref(),
            Some("✓ graduated"),
            "{branch}"
        );
    }
}

#[test]
fn when_no_member_contains_the_others_the_refusal_names_the_integrate_that_fixes_it() {
    // 14 gains work after 15 folded it in: now neither contains the other's
    // tip, so any graduation would land a partial copy of the other. Never a
    // deadlock, though — the advice is one `rf integrate` away from a carrier.
    let sb = cycle_pair();
    sb.git(&["checkout", MENU]);
    sb.commit_file("menu3.txt", "late\n", "menu work after the fold-in");
    sb.git(&["checkout", SHOW]);

    let show = row(&sb, SHOW);
    let menu = row(&sb, MENU);
    assert!(show["cycle"]["carrier"].is_null(), "{show}");
    assert_eq!(show["state"], "⛔ blocked");
    assert_eq!(menu["state"], "⛔ blocked");
    let advice = show["cycle"]["advice"].as_str().unwrap().to_string();
    assert!(advice.contains(&format!("rf integrate {MENU}")), "{advice}");
    assert!(advice.contains(SHOW), "{advice}");

    // Graduating either refuses before anything runs, with that same advice.
    for branch in [SHOW, MENU] {
        sb.git(&["checkout", branch]);
        let out = sb.rf(&["graduate", "--yes"]);
        assert!(!out.success, "{}", out.combined());
        assert!(
            out.combined().contains(&format!("rf integrate {MENU}")),
            "{}",
            out.combined()
        );
    }
    assert!(graduations(&sb).is_empty());

    // Following the advice makes 15 the carrier, and it graduates.
    sb.git(&["checkout", SHOW]);
    assert!(sb.rf(&["integrate", MENU]).success);
    assert_eq!(row(&sb, SHOW)["cycle"]["carrier"], 3);
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "{}", out.combined());
    assert_eq!(sb.roll_state(MENU).as_deref(), Some("✓ graduated"));
}

#[test]
fn the_plain_table_marks_cycle_deps_and_says_which_roll_to_graduate() {
    let sb = cycle_pair();

    let out = sb.rf(&["list", "--no-tui", "--deps"]);
    assert!(out.success, "{}", out.combined());
    let text = out.combined();
    // 15 moved on after 14 integrated it (it folded 14 in), so 14's copy is
    // also stale: both marks, staleness first. 15 has 14's latest.
    assert!(text.contains("3⚠↻"), "{text}");
    assert!(text.contains("2↻"), "{text}");
    assert!(text.contains(&format!("graduate {SHOW}")), "{text}");

    let out = sb.rf(&["status", "--no-tui"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined().contains(&format!("graduate {SHOW}")),
        "{}",
        out.combined()
    );
}

#[test]
fn a_carried_roll_promotes_at_the_carriers_graduation_merge() {
    // The carried roll's graduation commit is where it actually landed on
    // rolling's mainline — the carrier's graduation merge — not the integrate
    // merge inside the carrier's branch. Promoting it therefore advances
    // stable to a commit on rolling's first-parent line, never to one that
    // only lives on a roll branch.
    let sb = cycle_pair();
    assert!(sb.rf(&["graduate"]).success);
    let carrier_merge = sb.rev("rolling");
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--roll", MENU, "--yes"]);
    assert!(out.success, "promote: {}", out.combined());
    assert_eq!(sb.rev("main^2"), carrier_merge);
    assert_eq!(sb.roll_state(MENU).as_deref(), Some("✓ promoted"));
    assert_eq!(sb.roll_state(SHOW).as_deref(), Some("✓ promoted"));

    // Naming both is one merge, not a second one of an already-merged commit.
    let sb = cycle_pair();
    assert!(sb.rf(&["graduate"]).success);
    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote", "--roll", MENU, "--roll", SHOW, "--yes"]);
    assert!(out.success, "promote both: {}", out.combined());
    assert_eq!(sb.roll_state(MENU).as_deref(), Some("✓ promoted"));
    assert_eq!(sb.roll_state(SHOW).as_deref(), Some("✓ promoted"));
}
