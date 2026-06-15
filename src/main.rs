mod cli;
mod core;
mod error;

use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Serialize;

use cli::{Cli, Cmd};
use core::{branches, config::Config, git};

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
        Cmd::Integrate { .. } => todo!("rf integrate"),
        Cmd::Verify { dry_run } => cmd_verify(dry_run)?,
        Cmd::Promote { dry_run } => cmd_promote(dry_run)?,
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
    if cfg_path.exists() && !force {
        bail!(
            "config already exists at {}; pass --force to overwrite",
            cfg_path.display()
        );
    }
    if !git::ref_exists(&config.repo_root, &config.stable_branch) {
        bail!("stable branch '{}' not found", config.stable_branch);
    }
    if !git::ref_exists(&config.repo_root, &config.rolling_branch) {
        git::run_git(
            &config.repo_root,
            &["branch", &config.rolling_branch, &config.stable_branch],
        )?;
    }

    config.save()?;
    println!("Initialized roll-flow at {}", cfg_path.display());
    Ok(())
}

fn cmd_create(slug: &str, date: Option<String>, dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ensure_clean_state(&config)?;
    if !git::ref_exists(&config.repo_root, &config.rolling_branch) {
        bail!("rolling branch '{}' not found", config.rolling_branch);
    }

    let normalized_slug = normalize_slug(slug)?;
    let mmdd = match date {
        Some(d) => d,
        None => current_mmdd()?,
    };
    validate_mmdd(&mmdd)?;
    let rolls = branches::list_rolls(&config)?;
    let next = rolls.iter().map(|r| r.number).max().unwrap_or(0) + 1;
    let branch_name = format!(
        "{}{}-{}-{}",
        config.roll_prefix, next, mmdd, normalized_slug
    );

    if git::ref_exists(&config.repo_root, &branch_name) {
        bail!("roll branch '{}' already exists", branch_name);
    }

    if dry_run {
        println!(
            "Dry-run: would create '{}' from '{}'",
            branch_name, config.rolling_branch
        );
        return Ok(());
    }

    git::run_git(
        &config.repo_root,
        &["checkout", "-b", &branch_name, &config.rolling_branch],
    )?;
    println!("Created {}", branch_name);
    Ok(())
}

fn cmd_verify(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ensure_clean_state(&config)?;
    let ctx = promotion_context(&config)?;
    validate_fast_forward(&config, &ctx.source, &ctx.target)?;
    run_gates(&config.repo_root, &ctx.gates, dry_run)?;
    println!("Verification passed: {} -> {}", ctx.source, ctx.target);
    Ok(())
}

fn cmd_promote(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ensure_clean_state(&config)?;
    let ctx = promotion_context(&config)?;
    validate_fast_forward(&config, &ctx.source, &ctx.target)?;
    run_gates(&config.repo_root, &ctx.gates, dry_run)?;

    if dry_run {
        println!("Dry-run: would promote {} -> {}", ctx.source, ctx.target);
        return Ok(());
    }

    git::run_git(&config.repo_root, &["checkout", &ctx.target])?;
    let merge_result = git::run_git(&config.repo_root, &["merge", "--ff-only", &ctx.source]);
    let _ = git::run_git(&config.repo_root, &["checkout", &ctx.source]);
    merge_result?;

    println!("Promoted {} -> {}", ctx.source, ctx.target);
    Ok(())
}

fn cmd_status_json() -> Result<()> {
    let config = Config::load()?;
    let current = git::current_branch(&config.repo_root).unwrap_or_else(|_| "HEAD".to_string());
    let detached = git::is_detached_head(&config.repo_root)?;
    let clean = workflow_clean(&config)?;
    let rolls = branches::list_rolls(&config)?;
    let tier = branch_tier(&config, &current, detached);

    let promotion = promotion_context(&config)
        .map(|ctx| PromotionReadiness {
            description: format!("{} -> {}", ctx.source, ctx.target),
            ready: validate_fast_forward(&config, &ctx.source, &ctx.target).is_ok() && clean,
            reason: if !clean {
                Some("working tree must be clean".to_string())
            } else {
                None
            },
        })
        .unwrap_or(PromotionReadiness {
            description: "none".to_string(),
            ready: false,
            reason: Some("current branch is not promotable".to_string()),
        });

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

fn cmd_list_text(no_tui: bool, _deps: bool) -> Result<()> {
    let _ = no_tui;
    let config = Config::load()?;
    let rolls = branches::list_rolls(&config)?;
    if rolls.is_empty() {
        println!("(no roll branches)");
        return Ok(());
    }
    for roll in rolls {
        println!(
            "{}\t{}\t{}\t{}",
            roll.number,
            roll.branch,
            roll.location.symbol(),
            roll.state.label()
        );
    }
    Ok(())
}

fn ensure_clean_state(config: &Config) -> Result<()> {
    if git::is_detached_head(&config.repo_root)? {
        bail!("detached HEAD is not supported");
    }
    if !workflow_clean(config)? {
        bail!("working tree must be clean");
    }
    Ok(())
}

fn workflow_clean(config: &Config) -> Result<bool> {
    if git::working_tree_clean(&config.repo_root)? {
        return Ok(true);
    }
    let status = git::capture_git(&config.repo_root, &["status", "--porcelain"])?;
    let allowed = Config::config_path(&config.repo_root)
        .strip_prefix(&config.repo_root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or(".roll-flow.toml")
        .replace('\\', "/");
    let all_allowed = status.lines().all(|line| {
        let trimmed = line.trim();
        trimmed == format!("?? {allowed}")
    });
    Ok(all_allowed)
}

struct PromotionContext {
    source: String,
    target: String,
    gates: Vec<String>,
}

fn promotion_context(config: &Config) -> Result<PromotionContext> {
    let current = git::current_branch(&config.repo_root)?;
    if current == config.rolling_branch {
        return Ok(PromotionContext {
            source: config.rolling_branch.clone(),
            target: config.stable_branch.clone(),
            gates: config.rolling_to_main_gates.clone(),
        });
    }
    if current.starts_with(&config.roll_prefix) {
        return Ok(PromotionContext {
            source: current,
            target: config.rolling_branch.clone(),
            gates: config.roll_to_rolling_gates.clone(),
        });
    }
    bail!(
        "branch '{}' is not promotable; expected '{}' or '{}*'",
        current,
        config.rolling_branch,
        config.roll_prefix
    )
}

fn validate_fast_forward(config: &Config, source: &str, target: &str) -> Result<()> {
    if !git::ref_exists(&config.repo_root, source) {
        bail!("source branch '{}' not found", source);
    }
    if !git::ref_exists(&config.repo_root, target) {
        bail!("target branch '{}' not found", target);
    }
    let ff_ok = git::is_ancestor(&config.repo_root, target, source)?;
    if !ff_ok {
        bail!(
            "fast-forward-only promotion blocked: '{}' is not ancestor of '{}'",
            target,
            source
        );
    }
    Ok(())
}

fn run_gates(repo: &std::path::Path, gates: &[String], dry_run: bool) -> Result<()> {
    if gates.is_empty() {
        println!("No gates configured");
        return Ok(());
    }
    for gate in gates {
        if dry_run {
            println!("Dry-run gate: {gate}");
            continue;
        }
        let status = Command::new("sh")
            .arg("-c")
            .arg(gate)
            .current_dir(repo)
            .status()
            .with_context(|| format!("failed to run gate: {gate}"))?;
        if !status.success() {
            bail!("gate failed: {gate}");
        }
    }
    Ok(())
}

fn normalize_slug(input: &str) -> Result<String> {
    let slug = input
        .trim()
        .to_lowercase()
        .replace('_', "-")
        .replace(' ', "-");
    if slug.is_empty() {
        bail!("slug cannot be empty");
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("slug may only contain [a-z0-9-]");
    }
    Ok(slug.trim_matches('-').to_string())
}

fn validate_mmdd(mmdd: &str) -> Result<()> {
    if mmdd.len() != 4 || !mmdd.chars().all(|c| c.is_ascii_digit()) {
        bail!("date must be MMDD");
    }
    let month: u32 = mmdd[0..2].parse()?;
    let day: u32 = mmdd[2..4].parse()?;
    if month == 0 || month > 12 || day == 0 || day > 31 {
        bail!("invalid MMDD date");
    }
    Ok(())
}

fn current_mmdd() -> Result<String> {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let format = time::format_description::parse("[month repr:numerical][day]")
        .context("bad date format")?;
    let mmdd = now
        .format(&format)
        .context("failed to format current date")?;
    validate_mmdd(&mmdd)?;
    Ok(mmdd)
}

fn branch_tier(config: &Config, current: &str, detached: bool) -> String {
    if detached {
        return "detached".to_string();
    }
    if current == config.stable_branch {
        return "main".to_string();
    }
    if current == config.rolling_branch {
        return "rolling".to_string();
    }
    if current.starts_with(&config.roll_prefix) {
        return "roll".to_string();
    }
    "other".to_string()
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
