//! Dependency-ordered graduation and promotion.
//!
//! A roll that integrated another cannot graduate until that one has — the
//! `⛔ blocked` gate. Rather than sending the user off to graduate the
//! dependency by hand, `rf graduate` walks the chain: dependencies first, each
//! its own `--no-ff` merge behind its own gate run, then the roll asked for.
//! The plan is shown and confirmed because it merges more than the branch
//! the user is standing on.

mod harness;

use harness::Sandbox;

const ALPHA: &str = "roll/1-0611-alpha";
const BETA: &str = "roll/2-0612-beta";

/// alpha with work, beta with work that integrates alpha. Leaves HEAD on beta,
/// which is therefore `⛔ blocked` on alpha.
fn blocked_pair() -> Sandbox {
    let sb = Sandbox::plain();
    sb.init();

    sb.create_roll("alpha", "0611");
    sb.commit_file("alpha.txt", "a\n", "alpha work");

    sb.git(&["checkout", "main"]);
    let out = sb.create_roll("beta", "0612");
    assert!(out.success, "create beta: {}", out.combined());
    sb.commit_file("beta.txt", "b\n", "beta work");
    let out = sb.rf(&["integrate", ALPHA]);
    assert!(out.success, "integrate: {}", out.combined());

    assert_eq!(sb.roll_state(BETA).as_deref(), Some("⛔ blocked"));
    sb
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
fn graduating_a_blocked_roll_graduates_its_dependency_first() {
    let sb = blocked_pair();

    let out = sb.rf(&["graduate", "--yes"]);
    assert!(out.success, "graduate: {}", out.combined());

    // Both landed, alpha before beta, each as its own structured merge.
    assert_eq!(
        graduations(&sb),
        vec![
            format!("Graduate {ALPHA} into rolling"),
            format!("Graduate {BETA} into rolling"),
        ]
    );
    assert_eq!(sb.roll_state(ALPHA).as_deref(), Some("✓ graduated"));
    assert_eq!(sb.roll_state(BETA).as_deref(), Some("✓ graduated"));
    // The plan was announced, with the dependency named as such.
    assert!(
        out.combined().contains("dependency of 2"),
        "{}",
        out.combined()
    );
    // And the user is back where they started.
    assert_eq!(sb.current_branch(), BETA);
}

#[test]
fn a_chain_is_shown_and_not_run_when_nobody_can_confirm_it() {
    // The same unattended rule every other confirmation has: no tty and no
    // `--yes` means the plan is printed and nothing is merged, exit 0.
    let sb = blocked_pair();

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined().contains("Re-run with --yes"),
        "{}",
        out.combined()
    );
    assert!(graduations(&sb).is_empty(), "{:?}", graduations(&sb));
    assert_eq!(sb.roll_state(BETA).as_deref(), Some("⛔ blocked"));
}

#[test]
fn a_dry_run_previews_every_step_and_merges_nothing() {
    let sb = blocked_pair();

    let out = sb.rf(&["graduate", "--dry-run"]);
    assert!(out.success, "{}", out.combined());
    let text = out.combined();
    assert!(
        text.contains(&format!("would graduate '{ALPHA}'")),
        "{text}"
    );
    assert!(text.contains(&format!("would graduate '{BETA}'")), "{text}");
    assert!(graduations(&sb).is_empty());
}

#[test]
fn a_roll_with_no_ungraduated_dependencies_graduates_as_it_always_did() {
    // No plan, no prompt: one roll, one merge, exactly the old behaviour.
    let sb = blocked_pair();
    sb.git(&["checkout", ALPHA]);

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "{}", out.combined());
    assert!(!out.combined().contains("in order"), "{}", out.combined());
    assert_eq!(graduations(&sb).len(), 1);
}

#[test]
fn promoting_one_roll_promotes_the_dependency_it_graduated_behind() {
    let sb = blocked_pair();
    assert!(sb.rf(&["graduate", "--yes"]).success);
    sb.git(&["checkout", "rolling"]);

    let out = sb.rf(&["promote", "--roll", BETA, "--yes"]);
    assert!(out.success, "promote: {}", out.combined());
    let text = out.combined();
    // alpha was added ahead of what was named, and each was its own step.
    assert!(text.contains("dependencies added"), "{text}");
    assert!(text.contains(&format!("Promoted '{ALPHA}'")), "{text}");
    assert!(text.contains(&format!("Promoted '{BETA}'")), "{text}");
    assert_eq!(sb.roll_state(ALPHA).as_deref(), Some("✓ promoted"));
    assert_eq!(sb.roll_state(BETA).as_deref(), Some("✓ promoted"));
}
