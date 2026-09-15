mod cli;
mod core;
mod error;
mod tui;

use std::io::IsTerminal;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Serialize;

use cli::{Cli, Cmd};
use core::version::{BumpLevel, VersionCheck, VersionStatus};
use core::{branches, config::Config, git, ops};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Bare `rf` (no subcommand) runs the status dashboard, identical to
    // `rf status` (issue #100). `--help`/`-h`/`--version` never reach here —
    // clap intercepts them during `parse()`.
    let Some(command) = cli.command else {
        return cli::status::run(false, true);
    };

    match command {
        Cmd::Init {
            rolling_branch,
            stable_branch,
            roll_prefix,
            username,
            hosts,
            mode,
            force,
            yes,
        } => cmd_init(
            rolling_branch,
            stable_branch,
            roll_prefix,
            username,
            hosts,
            mode,
            force,
            yes,
        )?,
        Cmd::Create {
            slug,
            date,
            dry_run,
        } => cmd_create(&slug, date, dry_run)?,
        Cmd::Integrate { branch } => cmd_integrate(&branch)?,
        Cmd::Hotfix {
            slug,
            date,
            land,
            dry_run,
        } => {
            if land {
                cmd_hotfix_land(dry_run)?;
            } else {
                match slug {
                    Some(slug) => cmd_hotfix_create(&slug, date, dry_run)?,
                    None => bail!(
                        "rf hotfix requires a <slug> (or pass --land to land the current hotfix)"
                    ),
                }
            }
        }
        Cmd::Verify { dry_run, bump, yes } => cmd_verify(dry_run, bump, yes)?,
        Cmd::Graduate {
            dry_run,
            force,
            reason,
        } => cmd_graduate(dry_run, force, reason)?,
        Cmd::Promote {
            roll,
            dry_run,
            force,
            reason,
            bump,
            no_tag,
            yes,
        } => cmd_promote(roll, dry_run, force, reason, bump, !no_tag, yes)?,
        Cmd::Status {
            no_tui,
            no_deps,
            json,
        } => {
            if json {
                cmd_status_json()?;
            } else {
                cli::status::run(no_tui, !no_deps)?;
            }
        }
        Cmd::List { no_tui, deps, json } => {
            if json {
                cmd_list_json()?;
            } else {
                cmd_list_text(no_tui, deps)?;
            }
        }
        Cmd::Update { dry_run } => cmd_update(dry_run)?,
        Cmd::Prune {
            dry_run,
            local,
            remote,
            yes,
            force,
            no_fetch,
        } => cmd_prune(dry_run, local, remote, yes, force, no_fetch)?,
        Cmd::Delete {
            branch,
            dry_run,
            local,
            remote,
            yes,
            force,
            no_fetch,
        } => cmd_delete(&branch, dry_run, local, remote, yes, force, no_fetch)?,
        Cmd::Clean {
            dry_run,
            yes,
            force,
            with_remote,
            no_fetch,
        } => cli::clean::run(dry_run, yes, force, with_remote, no_fetch)?,
        Cmd::Version => println!("{}", env!("CARGO_PKG_VERSION")),
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    rolling_branch: Option<String>,
    stable_branch: Option<String>,
    roll_prefix: Option<String>,
    username: Option<String>,
    hosts: Option<String>,
    mode: Option<String>,
    force: bool,
    yes: bool,
) -> Result<()> {
    let mut config = Config::auto_detect()?;
    config = config.with_overrides(rolling_branch, stable_branch, roll_prefix, username, hosts);

    let cfg_path = Config::config_path(&config.repo_root);
    if !git::ref_exists(&config.repo_root, &config.stable_branch) {
        bail!("stable branch '{}' not found", config.stable_branch);
    }
    if !git::ref_exists(&config.repo_root, &config.rolling_branch) {
        git::run_git(
            &config.repo_root,
            &["branch", &config.rolling_branch, &config.stable_branch],
        )?;
    }

    // Resolve the workflow mode (issue #18): an explicit `--mode` always wins;
    // otherwise, if a config already exists, preserve its mode so a bare re-init
    // never silently resets it (and stays idempotent). Absent both, the field's
    // serde default (`manage`) applies via `auto_detect`.
    if let Some(m) = mode {
        config.mode = core::config::Mode::parse(&m)?;
    } else if cfg_path.exists() {
        if let Ok(existing) = std::fs::read_to_string(&cfg_path) {
            if let Ok(existing_cfg) = toml::from_str::<Config>(&existing) {
                config.mode = existing_cfg.mode;
            }
        }
    }

    // Re-running `rf init` is idempotent and non-destructive: it regenerates the
    // config from the repo's actual detected state and only rewrites the file
    // when the result differs. Serialize both sides through the same renderer
    // (`to_toml_string`) so an unchanged re-run is a true no-op — no rewrite,
    // and no `--force` required (issue #16).
    let regenerated = config.to_toml_string()?;
    if cfg_path.exists() {
        let existing = std::fs::read_to_string(&cfg_path)
            .with_context(|| format!("reading existing config at {}", cfg_path.display()))?;
        if existing == regenerated {
            if !force {
                println!("roll-flow config already up to date (no changes)");
                return Ok(());
            }
            // Identical content but `--force`: rewrite anyway, matching prior
            // `--force` semantics.
            config.save()?;
            println!("Updated {} from detected state", cfg_path.display());
            return Ok(());
        }

        // The regenerated config differs from what's on disk. Rather than
        // silently overwriting, show the change and decide non-destructively
        // (issue #17).
        println!("Detected config changes for {}:", cfg_path.display());
        print!("{}", config_diff(&existing, &regenerated));

        let apply = if force || yes {
            true
        } else if std::io::stdin().is_terminal() {
            cli::prompt_yes("Apply these changes to .roll-flow.toml? [y/N] ")?
        } else {
            // Non-interactive without --yes/--force: default to keeping the
            // existing file. Nothing is written; exit 0.
            false
        };

        if apply {
            config.save()?;
            println!("Updated {} from detected state", cfg_path.display());
        } else {
            if !std::io::stdin().is_terminal() {
                println!("Changes detected but not applied. Run with --yes to apply, or --force.");
            }
            println!("Kept existing config at {}", cfg_path.display());
        }
    } else {
        config.save()?;
        println!("Initialized roll-flow at {}", cfg_path.display());
    }
    Ok(())
}

/// A dependency-free, line-based diff of two config renderings: lines only in
/// `current` are prefixed `-`, lines only in `detected` are prefixed `+`.
fn config_diff(current: &str, detected: &str) -> String {
    let cur: Vec<&str> = current.lines().collect();
    let det: Vec<&str> = detected.lines().collect();
    let mut out = String::new();
    for line in &cur {
        if !det.contains(line) {
            out.push_str(&format!("-{line}\n"));
        }
    }
    for line in &det {
        if !cur.contains(line) {
            out.push_str(&format!("+{line}\n"));
        }
    }
    out
}

fn cmd_create(slug: &str, date: Option<String>, dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
    let outcome = ops::create(&config, slug, date, dry_run)?;
    print_create(&outcome);
    Ok(())
}

fn cmd_integrate(arg: &str) -> Result<()> {
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
    let branch = resolve_integrate_target(&config, arg)?;
    let outcome = ops::integrate(&config, &branch)?;
    println!("integrated {} into {}", outcome.branch, outcome.current);
    Ok(())
}

/// Resolve the `integrate` argument to a branch name. A bare positive integer is
/// looked up as a roll number and mapped to its `roll/<N>-…` branch; anything
/// else is treated verbatim as a branch name (back-compatible).
fn resolve_integrate_target(config: &Config, arg: &str) -> Result<String> {
    let Ok(number) = arg.parse::<u32>() else {
        return Ok(arg.to_string());
    };
    let rolls = branches::list_rolls(config)?;
    match rolls.into_iter().find(|r| r.number == number) {
        Some(roll) => Ok(roll.branch),
        None => bail!("no roll with number {number}"),
    }
}

fn cmd_hotfix_create(slug: &str, date: Option<String>, dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
    let outcome = ops::hotfix_create(&config, slug, date, dry_run)?;
    print_create(&outcome);
    Ok(())
}

/// Shared renderer for roll/hotfix creation (both emit the same lines).
fn print_create(outcome: &ops::CreateOutcome) {
    if outcome.dry_run {
        println!(
            "Dry-run: would create '{}' from '{}'",
            outcome.branch, outcome.stable
        );
    } else {
        println!("Created {}", outcome.branch);
    }
}

fn cmd_hotfix_land(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
    let outcome = ops::hotfix_land(&config, dry_run)?;
    render_gate_notices(&outcome.gate_notices);
    if outcome.dry_run {
        println!(
            "Dry-run: would land '{}' into '{}' (--no-ff), then reintegrate '{}' into '{}'",
            outcome.current, outcome.stable, outcome.stable, outcome.rolling
        );
    } else {
        println!("Landed '{}' into '{}'", outcome.current, outcome.stable);
        println!(
            "Reintegrated '{}' into '{}'",
            outcome.stable, outcome.rolling
        );
    }
    Ok(())
}

fn cmd_verify(dry_run: bool, bump: Option<BumpLevel>, yes: bool) -> Result<()> {
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;

    // Resolved before `ops::verify` so an accepted bump is already committed by
    // the time the gates (and their `--locked` cargo commands) run.
    resolve_version_gate(&config, bump, yes, false, dry_run)?;

    let outcome = ops::verify(&config, dry_run)?;
    if outcome.diverged_note {
        println!(
            "note: '{}' has commits not in '{}'; graduation/promotion will create a --no-ff merge",
            outcome.target, outcome.source
        );
    }
    render_version_check(&outcome.version, &outcome.source, &outcome.target);
    render_gate_notices(&outcome.gate_notices);
    render_gate_notices(&outcome.host_notices);
    render_host_results(&outcome.host_results);
    if !outcome.failed_hosts.is_empty() {
        bail!(
            "host verification failed: {}",
            outcome.failed_hosts.join(", ")
        );
    }
    // `--dry-run` previews rather than enforces, matching how it treats the
    // configured gates (printed, never executed) and `rf promote --dry-run`.
    if !dry_run && !outcome.version.is_satisfied() {
        return Err(ops::version_gate_error(
            &outcome.version,
            &outcome.source,
            &outcome.target,
        ));
    }
    println!(
        "Verification passed: {} -> {}",
        outcome.source, outcome.target
    );
    Ok(())
}

// ── Version gate ────────────────────────────────────────────────────────────

/// Enforce the crate-version bump requirement before the expensive gates run,
/// offering to apply the bump when it is missing.
///
/// Sequencing matters: a bump rewrites `Cargo.lock` as well as `Cargo.toml`, and
/// the configured gates include `cargo update --workspace --locked`, which would
/// fail against a stale lockfile. So the bump has to land *first*, which is why
/// this is a CLI-level step rather than something inside `ops::promote`.
///
/// Mirrors `.github/workflows/version-bump-check.yml`, so a promotion done with
/// `rf` and one done through a PR are held to the same standard.
fn resolve_version_gate(
    config: &Config,
    bump: Option<BumpLevel>,
    yes: bool,
    forced: bool,
    dry_run: bool,
) -> Result<()> {
    let current = git::current_branch(&config.repo_root)?;
    let route = match ops::infer_route(config, &current) {
        Some(route) => route,
        // Not a promotable branch: the caller raises its own clearer error.
        None => return Ok(()),
    };
    let (source, target) = match &route {
        ops::Route::Graduate { roll } => (roll.clone(), config.rolling_branch.clone()),
        ops::Route::Promote => (config.rolling_branch.clone(), config.stable_branch.clone()),
    };

    // Graduation into rolling is deliberately out of scope: only the promotion
    // route carries the bump requirement here.
    if !matches!(route, ops::Route::Promote) {
        return Ok(());
    }
    // Resolve the target the same way the merge will: local branch first, then
    // `origin/<target>`. A repo that has never checked stable out locally still
    // has a version to compare against, and skipping the check here would let
    // the bump be missed and only resurface as a hard error later.
    let Some(target_ref) = git::resolve_branch(&config.repo_root, &target) else {
        // Genuinely missing on both sides — `ops::promote` raises the clearer
        // "branch not found" error for this.
        return Ok(());
    };

    let check = ops::version_check(config, &source, &target_ref)?;
    if check.is_satisfied() {
        return Ok(());
    }

    // A version *below* the target is never fixable by bumping one level, and
    // silently jumping it would hide a bad merge. Always a hard error.
    if check.status == VersionStatus::Lower {
        return Err(ops::version_gate_error(&check, &source, &target));
    }

    let head = check
        .head
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<unreadable>".to_string());
    println!("Version: {head} on '{source}' is unchanged from '{target}'; a bump is required");

    if dry_run {
        println!("Dry-run: not bumping the version");
        return Ok(());
    }

    // The bump commit must land on the branch being merged from. `run_merge`
    // returns us here afterwards, and `ensure_clean_state` already ran, but
    // assert rather than trust it — a misplaced release commit is painful.
    if current != source {
        bail!(
            "refusing to bump the version: expected to be on '{source}' but '{current}' is checked out"
        );
    }

    let level = match bump {
        Some(level) => Some(level),
        None if yes => Some(BumpLevel::Patch),
        None if std::io::stdin().is_terminal() => prompt_bump_level()?,
        None => {
            if forced {
                None
            } else {
                return Err(ops::version_gate_error(&check, &source, &target));
            }
        }
    };

    let Some(level) = level else {
        if forced {
            eprintln!("warning: version not bumped, continuing under --force");
        }
        return Ok(());
    };

    let (from, to) = ops::apply_version_bump(config, level, &source)?;
    println!("Bumped version {from} -> {to} (chore(release) commit on '{current}')");
    Ok(())
}

/// Ask which field to raise. `None` means the user declined.
fn prompt_bump_level() -> Result<Option<BumpLevel>> {
    use std::io::Write;
    print!("Bump version? [p]atch / [m]inor / [M]ajor / [n]o: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    // Case-sensitive on purpose: `m` and `M` are different answers here, so
    // this cannot reuse the lowercasing `prompt_yes`.
    match line.trim() {
        "p" | "patch" | "" => Ok(Some(BumpLevel::Patch)),
        "m" | "minor" => Ok(Some(BumpLevel::Minor)),
        "M" | "major" | "MAJOR" => Ok(Some(BumpLevel::Major)),
        _ => Ok(None),
    }
}

/// Render the version comparison line. Silent when the repo does not version
/// through `Cargo.toml`, so unaffected repos see no new output.
fn render_version_check(check: &VersionCheck, source: &str, target: &str) {
    if check.status == VersionStatus::NotApplicable {
        return;
    }
    let head = check
        .head
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<unreadable>".to_string());
    let base = check
        .base
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_string());
    let verdict = match check.status {
        VersionStatus::Ok => "OK",
        VersionStatus::Unchanged => "UNCHANGED",
        VersionStatus::Lower => "LOWER",
        VersionStatus::Unreadable => "UNREADABLE",
        VersionStatus::NotApplicable => return,
    };
    println!("Version: {head} on '{source}' (base '{target}' {base}) {verdict}");
}

/// Offer to push a freshly created release tag.
///
/// This is the one place outside `rf prune` that writes to the remote, so it is
/// always gated on an explicit confirmation (or `--yes`) and never happens
/// silently.
fn offer_tag_push(config: &Config, tag: &str, yes: bool) -> Result<()> {
    if !config.push_tag {
        return Ok(());
    }
    let repo = &config.repo_root;
    if !git::has_remote(repo, "origin") {
        return Ok(());
    }
    let push = if yes {
        true
    } else if std::io::stdin().is_terminal() {
        cli::prompt_yes(&format!("Push tag {tag} to origin? [y/N] "))?
    } else {
        println!("note: tag {tag} was not pushed (run with --yes, or: git push origin {tag})");
        return Ok(());
    };
    if push {
        git::push_tag(repo, "origin", tag)
            .with_context(|| format!("failed to push tag {tag} to origin"))?;
        println!("Pushed tag {tag} to origin");
    } else {
        println!("note: tag {tag} kept local (push later with: git push origin {tag})");
    }
    Ok(())
}

fn cmd_graduate(dry_run: bool, force: bool, reason: Option<String>) -> Result<()> {
    let force = ops::ForceOpts::new(force, reason)?;
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
    let current = git::current_branch(&config.repo_root)?;
    if !current.starts_with(&config.roll_prefix) {
        bail!(
            "rf graduate must be run from a roll branch (current: '{}'); \
             to promote '{}' -> '{}' use rf promote",
            current,
            config.rolling_branch,
            config.stable_branch
        );
    }
    let outcome = ops::graduate(&config, &current, dry_run, &force)?;
    print_graduate(&outcome);
    Ok(())
}

fn cmd_promote(
    rolls: Vec<String>,
    dry_run: bool,
    force: bool,
    reason: Option<String>,
    bump: Option<BumpLevel>,
    tag: bool,
    yes: bool,
) -> Result<()> {
    let forced = force;
    let force = ops::ForceOpts::new(force, reason)?;
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;

    // `--roll` names what to promote outright, so it needs no route inference —
    // and deliberately works from any branch, rather than being redirected to
    // graduate because HEAD happens to sit on a roll.
    if !rolls.is_empty() {
        // No version-bump prompt here: a per-roll promotion merges a commit that
        // already exists on rolling, so there is no branch to land a bump on —
        // the gate compares what that commit carries and says so if it is short.
        // Say so rather than dropping the flag on the floor.
        if bump.is_some() {
            eprintln!(
                "warning: --bump is ignored with --roll; a roll's version is whatever \
                 its graduation commit already carries"
            );
        }
        let outcome = ops::promote(
            &config,
            &ops::PromoteTarget::Rolls(rolls),
            dry_run,
            &force,
            tag,
        )?;
        print_promote(&outcome);
        offer_step_tag_pushes(&config, &outcome, yes)?;
        return Ok(());
    }

    let current = git::current_branch(&config.repo_root)?;
    match ops::infer_route(&config, &current) {
        Some(ops::Route::Graduate { roll }) => {
            println!(
                "note: '{}' is a roll branch; graduating into '{}' — use rf graduate directly next time",
                roll, config.rolling_branch
            );
            let outcome = ops::graduate(&config, &roll, dry_run, &force)?;
            print_graduate(&outcome);
        }
        Some(ops::Route::Promote) => {
            // Resolved before `ops::promote` so the bump commit is part of what
            // gets merged, and so it precedes the `--locked` cargo gates.
            resolve_version_gate(&config, bump, yes, forced, dry_run)?;

            let outcome =
                ops::promote(&config, &ops::PromoteTarget::Rolling, dry_run, &force, tag)?;
            print_promote(&outcome);
            offer_step_tag_pushes(&config, &outcome, yes)?;
        }
        None => return Err(ops::not_promotable_error(&config, &current)),
    }
    Ok(())
}

/// Render a promotion outcome: every step in the order it was applied, then the
/// rolls that needed no work. A whole-rolling promotion is one step, so this
/// prints exactly what it always did for that case.
fn print_promote(outcome: &ops::PromoteOutcome) {
    for step in &outcome.steps {
        render_version_check(&step.version, &step.source, &outcome.stable);
        render_gate_notices(&step.gate_notices);
        render_gate_notices(&step.host_notices);
        render_host_results(&step.host_results);
        let what = step.roll.as_deref().unwrap_or(&outcome.rolling);
        if outcome.dry_run {
            // Naming the source matters for `--roll`: it shows the merge is of a
            // graduation commit on rolling, not of the roll branch.
            println!(
                "Dry-run: would promote '{}' into '{}' by merging '{}' (--no-ff)",
                what, outcome.stable, step.source
            );
        } else {
            println!("Promoted '{}' into '{}'", what, outcome.stable);
        }
        if let Some(line) = step.tag.describe() {
            println!("{line}");
        }
    }
    for skip in &outcome.skipped {
        println!("skipped '{}': {}", skip.roll, skip.reason);
    }
}

/// Offer to push whatever release tags the promotion created. A whole-rolling
/// promotion is one step and so asks once, exactly as it always did; a per-roll
/// promotion asks per tag, since each step is its own release.
fn offer_step_tag_pushes(config: &Config, outcome: &ops::PromoteOutcome, yes: bool) -> Result<()> {
    for step in &outcome.steps {
        if let Some(tag) = step.tag.created_tag() {
            offer_tag_push(config, tag, yes)?;
        }
    }
    Ok(())
}

fn print_graduate(outcome: &ops::GraduateOutcome) {
    render_gate_notices(&outcome.gate_notices);
    if outcome.dry_run {
        println!(
            "Dry-run: would graduate '{}' into '{}' (--no-ff)",
            outcome.roll, outcome.rolling
        );
    } else {
        println!("Graduated '{}' into '{}'", outcome.roll, outcome.rolling);
    }
}

/// Render the roll-flow status lines that `ops::run_gates` collects instead of
/// printing, preserving the exact strings and stdout/stderr streams.
fn render_gate_notices(notices: &[ops::GateNotice]) {
    for notice in notices {
        match notice {
            ops::GateNotice::NoGates => println!("No gates configured"),
            ops::GateNotice::DryRun(gate) => println!("Dry-run gate: {gate}"),
            ops::GateNotice::DryRunHost(gate) => println!("Dry-run host gate: {gate}"),
            ops::GateNotice::Bypassed { gate, code } => eprintln!(
                "warning: gate failed but bypassed (--force): {gate} ({})",
                ops::exit_desc(*code)
            ),
        }
    }
}

/// Render the per-host verification summary that `rf verify`/`rf promote`
/// produce when host gates ran. Prints nothing when no host gates executed (no
/// host gates configured, or no active hosts), so unaffected repos stay quiet.
fn render_host_results(results: &[ops::HostResult]) {
    if results.is_empty() {
        return;
    }
    println!("Host verification:");
    for result in results {
        let status = if result.passed() { "PASSED" } else { "FAILED" };
        println!("  {}: {status}", result.host);
    }
}

fn cmd_status_json() -> Result<()> {
    let config = Config::load()?;
    let current = git::current_branch(&config.repo_root).unwrap_or_else(|_| "HEAD".to_string());
    let detached = git::is_detached_head(&config.repo_root)?;
    let clean = ops::workflow_clean(&config)?;
    let rolls = branches::list_rolls(&config)?;
    let tier = ops::branch_tier(&config, &current, detached);

    let readiness = ops::promotion_readiness(&config, &current, clean, detached);
    let promotion = PromotionReadiness {
        description: readiness.description,
        ready: readiness.ready,
        reason: readiness.reason,
    };

    let payload = StatusPayload {
        current_branch: current,
        detached_head: detached,
        tier,
        clean_working_tree: clean,
        pending_roll_branches: rolls.into_iter().map(|r| r.branch).collect(),
        promotion,
    };
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn cmd_list_json() -> Result<()> {
    let config = Config::load()?;
    let rolls = branches::list_rolls(&config)?;
    println!("{}", serde_json::to_string_pretty(&rolls_for_json(rolls))?);
    Ok(())
}

fn cmd_update(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    match ops::update(&config, dry_run)? {
        ops::UpdateOutcome::NoActiveRolls => {
            println!("no active local rolls to update");
        }
        ops::UpdateOutcome::Ran { stable, items } => {
            for item in items {
                match item {
                    ops::UpdateItem::AlreadyUpToDate { roll } => {
                        println!("'{roll}' is already up to date with '{stable}'");
                    }
                    ops::UpdateItem::WouldMerge { roll, behind } => {
                        println!(
                            "dry-run: would merge '{stable}' into '{roll}' ({behind} commit{} ahead)",
                            if behind == 1 { "" } else { "s" },
                        );
                    }
                    ops::UpdateItem::Updated { roll } => {
                        println!("updated '{roll}' with '{stable}'");
                    }
                }
            }
        }
    }
    Ok(())
}

/// `rf prune` — delete roll branches already promoted to stable.
///
/// Deliberately does not call `ops::ensure_clean_state`: unlike graduate/promote
/// this never moves `HEAD` and never merges, so a dirty working tree is
/// irrelevant to it.
fn cmd_prune(
    dry_run: bool,
    local: bool,
    remote: bool,
    yes: bool,
    force: bool,
    no_fetch: bool,
) -> Result<()> {
    let config = Config::load()?;

    // Neither flag means both copies; either one narrows to just that side.
    let scope = ops::PruneScope {
        local: local || !remote,
        remote: remote || !local,
        force,
        fetch: !no_fetch,
    };

    let plan = ops::prune_plan(&config, &scope)?;

    if plan.is_empty() {
        println!("no promoted roll branches to prune");
        render_prune_skips(&plan.skipped);
        return Ok(());
    }

    render_prune_plan(&plan, "Promoted roll branches to prune:");
    render_prune_skips(&plan.skipped);

    if dry_run {
        println!("\nDry-run: nothing deleted");
        return Ok(());
    }

    // `--force` widens *what* may be deleted; only `--yes` skips the prompt.
    match cli::confirm(yes, "\nDelete these branches? [y/N] ")? {
        cli::Confirm::Yes => {}
        cli::Confirm::Declined => {
            println!("Nothing deleted.");
            return Ok(());
        }
        cli::Confirm::Unattended => {
            println!("\nNothing deleted. Re-run with --yes to apply.");
            return Ok(());
        }
    }

    println!();
    let results = ops::prune_apply(&config, &plan)?;
    let failures = render_prune_results(&results, "Pruned");
    if failures > 0 {
        bail!(
            "{failures} branch{} could not be deleted",
            if failures == 1 { "" } else { "es" }
        );
    }
    Ok(())
}

/// `rf delete <branch>` — delete one named roll branch, locally, on origin, or
/// both.
///
/// The CLI twin of the TUI's `[d]elete`, and the reason the shared deletion
/// rules in `ops` are testable end to end at all. Like `cmd_prune` it never
/// moves `HEAD`, so a dirty working tree is irrelevant and `ensure_clean_state`
/// is deliberately not called.
#[allow(clippy::too_many_arguments)]
fn cmd_delete(
    branch: &str,
    dry_run: bool,
    local: bool,
    remote: bool,
    yes: bool,
    force: bool,
    no_fetch: bool,
) -> Result<()> {
    let config = Config::load()?;

    // Neither flag means both copies; either one narrows to just that side.
    let scope = ops::PruneScope {
        local: local || !remote,
        remote: remote || !local,
        force,
        fetch: !no_fetch,
    };

    let plan = ops::delete_branch_plan(&config, branch, &scope)?;

    if plan.is_empty() {
        println!("nothing to delete for '{branch}'");
        render_prune_skips(&plan.skipped);
        return Ok(());
    }

    render_prune_plan(&plan, "Branch to delete:");
    render_prune_skips(&plan.skipped);

    if dry_run {
        println!("\nDry-run: nothing deleted");
        return Ok(());
    }

    // Same split as `rf prune`: `--force` widens *what* may be deleted, only
    // `--yes` skips the prompt, and an unattended run without `--yes` deletes
    // nothing and exits 0.
    match cli::confirm(yes, &format!("\nDelete '{branch}'? [y/N] "))? {
        cli::Confirm::Yes => {}
        cli::Confirm::Declined => {
            println!("Nothing deleted.");
            return Ok(());
        }
        cli::Confirm::Unattended => {
            println!("\nNothing deleted. Re-run with --yes to apply.");
            return Ok(());
        }
    }

    println!();
    let results = ops::prune_apply(&config, &plan)?;
    let failures = render_prune_results(&results, "Deleted");
    if failures > 0 {
        bail!("'{branch}' could not be deleted");
    }
    Ok(())
}

/// Render the branches a prune would delete, and which copies of each.
fn render_prune_plan(plan: &ops::PrunePlan, title: &str) {
    let name_w = plan
        .candidates
        .iter()
        .map(|c| c.branch.len())
        .max()
        .unwrap_or(6)
        .max(6);

    println!("{title}");
    println!();
    println!(
        "  {num:>3}  {name:<nw$}  delete",
        num = "#",
        name = "branch",
        nw = name_w,
    );
    println!("  ───  {}  ──────────────", "─".repeat(name_w));

    for candidate in &plan.candidates {
        let target = match (candidate.delete_local, candidate.delete_remote) {
            (true, true) => "local + origin",
            (true, false) => "local",
            (false, true) => "origin",
            (false, false) => "—",
        };
        println!(
            "  {num:>3}  {name:<nw$}  {target}",
            num = candidate.number,
            name = candidate.branch,
            nw = name_w,
        );
    }

    if !plan.has_remote {
        println!("\nnote: no 'origin' remote configured — local branches only");
    }
}

/// Render branch copies prune declined to touch. Nothing is skipped silently.
fn render_prune_skips(skipped: &[ops::PruneSkip]) {
    if skipped.is_empty() {
        return;
    }
    println!("\nSkipped:");
    for skip in skipped {
        println!("  {}: {}", skip.branch, skip.reason);
    }
}

/// Render what actually happened, returning the number of failed branches.
fn render_prune_results(results: &[ops::PruneResult], verb: &str) -> usize {
    let mut failures = 0;
    for result in results {
        if result.errors.is_empty() {
            let mut where_ = Vec::new();
            if result.local_deleted {
                where_.push("local");
            }
            if result.remote_deleted {
                where_.push("origin");
            }
            println!("deleted '{}' ({})", result.branch, where_.join(", "));
        } else {
            failures += 1;
            for err in &result.errors {
                eprintln!("failed to delete '{}': {err}", result.branch);
            }
        }
    }
    let deleted = results.len() - failures;
    println!(
        "\n{verb} {deleted} branch{}",
        if deleted == 1 { "" } else { "es" }
    );
    failures
}

fn cmd_list_text(no_tui: bool, deps: bool) -> Result<()> {
    let config = Config::load()?;
    let rolls = branches::list_rolls(&config)?;

    if !no_tui && std::io::stdout().is_terminal() {
        let current = git::current_branch(&config.repo_root)?;
        return tui::rolls::run(config, current, rolls, deps);
    }

    if rolls.is_empty() {
        println!("(no roll branches)");
        return Ok(());
    }

    let name_w = rolls
        .iter()
        .map(|r| r.branch.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let state_w = "⛔ blocked".len();

    let (dep_w, dependant_w) = dep_column_widths(&rolls);

    println!(
        "  {num:>3}  {name:<nw$}  {loc:<3}  {state:<sw$}{deps_hdr}",
        num = "#",
        name = "branch",
        loc = "loc",
        state = "state",
        deps_hdr = if deps {
            format!("  {DEPS_HDR:<dep_w$}  {DEPENDANTS_HDR}")
        } else {
            String::new()
        },
        nw = name_w,
        sw = state_w,
    );
    println!(
        "  ───  {sep_e}  ───  {sep_s}{sep_d}",
        sep_e = "─".repeat(name_w),
        sep_s = "─".repeat(state_w),
        sep_d = if deps {
            format!("  {}  {}", "─".repeat(dep_w), "─".repeat(dependant_w))
        } else {
            String::new()
        },
    );

    for roll in &rolls {
        let cur = if roll.is_current { ">" } else { " " };
        let deps_col = if deps {
            format!(
                "  {:<dep_w$}  {}",
                branches::format_roll_numbers(&roll.deps),
                branches::format_roll_numbers(&roll.dependents),
            )
        } else {
            String::new()
        };
        println!(
            "{cur} {num:>3}  {name:<nw$}  {loc:<3}  {state:<sw$}{deps_col}",
            num = roll.number,
            name = roll.branch,
            loc = roll.location.symbol(),
            state = roll.state.label(),
            nw = name_w,
            sw = state_w,
        );
    }

    Ok(())
}

/// Column headers for the dependency pair, shared by `rf list --no-tui --deps`
/// and `rf status --no-tui` so the two tables read identically.
pub(crate) const DEPS_HDR: &str = "deps";
pub(crate) const DEPENDANTS_HDR: &str = "dependants";

/// Widths for the `deps` / `dependants` columns: wide enough for the header and
/// for the longest comma-joined number list in the table. Trailing whitespace on
/// the last column is trimmed by the caller's format, so only `deps` needs a
/// computed width — `dependants` is returned for the separator rule.
pub(crate) fn dep_column_widths(rolls: &[branches::RollInfo]) -> (usize, usize) {
    let widest = |pick: fn(&branches::RollInfo) -> &Vec<u32>, hdr: &str| {
        rolls
            .iter()
            .map(|r| branches::format_roll_numbers(pick(r)).chars().count())
            .max()
            .unwrap_or(0)
            .max(hdr.chars().count())
    };
    (
        widest(|r| &r.deps, DEPS_HDR),
        widest(|r| &r.dependents, DEPENDANTS_HDR),
    )
}

#[derive(Serialize)]
struct StatusPayload {
    current_branch: String,
    detached_head: bool,
    tier: String,
    clean_working_tree: bool,
    pending_roll_branches: Vec<String>,
    promotion: PromotionReadiness,
}

#[derive(Serialize)]
struct PromotionReadiness {
    description: String,
    ready: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
struct JsonRoll {
    branch: String,
    number: u32,
    state: String,
    location: String,
    is_current: bool,
    /// Roll numbers this roll integrated, and the inverse. Emitted so scripted
    /// consumers see the same dependency graph the TUI draws.
    deps: Vec<u32>,
    dependants: Vec<u32>,
}

fn rolls_for_json(rolls: Vec<branches::RollInfo>) -> Vec<JsonRoll> {
    rolls
        .into_iter()
        .map(|r| JsonRoll {
            branch: r.branch,
            number: r.number,
            state: r.state.label().to_string(),
            location: r.location.symbol().to_string(),
            is_current: r.is_current,
            deps: r.deps,
            dependants: r.dependents,
        })
        .collect()
}
