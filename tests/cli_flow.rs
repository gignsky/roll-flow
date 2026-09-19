//! CLI flow integration tests, driven through the shared [`harness::Sandbox`].
//!
//! These assert the merge-based (`--no-ff`) graduate/promote workflow from
//! epic #2: structured merge subjects, divergence tolerance, and clear errors
//! for genuinely-bad states. The broader lifecycle scenarios live in
//! `e2e_lifecycle.rs`.

mod harness;

use harness::Sandbox;

#[test]
fn init_create_and_promote_flow() {
    let sb = Sandbox::plain();

    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    sb.commit_file("work.txt", "w\n", "roll work");

    let out = sb.rf(&["verify"]);
    assert!(out.success, "verify failed: {}", out.combined());

    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate failed: {}", out.combined());

    // Graduation is a --no-ff merge with a structured subject, and we are
    // returned to the roll branch afterward.
    assert!(sb.tip_is_merge("rolling"), "rolling tip should be a merge");
    assert!(
        sb.tip_subject("rolling").starts_with("Graduate roll/1"),
        "unexpected graduation subject: {}",
        sb.tip_subject("rolling")
    );
    assert_eq!(sb.current_branch(), "roll/1-0611-feature");

    sb.git(&["checkout", "rolling"]);
    let out = sb.rf(&["promote"]);
    assert!(
        out.success,
        "promote rolling->main failed: {}",
        out.combined()
    );

    assert!(sb.tip_is_merge("main"), "main tip should be a merge");
    assert!(
        sb.tip_subject("main").starts_with("Promote "),
        "unexpected promotion subject: {}",
        sb.tip_subject("main")
    );
    assert!(
        sb.is_ancestor("rolling", "main"),
        "rolling should be merged into main"
    );
}

#[test]
fn promote_requires_clean_tree() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("dirty", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    sb.write("dirty.txt", "x\n");
    let out = sb.rf(&["promote"]);
    assert!(!out.success, "promote should fail on dirty tree");
    assert!(
        out.combined().contains("working tree must be clean"),
        "unexpected: {}",
        out.combined()
    );
}

#[test]
fn create_branches_off_stable_not_rolling() {
    // A roll must start from the stable branch so its baseline is clean and it
    // does not implicitly depend on whatever has accumulated on rolling. Only an
    // explicit `rf integrate` should pull another branch's work into a roll (#62).
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    // Advance rolling beyond stable so the two branch tips genuinely differ.
    sb.git(&["checkout", "rolling"]);
    sb.commit_file("rolling-only.txt", "r\n", "rolling advances");
    sb.git(&["checkout", "main"]);

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    // The roll is branched off stable (main): main is an ancestor of the roll,
    // and rolling's extra commit is NOT in the roll's history.
    assert!(
        sb.is_ancestor("main", "roll/1-0611-feature"),
        "stable should be an ancestor of the freshly created roll"
    );
    assert!(
        !sb.is_ancestor("rolling", "roll/1-0611-feature"),
        "rolling must NOT be an ancestor of a freshly created roll"
    );
}

#[test]
fn integrate_merges_branch_into_roll() {
    let sb = Sandbox::plain();

    let out = sb.init();
    assert!(out.success, "init: {}", out.combined());

    // A feature branch with unique content.
    sb.git(&["checkout", "-b", "feature/foo"]);
    sb.commit_file("foo.txt", "feature content\n", "add foo feature");

    // Create a roll (rf create checks out the new roll branch).
    let out = sb.create_roll("myroll", "0101");
    assert!(out.success, "create: {}", out.combined());

    // Integrate the feature branch into the roll.
    let out = sb.rf(&["integrate", "feature/foo"]);
    assert!(out.success, "integrate: {}", out.combined());

    // Feature content should now exist on the roll branch.
    assert!(sb.exists("foo.txt"), "foo.txt missing after integrate");

    // Should have been a --no-ff merge commit.
    assert!(
        sb.tip_subject("HEAD").contains("Merge branch"),
        "expected merge commit, got: {}",
        sb.tip_subject("HEAD")
    );
}

#[test]
fn promote_tolerates_divergence() {
    // Divergence is a supported state (epic #2): `rf promote` on a roll branch
    // falls through to graduation and merges with --no-ff even when rolling
    // has advanced independently.
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    sb.git(&["checkout", "rolling"]);
    sb.git(&["checkout", "-b", "roll/1-0611-diverge"]);
    sb.commit_file("roll.txt", "roll\n", "roll commit");

    sb.git(&["checkout", "rolling"]);
    sb.commit_file("rolling.txt", "rolling\n", "rolling diverges");

    sb.git(&["checkout", "roll/1-0611-diverge"]);
    let out = sb.rf(&["promote"]);
    assert!(
        out.success,
        "promote should tolerate divergence: {}",
        out.combined()
    );
    assert!(
        out.combined().contains("use rf graduate"),
        "expected the graduate redirect note: {}",
        out.combined()
    );
    assert!(
        sb.tip_is_merge("rolling"),
        "expected a --no-ff merge on rolling"
    );
    assert!(
        sb.tip_subject("rolling").starts_with("Graduate roll/1"),
        "unexpected subject: {}",
        sb.tip_subject("rolling")
    );
}

#[test]
fn init_is_idempotent_no_op_on_rerun() {
    let sb = Sandbox::nixos();
    let cfg_path = sb.path().join(".roll-flow.toml");

    // First run writes the config from detected state.
    let out = sb.init();
    assert!(out.success, "first init failed: {}", out.combined());
    assert!(
        out.combined().contains("Initialized roll-flow"),
        "unexpected first-run message: {}",
        out.combined()
    );
    assert!(sb.exists(".roll-flow.toml"), "config should exist");
    let after_first = std::fs::read_to_string(&cfg_path).expect("read config");

    // Second run detects the same state: a no-op that reports "up to date",
    // exits 0, needs no --force, and leaves the file byte-for-byte unchanged.
    let out = sb.init();
    assert!(out.success, "second init failed: {}", out.combined());
    assert_eq!(out.code, Some(0), "second init should exit 0");
    assert!(
        out.combined().contains("already up to date"),
        "expected 'already up to date', got: {}",
        out.combined()
    );
    let after_second = std::fs::read_to_string(&cfg_path).expect("read config");
    assert_eq!(
        after_first, after_second,
        "idempotent re-run must not rewrite the config"
    );
}

#[test]
fn init_regenerates_missing_config() {
    let sb = Sandbox::nixos();

    let out = sb.init();
    assert!(out.success, "first init failed: {}", out.combined());
    assert!(sb.exists(".roll-flow.toml"), "config should exist");

    // Delete the generated config, then re-init: it regenerates from detected
    // state rather than treating the file as required.
    std::fs::remove_file(sb.path().join(".roll-flow.toml")).expect("remove config");
    assert!(!sb.exists(".roll-flow.toml"), "config should be gone");

    let out = sb.init();
    assert!(out.success, "regenerating init failed: {}", out.combined());
    assert!(
        out.combined().contains("Initialized roll-flow"),
        "unexpected regenerate message: {}",
        out.combined()
    );
    assert!(sb.exists(".roll-flow.toml"), "config should be regenerated");
}

#[test]
fn init_applies_detected_changes_with_yes() {
    let sb = Sandbox::nixos();
    let cfg_path = sb.path().join(".roll-flow.toml");

    let out = sb.init();
    assert!(out.success, "first init failed: {}", out.combined());
    let detected = std::fs::read_to_string(&cfg_path).expect("read config");

    // Perturb the on-disk config so it diverges from the detected state.
    let stale = detected.replace("username = \"gig\"", "username = \"stale\"");
    assert_ne!(stale, detected, "perturbation should change the file");
    std::fs::write(&cfg_path, &stale).expect("write stale config");

    // With --yes, the detected config is applied non-interactively.
    let out = sb.rf(&["init", "--yes"]);
    assert!(out.success, "init --yes failed: {}", out.combined());
    assert!(
        out.combined().contains("Updated"),
        "expected an 'Updated' message, got: {}",
        out.combined()
    );
    let after = std::fs::read_to_string(&cfg_path).expect("read config");
    assert_eq!(after, detected, "--yes should restore the detected config");
}

#[test]
fn init_keeps_existing_config_when_non_interactive_without_flags() {
    let sb = Sandbox::nixos();
    let cfg_path = sb.path().join(".roll-flow.toml");

    let out = sb.init();
    assert!(out.success, "first init failed: {}", out.combined());
    let detected = std::fs::read_to_string(&cfg_path).expect("read config");

    let stale = detected.replace("username = \"gig\"", "username = \"stale\"");
    assert_ne!(stale, detected, "perturbation should change the file");
    std::fs::write(&cfg_path, &stale).expect("write stale config");

    // No flags, non-interactive (harness has no TTY): default to keeping the
    // existing file. Exit 0, nothing written.
    let out = sb.rf(&["init"]);
    assert!(out.success, "init should succeed: {}", out.combined());
    assert_eq!(out.code, Some(0), "non-destructive keep should exit 0");
    assert!(
        out.combined().contains("not applied"),
        "expected a 'not applied' note, got: {}",
        out.combined()
    );
    let after = std::fs::read_to_string(&cfg_path).expect("read config");
    assert_eq!(after, stale, "file must be left unchanged");
}

#[test]
fn init_mode_assist_persists_and_is_preserved_on_reinit() {
    let sb = Sandbox::nixos();
    let cfg_path = sb.path().join(".roll-flow.toml");

    let out = sb.rf(&["init", "--mode", "assist"]);
    assert!(out.success, "init --mode assist failed: {}", out.combined());
    let first = std::fs::read_to_string(&cfg_path).expect("read config");
    assert!(
        first.contains("mode = \"assist\""),
        "assist mode should persist: {first}"
    );

    // A plain re-init (no --mode) preserves the assist mode and is a no-op.
    let out = sb.init();
    assert!(out.success, "re-init failed: {}", out.combined());
    assert!(
        out.combined().contains("already up to date"),
        "re-init should be a no-op: {}",
        out.combined()
    );
    let second = std::fs::read_to_string(&cfg_path).expect("read config");
    assert_eq!(
        first, second,
        "re-init must preserve the config byte-for-byte"
    );
    assert!(
        second.contains("mode = \"assist\""),
        "mode must stay assist"
    );
}

#[test]
fn init_rejects_invalid_mode() {
    let sb = Sandbox::nixos();
    let out = sb.rf(&["init", "--mode", "bogus"]);
    assert!(!out.success, "invalid mode should fail");
    assert!(
        out.combined().contains("invalid --mode"),
        "expected a clear error, got: {}",
        out.combined()
    );
}

#[test]
fn graduate_errors_off_roll_branch() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    // Still on `main` after init.
    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "graduate should fail off a roll branch");
    assert!(
        out.combined().contains("must be run from a roll branch"),
        "unexpected: {}",
        out.combined()
    );
}

#[test]
fn graduate_errors_with_nothing_to_merge() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    // Fresh roll with no commits of its own.
    let out = sb.create_roll("empty", "0611");
    assert!(out.success, "create failed: {}", out.combined());

    let out = sb.rf(&["graduate"]);
    assert!(!out.success, "graduate should fail with nothing to merge");
    assert!(
        out.combined().contains("nothing to graduate"),
        "unexpected: {}",
        out.combined()
    );
}

#[test]
fn update_merges_stable_with_descriptive_subject() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("work.txt", "w\n", "roll work");

    // Advance stable so the roll is behind and has something to merge.
    sb.git(&["checkout", "main"]);
    sb.commit_file("stable.txt", "s\n", "advance stable");

    let out = sb.rf(&["update"]);
    assert!(out.success, "update failed: {}", out.combined());

    let roll = "roll/1-0611-feature";
    assert!(sb.tip_is_merge(roll), "roll tip should be a merge");
    assert!(
        sb.tip_subject(roll)
            .starts_with(&format!("Update {roll} from main")),
        "unexpected update subject: {}",
        sb.tip_subject(roll)
    );
    assert!(
        sb.is_ancestor("main", roll),
        "main should be merged into the roll"
    );
}

#[test]
fn update_skips_roll_already_up_to_date() {
    let sb = Sandbox::plain();
    let out = sb.init();
    assert!(out.success, "init failed: {}", out.combined());

    let out = sb.create_roll("feature", "0611");
    assert!(out.success, "create failed: {}", out.combined());
    sb.commit_file("work.txt", "w\n", "roll work");

    // Stable has not moved, so the roll already contains everything on main.
    let roll = "roll/1-0611-feature";
    let before = sb.rev(roll);

    let out = sb.rf(&["update"]);
    assert!(out.success, "update failed: {}", out.combined());
    assert!(
        out.combined().contains("already up to date"),
        "expected up-to-date message: {}",
        out.combined()
    );

    assert_eq!(before, sb.rev(roll), "no new commit should be created");
}

// ── #80: blocked-state derives from real integrations, not file overlap ───────

#[test]
fn overlap_alone_does_not_block() {
    // The exact bug from #80: two rolls that touch the SAME file but were never
    // integrated into one another must BOTH stay `active`. In a dotfiles repo
    // nearly every roll touches flake.lock, so file overlap must not block.
    let sb = Sandbox::plain();
    assert!(sb.init().success);

    // Roll A (roll/1) touches shared.txt.
    assert!(sb.create_roll("alpha", "0101").success);
    sb.commit_file("shared.txt", "from A\n", "A edits shared");

    // Roll B (roll/2), branched off stable, also touches shared.txt — no
    // integration between the two.
    assert!(sb.create_roll("beta", "0102").success);
    sb.commit_file("shared.txt", "from B\n", "B edits shared");

    assert_eq!(
        sb.roll_state("roll/1-0101-alpha").as_deref(),
        Some("active"),
        "A must not be blocked by mere file overlap"
    );
    assert_eq!(
        sb.roll_state("roll/2-0102-beta").as_deref(),
        Some("active"),
        "B must not be blocked by mere file overlap"
    );
}

#[test]
fn integration_of_ungraduated_roll_blocks() {
    // A roll that has directly integrated another (still-ungraduated) roll is
    // Blocked until that dependency graduates.
    let sb = Sandbox::plain();
    assert!(sb.init().success);

    assert!(sb.create_roll("alpha", "0101").success);
    sb.commit_file("a.txt", "a\n", "A work");

    assert!(sb.create_roll("beta", "0102").success);
    sb.commit_file("b.txt", "b\n", "B work");

    // From A, integrate B (a real `git merge --no-ff`).
    sb.git(&["checkout", "roll/1-0101-alpha"]);
    let out = sb.rf(&["integrate", "roll/2-0102-beta"]);
    assert!(out.success, "integrate failed: {}", out.combined());

    assert_eq!(
        sb.roll_state("roll/1-0101-alpha").as_deref(),
        Some("⛔ blocked"),
        "A integrated ungraduated B, so A must be blocked"
    );
    assert_eq!(
        sb.roll_state("roll/2-0102-beta").as_deref(),
        Some("active"),
        "B itself is a plain active roll"
    );
}

#[test]
fn graduating_dependency_unblocks_roll() {
    // Once the integrated dependency graduates, the roll is no longer blocked.
    let sb = Sandbox::plain();
    assert!(sb.init().success);

    assert!(sb.create_roll("alpha", "0101").success);
    sb.commit_file("a.txt", "a\n", "A work");

    assert!(sb.create_roll("beta", "0102").success);
    sb.commit_file("b.txt", "b\n", "B work");

    sb.git(&["checkout", "roll/1-0101-alpha"]);
    assert!(sb.rf(&["integrate", "roll/2-0102-beta"]).success);
    assert_eq!(
        sb.roll_state("roll/1-0101-alpha").as_deref(),
        Some("⛔ blocked"),
    );

    // Graduate B to rolling.
    sb.git(&["checkout", "roll/2-0102-beta"]);
    let out = sb.rf(&["graduate"]);
    assert!(out.success, "graduate B failed: {}", out.combined());

    assert_eq!(
        sb.roll_state("roll/2-0102-beta").as_deref(),
        Some("✓ graduated"),
        "B should now be graduated"
    );
    assert_eq!(
        sb.roll_state("roll/1-0101-alpha").as_deref(),
        Some("active"),
        "A's only integration dep graduated, so A is unblocked"
    );
}

// ── #81: `rf integrate` accepts a roll number ─────────────────────────────────

#[test]
fn integrate_accepts_roll_number() {
    let sb = Sandbox::plain();
    assert!(sb.init().success);

    assert!(sb.create_roll("alpha", "0101").success);
    sb.commit_file("a.txt", "a\n", "A work");

    assert!(sb.create_roll("beta", "0102").success);
    sb.commit_file("b.txt", "b\n", "B work");

    // From A, integrate roll #2 by number.
    sb.git(&["checkout", "roll/1-0101-alpha"]);
    let out = sb.rf(&["integrate", "2"]);
    assert!(
        out.success,
        "integrate by number failed: {}",
        out.combined()
    );
    assert!(sb.exists("b.txt"), "B's content should now be on A");
    assert!(
        sb.tip_subject("HEAD")
            .contains("Merge branch 'roll/2-0102-beta'"),
        "expected merge of roll/2, got: {}",
        sb.tip_subject("HEAD")
    );

    // A bogus number errors clearly.
    let out = sb.rf(&["integrate", "999"]);
    assert!(!out.success, "integrate of missing roll should fail");
    assert!(
        out.combined().contains("no roll with number 999"),
        "unexpected error: {}",
        out.combined()
    );
}

// ── config file honesty (roll/18) ────────────────────────────────────────────

#[test]
fn init_keeps_hand_edited_gates_comments_and_flags() {
    // The old `rf init --yes` regenerated the file from detection and so
    // deleted every gate, every clean_protect entry and every comment. It now
    // edits in place: detected keys refreshed, missing keys added, the rest
    // left exactly as typed.
    let sb = Sandbox::nixos();
    sb.init();
    let path = sb.path().join(".roll-flow.toml");
    let mut text = std::fs::read_to_string(&path).expect("read");
    text = text.replace(
        "rolling_to_main_gates = []",
        "# the release gate\nrolling_to_main_gates = [\"just test-all\"]",
    );
    text = text.replace("clean_protect = []", "clean_protect = [\"staging\"]");
    text = text.replace("push_tag = true", "push_tag = false");
    std::fs::write(&path, &text).expect("write");

    let out = sb.rf(&["init", "--yes"]);
    assert!(out.success, "{}", out.combined());
    let after = std::fs::read_to_string(&path).expect("read");
    assert!(
        after.contains("# the release gate"),
        "comment lost:\n{after}"
    );
    assert!(
        after.contains("rolling_to_main_gates = [\"just test-all\"]"),
        "{after}"
    );
    assert!(after.contains("clean_protect = [\"staging\"]"), "{after}");
    assert!(after.contains("push_tag = false"), "{after}");
    // And the detected hosts survived untouched.
    assert!(after.contains("ganoslal = true"), "{after}");
}

#[test]
fn init_adds_keys_a_newer_rf_knows_ahead_of_the_host_table() {
    // A config from before `lazygit_command` existed. Init adds it — and puts
    // it in the root table, not after `[host_active]` where it would become a
    // host name.
    let sb = Sandbox::nixos();
    sb.init();
    let path = sb.path().join(".roll-flow.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    let stripped: String = text
        .lines()
        .filter(|l| !l.starts_with("lazygit_command"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&path, &stripped).expect("write");

    let out = sb.rf(&["init", "--yes"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined().contains("+lazygit_command"),
        "{}",
        out.combined()
    );
    let after = std::fs::read_to_string(&path).expect("read");
    let key_at = after.find("lazygit_command").expect("key added");
    let table_at = after.find("[host_active]").expect("table present");
    assert!(
        key_at < table_at,
        "key landed inside the host table:\n{after}"
    );
    // Loading it back reads the key, not a host called lazygit_command.
    let out = sb.rf(&["status", "--no-tui"]);
    assert!(out.success, "{}", out.combined());
    assert!(!out.combined().contains("warning"), "{}", out.combined());
}

#[test]
fn a_typo_in_the_config_is_warned_about_and_the_command_still_runs() {
    let sb = Sandbox::nixos();
    sb.init();
    let path = sb.path().join(".roll-flow.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    let text = text.replace("clean_protect = []", "clean_protct = [\"staging\"]");
    std::fs::write(&path, &text).expect("write");

    let out = sb.rf(&["status", "--no-tui"]);
    assert!(
        out.success,
        "a typo must not break the command: {}",
        out.combined()
    );
    assert!(
        out.stderr.contains("warning: unknown key 'clean_protct'"),
        "no warning:\n{}",
        out.combined()
    );
}

#[test]
fn a_config_version_from_another_rf_warns_once_and_survives_init() {
    let sb = Sandbox::nixos();
    sb.init();
    let path = sb.path().join(".roll-flow.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    std::fs::write(
        &path,
        text.replace("config_version = 1", "config_version = 3"),
    )
    .expect("write");

    let out = sb.rf(&["status", "--no-tui"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.stderr.contains("config_version is 3"),
        "{}",
        out.combined()
    );

    // Init reports nothing to change: the version is the file's statement
    // about itself, not something detection knows better.
    let out = sb.rf(&["init"]);
    assert!(out.success, "{}", out.combined());
    assert!(
        out.combined().contains("already up to date"),
        "{}",
        out.combined()
    );
}

#[test]
fn a_missing_required_key_points_at_init() {
    let sb = Sandbox::nixos();
    sb.init();
    let path = sb.path().join(".roll-flow.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    let text: String = text
        .lines()
        .filter(|l| !l.starts_with("stable_branch"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&path, &text).expect("write");

    let out = sb.rf(&["status", "--no-tui"]);
    assert!(!out.success);
    assert!(
        out.combined().contains("stable_branch"),
        "{}",
        out.combined()
    );
    assert!(out.combined().contains("rf init"), "{}", out.combined());
}

#[test]
fn init_detects_hosts_from_the_real_hosts_nix_shape() {
    // The fixture is shaped like the dotfiles file: a bare attrset. Detection
    // never worked on that shape before, which is why the dotfiles config has
    // `hosts = []`.
    let sb = Sandbox::nixos();
    sb.init();
    let text = std::fs::read_to_string(sb.path().join(".roll-flow.toml")).expect("read");
    // Rendered multi-line by `toml`; what matters is all three, in file order.
    let listed: Vec<&str> = text
        .split("hosts = [")
        .nth(1)
        .and_then(|r| r.split(']').next())
        .map(|b| b.split('"').skip(1).step_by(2).collect())
        .unwrap_or_default();
    assert_eq!(listed, vec!["ganoslal", "merlin", "wsl"], "{text}");
    assert!(text.contains("username = \"gig\""), "{text}");
    assert!(text.contains("[host_active]"), "{text}");
    assert!(text.contains("wsl = false"), "{text}");
}

#[test]
fn init_never_flips_a_host_the_file_already_names() {
    // The file is where the user records what is true now; vars/hosts.nix only
    // seeds it. A host flipped off by hand, with a comment saying why, must
    // survive `rf init --yes` — and a host the seed newly mentions is added.
    let sb = Sandbox::nixos();
    sb.init();
    let path = sb.path().join(".roll-flow.toml");
    let text = std::fs::read_to_string(&path).expect("read");
    let text = text.replace(
        "[host_active]\nganoslal = true",
        "[host_active]\n# ganoslal is being rebuilt; back on next week\nganoslal = false",
    );
    std::fs::write(&path, &text).expect("write");
    // The seed grows a host the file has never heard of.
    sb.write(
        "vars/hosts.nix",
        "{\n  ganoslal = true;\n  merlin = true;\n  wsl = false;\n  spacedock = true;\n}\n",
    );

    let out = sb.rf(&["init", "--yes"]);
    assert!(out.success, "{}", out.combined());
    let after = std::fs::read_to_string(&path).expect("read");
    assert!(
        after.contains("# ganoslal is being rebuilt"),
        "comment lost:\n{after}"
    );
    assert!(
        after.contains("ganoslal = false"),
        "hand edit undone:\n{after}"
    );
    assert!(
        after.contains("spacedock = true"),
        "new host not added:\n{after}"
    );
    assert!(
        after.contains("\"spacedock\""),
        "new host not listed in hosts:\n{after}"
    );
}
