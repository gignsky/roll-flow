//! Workflow operations, extracted from `main.rs` (issue #19).
//!
//! Every function here is pure *logic*: it performs the git/nix work and
//! returns a structured outcome, and it never prints roll-flow's own status
//! messages. The one exception is child-process output from running configured
//! gates, which continues to inherit stdio — that is the subprocess's own
//! output, not ours.
//!
//! The `cmd_*` wrappers in `main.rs` load config, call into here, and render
//! every user-facing line, so both the CLI and the future TUI can drive the
//! exact same implementation.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::core::{branches, config::Config, git};

/// Prefix for the hotfix tier. Parallel to `roll_prefix`, but fixed rather than
/// configurable — hotfixes are a rarely-used sanctioned exception with their own
/// independent numbering.
pub(crate) const HOTFIX_PREFIX: &str = "hotfix/";

// ── Clean-state / working-tree guards ───────────────────────────────────────

pub(crate) fn ensure_clean_state(config: &Config) -> Result<()> {
    if git::is_detached_head(&config.repo_root)? {
        bail!("detached HEAD is not supported");
    }
    if !workflow_clean(config)? {
        bail!("working tree must be clean");
    }
    Ok(())
}

pub(crate) fn workflow_clean(config: &Config) -> Result<bool> {
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

// ── Routing / branch classification ─────────────────────────────────────────

/// Where a merge would go from the current branch.
pub(crate) enum Route {
    /// roll/* -> rolling
    Graduate { roll: String },
    /// rolling -> stable
    Promote,
}

pub(crate) fn infer_route(config: &Config, current: &str) -> Option<Route> {
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

pub(crate) fn not_promotable_error(config: &Config, current: &str) -> anyhow::Error {
    anyhow!(
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

fn classify_merge(repo: &Path, source: &str, target: &str) -> Result<MergeState> {
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

pub(crate) fn branch_tier(config: &Config, current: &str, detached: bool) -> String {
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
    if current.starts_with(HOTFIX_PREFIX) {
        return "hotfix".to_string();
    }
    "other".to_string()
}

// ── Merge execution ─────────────────────────────────────────────────────────

/// Merge `source` into `target` with `--no-ff` and a structured message, then
/// return to the branch we started on. On merge failure the merge is aborted
/// and the original checkout restored.
fn run_merge(
    repo: &Path,
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

// ── Gates ───────────────────────────────────────────────────────────────────

/// A gate that failed but was bypassed under `--force`: the command and its
/// exit code (`None` if terminated by a signal).
pub(crate) struct GateBypass {
    gate: String,
    code: Option<i32>,
}

/// A roll-flow status line about the gate run itself, to be rendered by the
/// caller. Keeps `run_gates` free of `println!` while preserving byte-identical
/// output.
pub(crate) enum GateNotice {
    /// No gates were configured for this transition.
    NoGates,
    /// Dry-run: a gate that would have executed.
    DryRun(String),
    /// A gate failed but was bypassed under `--force`.
    Bypassed { gate: String, code: Option<i32> },
}

/// The result of running the configured gates: any bypassed failures (for the
/// merge trailer) plus the ordered notices the caller should render.
pub(crate) struct GateReport {
    pub bypassed: Vec<GateBypass>,
    pub notices: Vec<GateNotice>,
}

/// Run the configured gates. Without `--force`, a failing gate aborts the
/// operation (the normal hard block). With `--force`, all gates still run but
/// failures are collected so the caller can record them in the merge commit
/// instead of aborting. Child-process output inherits stdio.
fn run_gates(
    repo: &Path,
    gates: &[String],
    dry_run: bool,
    force: &ForceOpts,
) -> Result<GateReport> {
    let mut bypassed = Vec::new();
    let mut notices = Vec::new();
    if gates.is_empty() {
        notices.push(GateNotice::NoGates);
        return Ok(GateReport { bypassed, notices });
    }
    for gate in gates {
        if dry_run {
            notices.push(GateNotice::DryRun(gate.clone()));
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
                notices.push(GateNotice::Bypassed {
                    gate: gate.clone(),
                    code: status.code(),
                });
                bypassed.push(GateBypass {
                    gate: gate.clone(),
                    code: status.code(),
                });
            } else {
                bail!("gate failed: {gate}");
            }
        }
    }
    Ok(GateReport { bypassed, notices })
}

pub(crate) fn exit_desc(code: Option<i32>) -> String {
    code.map(|c| format!("exit {c}"))
        .unwrap_or_else(|| "terminated by signal".to_string())
}

/// `--force` / `--reason` for graduate and promote. `--force` proceeds past
/// failing gates; `--reason` (required with `--force`) is recorded verbatim in
/// the merge commit so every bypass leaves a permanent, auditable trail.
pub(crate) struct ForceOpts {
    enabled: bool,
    reason: Option<String>,
}

impl ForceOpts {
    pub(crate) fn new(enabled: bool, reason: Option<String>) -> Result<Self> {
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

// ── Slug / date helpers ─────────────────────────────────────────────────────

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

// ── create / integrate ──────────────────────────────────────────────────────

/// Outcome of creating a roll or hotfix branch.
pub(crate) struct CreateOutcome {
    pub branch: String,
    pub stable: String,
    pub dry_run: bool,
}

/// Create a roll branch `roll/N-MMDD-slug` off the stable branch.
///
/// A roll is branched off stable, not rolling, so `diff(stable, roll)` is
/// exactly the roll's own changes and dependency detection does not treat
/// everything already on rolling as an implicit dependency.
pub(crate) fn create(
    config: &Config,
    slug: &str,
    date: Option<String>,
    dry_run: bool,
) -> Result<CreateOutcome> {
    if !git::ref_exists(&config.repo_root, &config.stable_branch) {
        bail!("stable branch '{}' not found", config.stable_branch);
    }

    let normalized_slug = normalize_slug(slug)?;
    let mmdd = match date {
        Some(d) => d,
        None => current_mmdd()?,
    };
    validate_mmdd(&mmdd)?;
    let rolls = branches::list_rolls(config)?;
    let next = rolls.iter().map(|r| r.number).max().unwrap_or(0) + 1;
    let branch_name = format!(
        "{}{}-{}-{}",
        config.roll_prefix, next, mmdd, normalized_slug
    );

    if git::ref_exists(&config.repo_root, &branch_name) {
        bail!("roll branch '{}' already exists", branch_name);
    }

    if !dry_run {
        git::run_git(
            &config.repo_root,
            &["checkout", "-b", &branch_name, &config.stable_branch],
        )?;
    }

    Ok(CreateOutcome {
        branch: branch_name,
        stable: config.stable_branch.clone(),
        dry_run,
    })
}

/// Outcome of integrating a branch into the current roll.
pub(crate) struct IntegrateOutcome {
    pub branch: String,
    pub current: String,
}

pub(crate) fn integrate(config: &Config, branch: &str) -> Result<IntegrateOutcome> {
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
    Ok(IntegrateOutcome {
        branch: branch.to_string(),
        current,
    })
}

// ── hotfix ──────────────────────────────────────────────────────────────────

/// Next hotfix number: one past the highest existing `hotfix/N-…` (local +
/// remote), independent of roll numbering.
fn next_hotfix_number(config: &Config) -> Result<u32> {
    let repo = &config.repo_root;
    let pattern = format!("{HOTFIX_PREFIX}*");
    let mut names = git::local_branches(repo, &pattern)?;
    names.extend(git::remote_branches(repo, &pattern)?);
    let max = names
        .iter()
        .filter_map(|b| branches::parse_roll_number(b, HOTFIX_PREFIX))
        .max()
        .unwrap_or(0);
    Ok(max + 1)
}

/// Short reference form used in hotfix merge subjects: the branch
/// `hotfix/N-MMDD-slug` renders as `hotfix/N-slug` (date dropped).
fn hotfix_short_name(branch: &str) -> Option<String> {
    let rest = branch.strip_prefix(HOTFIX_PREFIX)?;
    let mut parts = rest.splitn(3, '-');
    let number = parts.next()?;
    let _mmdd = parts.next()?;
    let slug = parts.next()?;
    if number.is_empty() || slug.is_empty() {
        return None;
    }
    Some(format!("{HOTFIX_PREFIX}{number}-{slug}"))
}

/// Create a hotfix branch off the stable branch: `hotfix/N-MMDD-slug`.
///
/// Mirrors [`create`] but over the `hotfix/` tier, which carries its own
/// independent numbering.
pub(crate) fn hotfix_create(
    config: &Config,
    slug: &str,
    date: Option<String>,
    dry_run: bool,
) -> Result<CreateOutcome> {
    if !git::ref_exists(&config.repo_root, &config.stable_branch) {
        bail!("stable branch '{}' not found", config.stable_branch);
    }

    let normalized_slug = normalize_slug(slug)?;
    let mmdd = match date {
        Some(d) => d,
        None => current_mmdd()?,
    };
    validate_mmdd(&mmdd)?;

    let next = next_hotfix_number(config)?;
    let branch_name = format!("{HOTFIX_PREFIX}{next}-{mmdd}-{normalized_slug}");

    if git::ref_exists(&config.repo_root, &branch_name) {
        bail!("hotfix branch '{}' already exists", branch_name);
    }

    if !dry_run {
        git::run_git(
            &config.repo_root,
            &["checkout", "-b", &branch_name, &config.stable_branch],
        )?;
    }

    Ok(CreateOutcome {
        branch: branch_name,
        stable: config.stable_branch.clone(),
        dry_run,
    })
}

/// Outcome of landing a hotfix.
pub(crate) struct HotfixLandOutcome {
    pub current: String,
    pub stable: String,
    pub rolling: String,
    pub dry_run: bool,
    pub gate_notices: Vec<GateNotice>,
}

/// Land the current hotfix: `--no-ff` merge into the stable branch, then
/// immediately reintegrate stable into rolling so the tiers never silently
/// diverge (the reintegration invariant).
///
/// Hotfixes bypass host verification by design, but still run the configured
/// stable-merge (flake/lint) gates.
pub(crate) fn hotfix_land(config: &Config, dry_run: bool) -> Result<HotfixLandOutcome> {
    let repo = &config.repo_root;
    let current = git::current_branch(repo)?;
    if !current.starts_with(HOTFIX_PREFIX) {
        bail!(
            "rf hotfix --land must be run from a hotfix branch (current: '{}')",
            current
        );
    }
    let short = hotfix_short_name(&current)
        .ok_or_else(|| anyhow!("could not parse hotfix branch name '{current}'"))?;
    let stable = config.stable_branch.clone();
    let rolling = config.rolling_branch.clone();

    // The landing merge (hotfix -> stable) must be viable.
    match classify_merge(repo, &current, &stable)? {
        MergeState::TargetMissing => bail!(target_missing_error(config, &stable)),
        MergeState::UnrelatedHistories => {
            bail!("'{}' and '{}' share no common history", current, stable)
        }
        MergeState::NothingToMerge => bail!(
            "nothing to land: '{}' has no commits that '{}' lacks (already up to date)",
            current,
            stable
        ),
        MergeState::Diverged | MergeState::FastForwardable => {}
    }

    // Reintegration (stable -> rolling) requires the rolling branch to exist.
    if !git::ref_exists(repo, &rolling) {
        bail!(target_missing_error(config, &rolling));
    }

    // Host gating is bypassed by design; the stable-merge gates still run.
    let report = run_gates(
        repo,
        &config.rolling_to_main_gates,
        dry_run,
        &ForceOpts::new(false, None)?,
    )?;

    if dry_run {
        return Ok(HotfixLandOutcome {
            current,
            stable,
            rolling,
            dry_run: true,
            gate_notices: report.notices,
        });
    }

    let land_subject = format!("Hotfix {short} into {stable}");
    run_merge(repo, &current, &stable, &land_subject, None)?;

    let reintegrate_subject = format!("Reintegrate {stable} into {rolling} (hotfix {short})");
    run_merge(repo, &stable, &rolling, &reintegrate_subject, None)?;

    Ok(HotfixLandOutcome {
        current,
        stable,
        rolling,
        dry_run: false,
        gate_notices: report.notices,
    })
}

// ── verify ──────────────────────────────────────────────────────────────────

/// Outcome of `rf verify`.
pub(crate) struct VerifyOutcome {
    pub source: String,
    pub target: String,
    /// Target has commits not in source; the caller should print the advisory
    /// note about the eventual `--no-ff` merge.
    pub diverged_note: bool,
    pub gate_notices: Vec<GateNotice>,
}

pub(crate) fn verify(config: &Config, dry_run: bool) -> Result<VerifyOutcome> {
    let current = git::current_branch(&config.repo_root)?;
    let route =
        infer_route(config, &current).ok_or_else(|| not_promotable_error(config, &current))?;

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

    let mut diverged_note = false;
    match classify_merge(&config.repo_root, &source, &target)? {
        MergeState::TargetMissing => bail!(target_missing_error(config, &target)),
        MergeState::UnrelatedHistories => {
            bail!("'{}' and '{}' share no common history", source, target)
        }
        MergeState::NothingToMerge => bail!(
            "nothing to merge: '{}' has no commits that '{}' lacks (already up to date)",
            source,
            target
        ),
        MergeState::Diverged => diverged_note = true,
        MergeState::FastForwardable => {}
    }

    let report = run_gates(
        &config.repo_root,
        gates,
        dry_run,
        &ForceOpts::new(false, None)?,
    )?;

    Ok(VerifyOutcome {
        source,
        target,
        diverged_note,
        gate_notices: report.notices,
    })
}

// ── graduate ────────────────────────────────────────────────────────────────

/// Outcome of graduating a roll into the rolling branch.
pub(crate) struct GraduateOutcome {
    pub roll: String,
    pub rolling: String,
    pub dry_run: bool,
    pub gate_notices: Vec<GateNotice>,
}

/// Graduate `roll` into the rolling branch with a structured `--no-ff` merge.
/// Shared by `rf graduate` and the `rf promote` fall-through.
pub(crate) fn graduate(
    config: &Config,
    roll: &str,
    dry_run: bool,
    force: &ForceOpts,
) -> Result<GraduateOutcome> {
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

    let report = run_gates(repo, &config.roll_to_rolling_gates, dry_run, force)?;

    if dry_run {
        return Ok(GraduateOutcome {
            roll: roll.to_string(),
            rolling: rolling.clone(),
            dry_run: true,
            gate_notices: report.notices,
        });
    }

    let subject = format!("Graduate {roll} into {rolling}");
    let body = force.trailer(&report.bypassed);
    run_merge(repo, roll, rolling, &subject, body.as_deref())?;
    Ok(GraduateOutcome {
        roll: roll.to_string(),
        rolling: rolling.clone(),
        dry_run: false,
        gate_notices: report.notices,
    })
}

// ── promote ─────────────────────────────────────────────────────────────────

/// Outcome of promoting the rolling branch into stable.
pub(crate) struct PromoteOutcome {
    pub rolling: String,
    pub stable: String,
    pub dry_run: bool,
    pub gate_notices: Vec<GateNotice>,
}

/// Promote the rolling branch into stable with a structured `--no-ff` merge.
pub(crate) fn promote(config: &Config, dry_run: bool, force: &ForceOpts) -> Result<PromoteOutcome> {
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

    let report = run_gates(repo, &config.rolling_to_main_gates, dry_run, force)?;

    if dry_run {
        return Ok(PromoteOutcome {
            rolling: rolling.clone(),
            stable: stable.clone(),
            dry_run: true,
            gate_notices: report.notices,
        });
    }

    let (subject, body) = promote_subject_and_body(config)?;
    let body = ForceOpts::append_trailer(body, force.trailer(&report.bypassed));
    run_merge(repo, rolling, stable, &subject, body.as_deref())?;
    Ok(PromoteOutcome {
        rolling: rolling.clone(),
        stable: stable.clone(),
        dry_run: false,
        gate_notices: report.notices,
    })
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

// ── update ──────────────────────────────────────────────────────────────────

/// Per-roll result of `rf update`.
pub(crate) enum UpdateItem {
    AlreadyUpToDate { roll: String },
    WouldMerge { roll: String, behind: u64 },
    Updated { roll: String },
}

/// Outcome of `rf update`.
pub(crate) enum UpdateOutcome {
    NoActiveRolls,
    Ran {
        stable: String,
        items: Vec<UpdateItem>,
    },
}

pub(crate) fn update(config: &Config, dry_run: bool) -> Result<UpdateOutcome> {
    let repo = &config.repo_root;
    let rolls = branches::list_rolls(config)?;

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
        return Ok(UpdateOutcome::NoActiveRolls);
    }

    let stable = &config.stable_branch;
    let mut items = Vec::new();

    for roll in &active {
        // Commits on stable that the roll doesn't already contain. Zero means
        // stable is already an ancestor of the roll — nothing to merge.
        let behind = git::capture_git(
            repo,
            &[
                "rev-list",
                "--count",
                &format!("{}..{}", roll.branch, stable),
            ],
        )?;
        let behind: u64 = behind.trim().parse().unwrap_or(0);

        if behind == 0 {
            items.push(UpdateItem::AlreadyUpToDate {
                roll: roll.branch.clone(),
            });
            continue;
        }

        if dry_run {
            items.push(UpdateItem::WouldMerge {
                roll: roll.branch.clone(),
                behind,
            });
            continue;
        }

        // Short SHA of the roll tip before the merge, recorded in the body so
        // the merge is self-describing.
        let before = git::capture_git(repo, &["rev-parse", "--short", &roll.branch])?;
        let subject = format!("Update {} from {stable}", roll.branch);
        let body = format!("Brought in: {behind} commits since {before}");

        run_merge(repo, stable, &roll.branch, &subject, Some(&body))?;
        items.push(UpdateItem::Updated {
            roll: roll.branch.clone(),
        });
    }

    Ok(UpdateOutcome::Ran {
        stable: stable.clone(),
        items,
    })
}

// ── promotion readiness (status --json) ─────────────────────────────────────

/// Advisory promotion-readiness data for `status --json` (and the future status
/// TUI). Plain data — `main.rs` maps it into the serialized payload.
pub(crate) struct PromotionReadinessData {
    pub description: String,
    pub ready: bool,
    pub reason: Option<String>,
}

/// Never errors; whenever `ready` is false, `reason` explains why. Deliberately
/// conservative: a diverged target reports not-ready with an explanation even
/// though `rf graduate`/`rf promote` would still succeed.
pub(crate) fn promotion_readiness(
    config: &Config,
    current: &str,
    clean: bool,
    detached: bool,
) -> PromotionReadinessData {
    let not_ready = |description: String, reason: String| PromotionReadinessData {
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
        Ok(MergeState::FastForwardable) => PromotionReadinessData {
            description,
            ready: true,
            reason: None,
        },
        Err(err) => not_ready(description, err.to_string()),
    }
}
