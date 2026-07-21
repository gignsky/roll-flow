mod cli;
mod core;
mod error;
mod tui;

use std::io::IsTerminal;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Serialize;

use cli::{Cli, Cmd};
use core::{branches, config::Config, git, ops};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Init {
            rolling_branch,
            stable_branch,
            roll_prefix,
            username,
            hosts,
            force,
        } => cmd_init(
            rolling_branch,
            stable_branch,
            roll_prefix,
            username,
            hosts,
            force,
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
        Cmd::Verify { dry_run } => cmd_verify(dry_run)?,
        Cmd::Graduate {
            dry_run,
            force,
            reason,
        } => cmd_graduate(dry_run, force, reason)?,
        Cmd::Promote {
            dry_run,
            force,
            reason,
        } => cmd_promote(dry_run, force, reason)?,
        Cmd::Status { no_tui, json } => {
            if json {
                cmd_status_json()?;
            } else {
                cli::status::run(no_tui)?;
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
        Cmd::Version => println!("{}", env!("CARGO_PKG_VERSION")),
    }

    Ok(())
}

fn cmd_init(
    rolling_branch: Option<String>,
    stable_branch: Option<String>,
    roll_prefix: Option<String>,
    username: Option<String>,
    hosts: Option<String>,
    force: bool,
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

    // Re-running `rf init` is idempotent and non-destructive: it regenerates the
    // config from the repo's actual detected state and only rewrites the file
    // when the result differs. Serialize both sides through the same renderer
    // (`to_toml_string`) so an unchanged re-run is a true no-op — no rewrite,
    // and no `--force` required (issue #16).
    let regenerated = config.to_toml_string()?;
    if cfg_path.exists() {
        let existing = std::fs::read_to_string(&cfg_path)
            .with_context(|| format!("reading existing config at {}", cfg_path.display()))?;
        if !force && existing == regenerated {
            println!("roll-flow config already up to date (no changes)");
            return Ok(());
        }
        config.save()?;
        println!("Updated {} from detected state", cfg_path.display());
    } else {
        config.save()?;
        println!("Initialized roll-flow at {}", cfg_path.display());
    }
    Ok(())
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

fn cmd_verify(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
    let outcome = ops::verify(&config, dry_run)?;
    if outcome.diverged_note {
        println!(
            "note: '{}' has commits not in '{}'; graduation/promotion will create a --no-ff merge",
            outcome.target, outcome.source
        );
    }
    render_gate_notices(&outcome.gate_notices);
    println!(
        "Verification passed: {} -> {}",
        outcome.source, outcome.target
    );
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

fn cmd_promote(dry_run: bool, force: bool, reason: Option<String>) -> Result<()> {
    let force = ops::ForceOpts::new(force, reason)?;
    let config = Config::load()?;
    ops::ensure_clean_state(&config)?;
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
            let outcome = ops::promote(&config, dry_run, &force)?;
            render_gate_notices(&outcome.gate_notices);
            if outcome.dry_run {
                println!(
                    "Dry-run: would promote '{}' into '{}' (--no-ff)",
                    outcome.rolling, outcome.stable
                );
            } else {
                println!("Promoted '{}' into '{}'", outcome.rolling, outcome.stable);
            }
        }
        None => return Err(ops::not_promotable_error(&config, &current)),
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
            ops::GateNotice::Bypassed { gate, code } => eprintln!(
                "warning: gate failed but bypassed (--force): {gate} ({})",
                ops::exit_desc(*code)
            ),
        }
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

    println!(
        "  {num:>3}  {name:<nw$}  {loc:<3}  {state:<sw$}{deps_hdr}",
        num = "#",
        name = "branch",
        loc = "loc",
        state = "state",
        deps_hdr = if deps { "  deps" } else { "" },
        nw = name_w,
        sw = state_w,
    );
    println!(
        "  ───  {sep_e}  ───  {sep_s}{sep_d}",
        sep_e = "─".repeat(name_w),
        sep_s = "─".repeat(state_w),
        sep_d = if deps { "  ────" } else { "" },
    );

    for roll in &rolls {
        let cur = if roll.is_current { ">" } else { " " };
        let deps_col = if deps && !roll.deps.is_empty() {
            format!(
                "  {}",
                roll.deps
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
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
        })
        .collect()
}
