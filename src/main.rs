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
    ensure_clean_state(&config)?;
    // A roll is branched off the stable branch, not rolling: it must start from a
    // clean baseline so `diff(stable, roll)` is exactly the roll's own changes and
    // dependency detection (core/branches.rs) does not treat everything already on
    // rolling as an implicit dependency. Rolling and other rolls become dependencies
    // only when explicitly merged in via `rf integrate`.
    if !git::ref_exists(&config.repo_root, &config.stable_branch) {
        bail!("stable branch '{}' not found", config.stable_branch);
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
            branch_name, config.stable_branch
        );
        return Ok(());
    }

    git::run_git(
        &config.repo_root,
        &["checkout", "-b", &branch_name, &config.stable_branch],
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
    let current = git::current_branch(&config.repo_root)?;
    let route =
        infer_route(&config, &current).ok_or_else(|| not_promotable_error(&config, &current))?;

    let (source, target, gates) = match &route {
        Route::Graduate { roll } => (
            roll.clone(),
            config.rolling_branch.clone(),
            &config.roll_to_rolling_gates,
        ),
        Route::Promote => (
            config.rolling_branch.clone(),
            config.stable_branch.clone(),
            &config.rolling_to_main_gates,
        ),
    };

    match classify_merge(&config.repo_root, &source, &target)? {
        MergeState::TargetMissing => bail!(target_missing_error(&config, &target)),
        MergeState::UnrelatedHistories => {
            bail!("'{}' and '{}' share no common history", source, target)
        }
        MergeState::NothingToMerge => bail!(
            "nothing to merge: '{}' has no commits that '{}' lacks (already up to date)",
            source,
            target
        ),
        MergeState::Diverged => println!(
            "note: '{}' has commits not in '{}'; graduation/promotion will create a --no-ff merge",
            target, source
        ),
        MergeState::FastForwardable => {}
    }

    run_gates(
        &config.repo_root,
        gates,
        dry_run,
        &ForceOpts::new(false, None)?,
    )?;
    println!("Verification passed: {} -> {}", source, target);
    Ok(())
}

fn cmd_graduate(dry_run: bool, force: bool, reason: Option<String>) -> Result<()> {
    let force = ForceOpts::new(force, reason)?;
    let config = Config::load()?;
    ensure_clean_state(&config)?;
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
    graduate_branch(&config, &current, dry_run, &force)
}

fn cmd_promote(dry_run: bool, force: bool, reason: Option<String>) -> Result<()> {
    let force = ForceOpts::new(force, reason)?;
    let config = Config::load()?;
    ensure_clean_state(&config)?;
    let current = git::current_branch(&config.repo_root)?;
    match infer_route(&config, &current) {
        Some(Route::Graduate { roll }) => {
            println!(
                "note: '{}' is a roll branch; graduating into '{}' — use rf graduate directly next time",
                roll, config.rolling_branch
            );
            graduate_branch(&config, &roll, dry_run, &force)
        }
        Some(Route::Promote) => promote_rolling(&config, dry_run, &force),
        None => Err(not_promotable_error(&config, &current)),
    }
}

/// Graduate `roll` into the rolling branch with a structured `--no-ff` merge.
/// Shared by `rf graduate` and the `rf promote` fall-through.
fn graduate_branch(config: &Config, roll: &str, dry_run: bool, force: &ForceOpts) -> Result<()> {
    let repo = &config.repo_root;
    if branches::check_promoted(repo, roll, &config.stable_branch) {
        bail!(
            "'{}' has already been promoted to '{}'",
            roll,
            config.stable_branch
        );
    }

    let rolling = &config.rolling_branch;
    match classify_merge(repo, roll, rolling)? {
        MergeState::TargetMissing => bail!(target_missing_error(config, rolling)),
        MergeState::UnrelatedHistories => {
            bail!("'{}' and '{}' share no common history", roll, rolling)
        }
        MergeState::NothingToMerge => bail!(
            "nothing to graduate: '{}' has no commits that '{}' lacks (already up to date)",
            roll,
            rolling
        ),
        MergeState::Diverged | MergeState::FastForwardable => {}
    }

    let bypassed = run_gates(repo, &config.roll_to_rolling_gates, dry_run, force)?;

    if dry_run {
        println!("Dry-run: would graduate '{roll}' into '{rolling}' (--no-ff)");
        return Ok(());
    }

    let subject = format!("Graduate {roll} into {rolling}");
    let body = force.trailer(&bypassed);
    run_merge(repo, roll, rolling, &subject, body.as_deref())?;
    println!("Graduated '{roll}' into '{rolling}'");
    Ok(())
}

/// Promote the rolling branch into stable with a structured `--no-ff` merge.
fn promote_rolling(config: &Config, dry_run: bool, force: &ForceOpts) -> Result<()> {
    let repo = &config.repo_root;
    let rolling = &config.rolling_branch;
    let stable = &config.stable_branch;

    match classify_merge(repo, rolling, stable)? {
        MergeState::TargetMissing => bail!(target_missing_error(config, stable)),
        MergeState::UnrelatedHistories => {
            bail!("'{}' and '{}' share no common history", rolling, stable)
        }
        MergeState::NothingToMerge => bail!(
            "nothing to promote: '{}' has no commits that '{}' lacks (already up to date)",
            rolling,
            stable
        ),
        MergeState::Diverged | MergeState::FastForwardable => {}
    }

    let bypassed = run_gates(repo, &config.rolling_to_main_gates, dry_run, force)?;

    if dry_run {
        println!("Dry-run: would promote '{rolling}' into '{stable}' (--no-ff)");
        return Ok(());
    }

    let (subject, body) = promote_subject_and_body(config)?;
    let body = ForceOpts::append_trailer(body, force.trailer(&bypassed));
    run_merge(repo, rolling, stable, &subject, body.as_deref())?;
    println!("Promoted '{rolling}' into '{stable}'");
    Ok(())
}

/// Subject and body for a promotion merge. Exactly one graduated roll included
/// → subject names it; otherwise a generic subject with the rolls listed in the
/// body so promoted-state detection can attribute them.
fn promote_subject_and_body(config: &Config) -> Result<(String, Option<String>)> {
    let rolls = branches::list_rolls(config)?;
    let included: Vec<&branches::RollInfo> = rolls
        .iter()
        .filter(|r| {
            matches!(
                r.state,
                branches::RollState::Graduated | branches::RollState::Diverged
            )
        })
        .collect();

    let subject = if included.len() == 1 {
        format!("Promote {} to {}", included[0].branch, config.stable_branch)
    } else {
        format!(
            "Promote {} to {}",
            config.rolling_branch, config.stable_branch
        )
    };

    let body = if included.is_empty() {
        None
    } else {
        let mut body = String::from("Rolls:\n");
        for roll in &included {
            body.push_str(&format!("  {}\n", roll.branch));
        }
        Some(body)
    };

    Ok((subject, body))
}

fn cmd_status_json() -> Result<()> {
    let config = Config::load()?;
    let current = git::current_branch(&config.repo_root).unwrap_or_else(|_| "HEAD".to_string());
    let detached = git::is_detached_head(&config.repo_root)?;
    let clean = workflow_clean(&config)?;
    let rolls = branches::list_rolls(&config)?;
    let tier = branch_tier(&config, &current, detached);

    let promotion = promotion_readiness(&config, &current, clean, detached);

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

/// Where a merge would go from the current branch.
enum Route {
    /// roll/* -> rolling
    Graduate { roll: String },
    /// rolling -> stable
    Promote,
}

fn infer_route(config: &Config, current: &str) -> Option<Route> {
    if current == config.rolling_branch {
        Some(Route::Promote)
    } else if current.starts_with(&config.roll_prefix) {
        Some(Route::Graduate {
            roll: current.to_string(),
        })
    } else {
        None
    }
}

fn not_promotable_error(config: &Config, current: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "branch '{}' is not promotable; expected '{}' or '{}*'",
        current,
        config.rolling_branch,
        config.roll_prefix
    )
}

fn target_missing_error(config: &Config, target: &str) -> String {
    if git::ref_exists(&config.repo_root, &format!("origin/{target}")) {
        format!(
            "target branch '{target}' not found locally; create it with `git branch {target} origin/{target}`"
        )
    } else {
        format!("target branch '{target}' not found")
    }
}

/// Pure git-topology classification of a prospective merge.
#[derive(Debug, PartialEq, Eq)]
enum MergeState {
    /// Target is strictly behind source; a merge is trivially clean.
    FastForwardable,
    /// Both sides have unique commits — mergeable via --no-ff.
    Diverged,
    /// Source is an ancestor of target (or tips are equal).
    NothingToMerge,
    /// No local target branch.
    TargetMissing,
    /// No common merge base.
    UnrelatedHistories,
}

fn classify_merge(repo: &std::path::Path, source: &str, target: &str) -> Result<MergeState> {
    if !git::ref_exists(repo, target) {
        return Ok(MergeState::TargetMissing);
    }
    let source_sha = git::rev_parse(repo, source)
        .with_context(|| format!("source branch '{source}' not found"))?;
    let target_sha = git::rev_parse(repo, target)?;
    if source_sha == target_sha {
        return Ok(MergeState::NothingToMerge);
    }
    if git::is_ancestor(repo, source, target)? {
        return Ok(MergeState::NothingToMerge);
    }
    if git::is_ancestor(repo, target, source)? {
        return Ok(MergeState::FastForwardable);
    }
    match git::merge_base(repo, source, target) {
        Ok(_) => Ok(MergeState::Diverged),
        Err(_) => Ok(MergeState::UnrelatedHistories),
    }
}

/// Merge `source` into `target` with `--no-ff` and a structured message, then
/// return to the branch we started on. On merge failure the merge is aborted
/// and the original checkout restored. Reused by future hotfix/revert flows.
fn run_merge(
    repo: &std::path::Path,
    source: &str,
    target: &str,
    subject: &str,
    body: Option<&str>,
) -> Result<()> {
    let original = git::current_branch(repo)?;

    git::run_git(repo, &["checkout", target])
        .with_context(|| format!("failed to check out '{target}'"))?;

    let mut merge_args = vec!["merge", "--no-ff", "--no-edit", "-m", subject];
    if let Some(body) = body {
        merge_args.push("-m");
        merge_args.push(body);
    }
    merge_args.push(source);

    if let Err(merge_err) = git::run_git(repo, &merge_args) {
        let _ = git::run_git(repo, &["merge", "--abort"]);
        let _ = git::run_git(repo, &["checkout", &original]);
        bail!(
            "merge of '{source}' into '{target}' failed (likely conflicts); \
             the merge was aborted and you are back on '{original}'. \
             Resolve manually: git checkout {target} && git merge --no-ff {source} ({merge_err})"
        );
    }

    git::run_git(repo, &["checkout", &original]).with_context(|| {
        format!("the merge into '{target}' succeeded, but checking out '{original}' again failed")
    })?;
    Ok(())
}

/// A gate that failed but was bypassed under `--force`: the command and its
/// exit code (`None` if terminated by a signal).
struct GateBypass {
    gate: String,
    code: Option<i32>,
}

/// Run the configured gates. Without `--force`, a failing gate aborts the
/// operation (the normal hard block). With `--force`, all gates still run but
/// failures are collected and returned so the caller can record them in the
/// merge commit instead of aborting.
fn run_gates(
    repo: &std::path::Path,
    gates: &[String],
    dry_run: bool,
    force: &ForceOpts,
) -> Result<Vec<GateBypass>> {
    let mut bypassed = Vec::new();
    if gates.is_empty() {
        println!("No gates configured");
        return Ok(bypassed);
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
            if force.enabled {
                eprintln!(
                    "warning: gate failed but bypassed (--force): {gate} ({})",
                    exit_desc(status.code())
                );
                bypassed.push(GateBypass {
                    gate: gate.clone(),
                    code: status.code(),
                });
            } else {
                bail!("gate failed: {gate}");
            }
        }
    }
    Ok(bypassed)
}

fn exit_desc(code: Option<i32>) -> String {
    code.map(|c| format!("exit {c}"))
        .unwrap_or_else(|| "terminated by signal".to_string())
}

/// `--force` / `--reason` for graduate and promote. `--force` proceeds past
/// failing gates; `--reason` (required with `--force`) is recorded verbatim in
/// the merge commit so every bypass leaves a permanent, auditable trail.
struct ForceOpts {
    enabled: bool,
    reason: Option<String>,
}

impl ForceOpts {
    fn new(enabled: bool, reason: Option<String>) -> Result<Self> {
        if enabled && reason.as_deref().map(str::trim).unwrap_or("").is_empty() {
            bail!(
                "--force requires --reason \"<why>\" (the reason is recorded in the merge commit)"
            );
        }
        if !enabled && reason.is_some() {
            bail!("--reason is only valid together with --force");
        }
        Ok(Self { enabled, reason })
    }

    /// Build the `Forced-Bypass:` / `Force-Reason:` trailer for a merge commit,
    /// or `None` when nothing was actually bypassed (a `--force` that hit no
    /// failing gate leaves no marker).
    fn trailer(&self, bypassed: &[GateBypass]) -> Option<String> {
        if bypassed.is_empty() {
            return None;
        }
        let mut s = String::from("Forced-Bypass:\n");
        for b in bypassed {
            s.push_str(&format!("  gate: {:?} ({})\n", b.gate, exit_desc(b.code)));
        }
        let reason = self.reason.as_deref().unwrap_or("(none given)");
        s.push_str(&format!("Force-Reason: {reason}"));
        Some(s)
    }

    /// Append a force trailer to an existing (optional) merge-commit body.
    fn append_trailer(body: Option<String>, trailer: Option<String>) -> Option<String> {
        match (body, trailer) {
            (Some(b), Some(t)) => Some(format!("{b}\n\n{t}")),
            (Some(b), None) => Some(b),
            (None, t) => t,
        }
    }
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

/// Advisory promotion readiness for `status --json` (and the future status
/// TUI). Never errors; whenever `ready` is false, `reason` explains why.
/// Deliberately conservative: a diverged target reports not-ready with an
/// explanation even though `rf graduate`/`rf promote` would still succeed.
fn promotion_readiness(
    config: &Config,
    current: &str,
    clean: bool,
    detached: bool,
) -> PromotionReadiness {
    let not_ready = |description: String, reason: String| PromotionReadiness {
        description,
        ready: false,
        reason: Some(reason),
    };

    if detached {
        return not_ready(
            "none".to_string(),
            "detached HEAD is not supported".to_string(),
        );
    }

    let Some(route) = infer_route(config, current) else {
        return not_ready(
            "none".to_string(),
            not_promotable_error(config, current).to_string(),
        );
    };

    let (source, target, verb) = match &route {
        Route::Graduate { roll } => (roll.clone(), config.rolling_branch.clone(), "graduate"),
        Route::Promote => (
            config.rolling_branch.clone(),
            config.stable_branch.clone(),
            "promote",
        ),
    };
    let description = format!("{source} -> {target}");

    if !clean {
        return not_ready(description, "working tree must be clean".to_string());
    }

    if let Route::Graduate { roll } = &route {
        if branches::check_promoted(&config.repo_root, roll, &config.stable_branch) {
            return not_ready(
                description,
                format!(
                    "'{}' has already been promoted to '{}'",
                    roll, config.stable_branch
                ),
            );
        }
    }

    match classify_merge(&config.repo_root, &source, &target) {
        Ok(MergeState::TargetMissing) => {
            not_ready(description, target_missing_error(config, &target))
        }
        Ok(MergeState::UnrelatedHistories) => not_ready(
            description,
            format!("'{source}' and '{target}' share no common history"),
        ),
        Ok(MergeState::NothingToMerge) => {
            let graduated = matches!(&route, Route::Graduate { roll }
                if branches::check_graduated(&config.repo_root, roll, &config.rolling_branch));
            let reason = if graduated {
                format!("'{source}' is already graduated into '{target}' — nothing new to merge")
            } else {
                format!("nothing to merge: '{source}' has no commits that '{target}' lacks")
            };
            not_ready(description, reason)
        }
        Ok(MergeState::Diverged) => not_ready(
            description,
            format!(
                "'{target}' has commits not in '{source}'; rf {verb} will create a --no-ff merge"
            ),
        ),
        Ok(MergeState::FastForwardable) => PromotionReadiness {
            description,
            ready: true,
            reason: None,
        },
        Err(err) => not_ready(description, err.to_string()),
    }
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
