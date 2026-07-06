mod cli;
mod core;
mod error;
mod tui;

use std::io::IsTerminal;
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
        Cmd::Integrate { branch } => cmd_integrate(&branch)?,
        Cmd::Verify { dry_run } => cmd_verify(dry_run)?,
        Cmd::Graduate { dry_run } => cmd_graduate(dry_run)?,
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

fn cmd_integrate(branch: &str) -> Result<()> {
    let config = Config::load()?;
    ensure_clean_state(&config)?;
    let repo = &config.repo_root;
    let current = git::current_branch(repo)?;
    if !current.starts_with(&config.roll_prefix) {
        bail!(
            "must be on a roll branch to integrate (current: {})",
            current
        );
    }
    if !git::ref_exists(repo, branch) {
        bail!("branch not found: {}", branch);
    }
    git::run_git(repo, &["merge", "--no-ff", branch])?;
    println!("integrated {} into {}", branch, current);
    Ok(())
}

fn cmd_verify(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    ensure_clean_state(&config)?;
    let ctx = promotion_context(&config)?;
    if let Err(reason) = promotion_readiness(&config, &ctx) {
        bail!("{reason}");
    }
    run_gates(&config.repo_root, &ctx.gates, dry_run)?;
    println!("Verification passed: {} -> {}", ctx.source, ctx.target);
    Ok(())
}

fn cmd_graduate(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    let ctx = promotion_context(&config)?;
    if ctx.kind != Direction::Graduate {
        bail!(
            "`rf graduate` graduates a roll into '{}'; you are on '{}'. \
             Use `rf promote` to promote '{}' to '{}'.",
            config.rolling_branch,
            ctx.source,
            config.rolling_branch,
            config.stable_branch
        );
    }
    run_merge(&config, &ctx, dry_run)
}

fn cmd_promote(dry_run: bool) -> Result<()> {
    let config = Config::load()?;
    let ctx = promotion_context(&config)?;
    if ctx.kind != Direction::Promote {
        bail!(
            "`rf promote` promotes '{}' to '{}'; you are on a roll branch ('{}'). \
             Use `rf graduate` to graduate it into '{}'.",
            config.rolling_branch,
            config.stable_branch,
            ctx.source,
            config.rolling_branch
        );
    }
    run_merge(&config, &ctx, dry_run)
}

/// Shared graduate/promote executor: a divergence-tolerant `--no-ff` merge of
/// `ctx.source` into `ctx.target` with the structured commit subject the state
/// detector reads. Leaves the user back on the branch they started from.
fn run_merge(config: &Config, ctx: &PromotionContext, dry_run: bool) -> Result<()> {
    ensure_clean_state(config)?;
    if let Err(reason) = promotion_readiness(config, ctx) {
        bail!("{reason}");
    }
    run_gates(&config.repo_root, &ctx.gates, dry_run)?;

    let subject = ctx.merge_subject();
    if dry_run {
        println!(
            "Dry-run: would {} {} -> {} (--no-ff: \"{}\")",
            ctx.kind.verb(),
            ctx.source,
            ctx.target,
            subject
        );
        return Ok(());
    }

    let repo = &config.repo_root;
    let origin = git::current_branch(repo)?;
    git::run_git(repo, &["checkout", &ctx.target])?;
    let merge = git::run_git(repo, &["merge", "--no-ff", "-m", &subject, &ctx.source]);
    if merge.is_err() {
        // Leave the repo in a coherent state on merge failure (e.g. conflicts).
        let _ = git::run_git(repo, &["merge", "--abort"]);
        let _ = git::run_git(repo, &["checkout", &origin]);
        return merge.map_err(Into::into);
    }
    // Return to where the user was (the roll, or rolling), not the target.
    git::run_git(repo, &["checkout", &origin])?;

    println!("{} {} -> {}", ctx.kind.past_tense(), ctx.source, ctx.target);
    Ok(())
}

fn cmd_status_json() -> Result<()> {
    let config = Config::load()?;
    let current = git::current_branch(&config.repo_root).unwrap_or_else(|_| "HEAD".to_string());
    let detached = git::is_detached_head(&config.repo_root)?;
    let clean = workflow_clean(&config)?;
    let rolls = branches::list_rolls(&config)?;
    let tier = branch_tier(&config, &current, detached);

    let promotion = match promotion_context(&config) {
        Ok(ctx) => {
            // Report the *first* blocking condition so `reason` is never null
            // while `ready` is false (issue #13).
            let reason = if !clean {
                Some("working tree must be clean".to_string())
            } else {
                promotion_readiness(&config, &ctx).err()
            };
            PromotionReadiness {
                description: format!("{} -> {}", ctx.source, ctx.target),
                ready: reason.is_none(),
                reason,
            }
        }
        Err(e) => PromotionReadiness {
            description: "none".to_string(),
            ready: false,
            reason: Some(e.to_string()),
        },
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
    let repo = &config.repo_root;
    let rolls = branches::list_rolls(&config)?;

    let active: Vec<_> = rolls
        .iter()
        .filter(|r| {
            matches!(
                r.state,
                branches::RollState::Active | branches::RollState::Blocked
            ) && matches!(
                r.location,
                branches::BranchLocation::Local | branches::BranchLocation::Both
            )
        })
        .collect();

    if active.is_empty() {
        println!("no active local rolls to update");
        return Ok(());
    }

    let current = git::current_branch(repo)?;

    for roll in &active {
        if dry_run {
            println!(
                "dry-run: would merge '{}' into '{}'",
                config.stable_branch, roll.branch
            );
            continue;
        }
        git::run_git(repo, &["checkout", &roll.branch])?;
        git::run_git(repo, &["merge", "--no-ff", &config.stable_branch])?;
        println!("updated '{}' with '{}'", roll.branch, config.stable_branch);
    }

    if !dry_run && !active.is_empty() {
        git::run_git(repo, &["checkout", &current])?;
    }

    Ok(())
}

fn cmd_list_text(no_tui: bool, deps: bool) -> Result<()> {
    let config = Config::load()?;
    let rolls = branches::list_rolls(&config)?;

    if !no_tui && std::io::stdout().is_terminal() {
        let current = git::current_branch(&config.repo_root)?;
        return tui::rolls::run(tui::rolls::TuiContext {
            config: &config,
            current_branch: &current,
            rolls: &rolls,
            show_deps: deps,
        });
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// roll/* → rolling
    Graduate,
    /// rolling → main
    Promote,
}

impl Direction {
    fn verb(self) -> &'static str {
        match self {
            Direction::Graduate => "graduate",
            Direction::Promote => "promote",
        }
    }

    fn past_tense(self) -> &'static str {
        match self {
            Direction::Graduate => "Graduated",
            Direction::Promote => "Promoted",
        }
    }
}

struct PromotionContext {
    kind: Direction,
    source: String,
    target: String,
    gates: Vec<String>,
}

impl PromotionContext {
    /// The structured merge subject the state detector reads back
    /// (`branches::extract_graduated_branch` / promoted-reachability).
    fn merge_subject(&self) -> String {
        match self.kind {
            Direction::Graduate => format!("Graduate {} into {}", self.source, self.target),
            Direction::Promote => format!("Promote {} to {}", self.source, self.target),
        }
    }
}

/// Determine what the current branch would graduate/promote into. Errors only
/// when the branch is neither the rolling branch nor a roll branch.
fn promotion_context(config: &Config) -> Result<PromotionContext> {
    let current = git::current_branch(&config.repo_root)?;
    if current == config.rolling_branch {
        return Ok(PromotionContext {
            kind: Direction::Promote,
            source: config.rolling_branch.clone(),
            target: config.stable_branch.clone(),
            gates: config.rolling_to_main_gates.clone(),
        });
    }
    if current.starts_with(&config.roll_prefix) {
        return Ok(PromotionContext {
            kind: Direction::Graduate,
            source: current,
            target: config.rolling_branch.clone(),
            gates: config.roll_to_rolling_gates.clone(),
        });
    }
    bail!(
        "branch '{}' is not promotable; run from a roll branch ('{}*') to graduate, \
         or from '{}' to promote",
        current,
        config.roll_prefix,
        config.rolling_branch
    )
}

/// Whether `ctx.source` can be merged into `ctx.target` right now. Unlike the
/// old fast-forward gate, ordinary divergence is fine — the `--no-ff` merge
/// handles it. Returns `Err(reason)` naming a genuinely-bad state (missing
/// branch, nothing to merge, unrelated histories) so callers can surface *why*.
fn promotion_readiness(config: &Config, ctx: &PromotionContext) -> std::result::Result<(), String> {
    let repo = &config.repo_root;
    if !git::ref_exists(repo, &ctx.source) {
        return Err(format!("source branch '{}' not found", ctx.source));
    }
    if !git::ref_exists(repo, &ctx.target) {
        return Err(format!("target branch '{}' not found", ctx.target));
    }
    // Nothing to do if the target already contains every commit on the source.
    let ahead = git::capture_git(
        repo,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", ctx.target, ctx.source),
        ],
    )
    .ok()
    .and_then(|s| s.trim().parse::<u32>().ok())
    .unwrap_or(0);
    if ahead == 0 {
        return Err(format!(
            "nothing to {}: '{}' has no commits beyond '{}'",
            ctx.kind.verb(),
            ctx.source,
            ctx.target
        ));
    }
    // Refuse to knit together histories that share no common ancestor.
    if git::capture_git(repo, &["merge-base", &ctx.source, &ctx.target]).is_err() {
        return Err(format!(
            "'{}' and '{}' have unrelated histories",
            ctx.source, ctx.target
        ));
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
    let slug = input.trim().to_lowercase().replace(['_', ' '], "-");
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
