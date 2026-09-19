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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::core::version::{BumpLevel, Semver, VersionCheck, VersionStatus};
use crate::core::{branches, config::Config, git, proc, version};

/// Prefix for the hotfix tier. Parallel to `roll_prefix`, but fixed rather than
/// configurable — hotfixes are a rarely-used sanctioned exception with their own
/// independent numbering.
pub(crate) use crate::core::branches::HOTFIX_PREFIX;

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

/// Make the merge `target` usable as a local branch and return the ref to
/// classify/merge against.
///
/// When the target exists only as `origin/<target>` (a fresh checkout that has
/// never had the branch locally — issue #32), create the local branch from the
/// remote so `run_merge` can check it out and merge into it. In `dry_run` mode
/// nothing is created; the remote ref is returned so a preview still reflects
/// reality without mutating the repo. Errors (with the existing actionable
/// message) when the branch exists neither locally nor on `origin`.
fn ensure_local_target(config: &Config, target: &str, dry_run: bool) -> Result<String> {
    let repo = &config.repo_root;
    if git::ref_exists(repo, target) {
        return Ok(target.to_string());
    }
    let remote = format!("origin/{target}");
    if git::ref_exists(repo, &remote) {
        if dry_run {
            return Ok(remote);
        }
        git::run_git(repo, &["branch", target, &remote])
            .with_context(|| format!("failed to create local branch '{target}' from '{remote}'"))?;
        return Ok(target.to_string());
    }
    bail!(target_missing_error(config, target))
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

/// Stage a `--no-ff` merge of `source` into `target` without committing it,
/// hand the resulting worktree to `run_step`, and commit only if that closure
/// succeeds.
///
/// This is [`run_merge`] split at the seam, and it exists so promotion gates can
/// test *what will land on stable* rather than whatever happened to be checked
/// out when `rf promote` was typed. Splitting it matters most for per-roll
/// promotion, where each roll is merged and gated in turn: gating the pre-merge
/// tree would run the same commands against the same content N times and prove
/// nothing about the intermediate states.
///
/// Failure handling matches `run_merge` exactly — `git merge --abort`, restore
/// the original checkout, and report how to finish by hand — and applies to a
/// failing `run_step` as well as a conflicting merge, so a rejected step leaves
/// no partial commit and no `MERGE_HEAD` behind.
fn merge_gated<T>(
    repo: &Path,
    source: &str,
    target: &str,
    subject: &str,
    body: Option<&str>,
    run_step: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let original = git::current_branch(repo)?;

    git::run_git(repo, &["checkout", target])
        .with_context(|| format!("failed to check out '{target}'"))?;

    // `--no-ff --no-commit` leaves MERGE_HEAD set and the result staged, which
    // is precisely the state the gates need to see.
    if let Err(merge_err) = git::run_git(
        repo,
        &["merge", "--no-ff", "--no-commit", "--no-edit", source],
    ) {
        unwind_merge(repo, &original);
        bail!(
            "merge of '{source}' into '{target}' failed (likely conflicts); \
             the merge was aborted and you are back on '{original}'. \
             Resolve manually: git checkout {target} && git merge --no-ff {source} ({merge_err})"
        );
    }

    let outcome = match run_step() {
        Ok(outcome) => outcome,
        Err(step_err) => {
            unwind_merge(repo, &original);
            return Err(step_err.context(format!(
                "promotion of '{source}' into '{target}' was rolled back; \
                 the merge was aborted and you are back on '{original}'"
            )));
        }
    };

    // Gates run arbitrary shell commands. One that rewrites a tracked file
    // (`cargo update` is a configured gate in this very repo) leaves changes
    // that `git commit` would silently drop, producing a merge commit whose
    // content the gates never actually saw, plus a dirty tree that blocks the
    // next operation. Refuse rather than commit something unverified.
    if let Some(dirty) = unstaged_tracked_changes(repo) {
        unwind_merge(repo, &original);
        bail!(
            "a gate modified tracked files while '{source}' was staged for merge into \
             '{target}', so the merge would not contain what the gates checked. \
             The merge was aborted and you are back on '{original}'. Modified: {dirty}"
        );
    }

    let mut commit_args = vec!["commit", "--no-edit", "-m", subject];
    if let Some(body) = body {
        commit_args.push("-m");
        commit_args.push(body);
    }
    if let Err(commit_err) = git::run_git(repo, &commit_args) {
        unwind_merge(repo, &original);
        bail!(
            "gates passed but committing the merge of '{source}' into '{target}' failed; \
             the merge was aborted and you are back on '{original}' ({commit_err})"
        );
    }

    git::run_git(repo, &["checkout", &original]).with_context(|| {
        format!("the merge into '{target}' succeeded, but checking out '{original}' again failed")
    })?;
    Ok(outcome)
}

/// Abandon an in-progress merge and return to `original`. Both steps are
/// best-effort: this runs on paths that are already reporting a failure, and
/// masking that failure with a cleanup error would hide the real cause.
fn unwind_merge(repo: &Path, original: &str) {
    let _ = git::run_git(repo, &["merge", "--abort"]);
    let _ = git::run_git(repo, &["checkout", original]);
}

/// Tracked files modified in the worktree but not staged, as a short printable
/// list, or `None` when there are none. Used to catch gates that mutate the
/// tree mid-merge. Untracked files are ignored — a gate dropping a build
/// artifact is noise, not a correctness problem.
fn unstaged_tracked_changes(repo: &Path) -> Option<String> {
    let out = git::capture_git(repo, &["diff", "--name-only"]).ok()?;
    let files: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    if files.is_empty() {
        return None;
    }
    Some(files.join(", "))
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
    /// Dry-run: a per-host gate that would have executed (already `{host}`-substituted).
    DryRunHost(String),
    /// A gate failed but was bypassed under `--force`.
    Bypassed { gate: String, code: Option<i32> },
}

/// The result of running the configured gates: any bypassed failures (for the
/// merge trailer) plus the ordered notices the caller should render.
pub(crate) struct GateReport {
    pub bypassed: Vec<GateBypass>,
    pub notices: Vec<GateNotice>,
}

/// Build the `sh -c <gate>` command a gate runs as.
///
/// Gates go through [`proc::run`] rather than `Command::status()` so their
/// output reaches the TUI's panel when one is listening. That also decides how
/// stdin is handled: with a sink installed nobody is reading the terminal on the
/// gate's behalf, so `proc` hands it `/dev/null` and a gate that tries to prompt
/// fails fast instead of hanging behind the alternate screen.
fn gate_command(gate: &str, repo: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(gate).current_dir(repo);
    cmd
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
        let status = proc::run(&mut gate_command(gate, repo))
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

// ── Per-host verification gates (issue #106) ────────────────────────────────

/// Outcome of the host gates for a single active host. A host PASSES iff every
/// one of its gates exited 0 — i.e. `failures` is empty.
pub(crate) struct HostResult {
    pub host: String,
    pub failures: Vec<GateBypass>,
}

impl HostResult {
    pub(crate) fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The result of running the per-host verification gates across every active
/// host: the per-host pass/fail breakdown, any bypassed failures (for the merge
/// trailer, only populated under `--force`), and the notices to render.
pub(crate) struct HostReport {
    pub results: Vec<HostResult>,
    pub notices: Vec<GateNotice>,
    pub bypassed: Vec<GateBypass>,
}

impl HostReport {
    /// Active hosts whose gates failed (non-empty only when a host did not pass).
    pub(crate) fn failed_hosts(&self) -> Vec<String> {
        self.results
            .iter()
            .filter(|r| !r.passed())
            .map(|r| r.host.clone())
            .collect()
    }
}

/// Run the configured host gates once per **active** host, substituting the
/// `{host}` token in each template. A host passes iff all its gates exit 0.
///
/// Mirrors [`run_gates`] semantics: `dry_run` prints (via notices) and runs
/// nothing; under `--force` a failing gate is recorded as a bypass rather than
/// blocking. Unlike `run_gates`, this never bails on a failing gate on its own —
/// it always returns the full per-host breakdown so the caller can name every
/// failed host and still surface the hosts that passed. The caller enforces the
/// hard block when not forcing.
///
/// If `host_gates` is empty or there are no active hosts, the report is empty
/// (a total no-op — roll-flow's own repo, with no host gates, is unaffected).
fn run_host_gates(config: &Config, dry_run: bool, force: &ForceOpts) -> Result<HostReport> {
    let mut results = Vec::new();
    let mut notices = Vec::new();
    let mut bypassed = Vec::new();

    let hosts = config.active_hosts();
    if config.host_gates.is_empty() || hosts.is_empty() {
        return Ok(HostReport {
            results,
            notices,
            bypassed,
        });
    }

    for host in hosts {
        let mut failures = Vec::new();
        for template in &config.host_gates {
            let cmd = template.replace("{host}", &host);
            if dry_run {
                notices.push(GateNotice::DryRunHost(cmd));
                continue;
            }
            let status = proc::run(&mut gate_command(&cmd, &config.repo_root))
                .with_context(|| format!("failed to run host gate: {cmd}"))?;
            if !status.success() {
                if force.enabled {
                    notices.push(GateNotice::Bypassed {
                        gate: cmd.clone(),
                        code: status.code(),
                    });
                    bypassed.push(GateBypass {
                        gate: cmd.clone(),
                        code: status.code(),
                    });
                }
                failures.push(GateBypass {
                    gate: cmd,
                    code: status.code(),
                });
            }
        }
        results.push(HostResult { host, failures });
    }

    Ok(HostReport {
        results,
        notices,
        bypassed,
    })
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
    let short = branches::hotfix_short_name(&current)
        .ok_or_else(|| anyhow!("could not parse hotfix branch name '{current}'"))?;
    let stable = config.stable_branch.clone();
    let rolling = config.rolling_branch.clone();

    // The landing merge (hotfix -> stable) must be viable. Bring stable local
    // if it only exists on origin (issue #32).
    let stable_ref = ensure_local_target(config, &stable, dry_run)?;
    match classify_merge(repo, &current, &stable_ref)? {
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

    // Reintegration (stable -> rolling) requires the rolling branch; bring it
    // local from origin when needed, mirroring the stable target above.
    ensure_local_target(config, &rolling, dry_run)?;

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

// ── Version gate and release tagging ────────────────────────────────────────

/// What happened to the release tag during a promotion.
pub(crate) enum TagOutcome {
    /// A new annotated tag was created on the promotion merge commit.
    Created { tag: String, sha: String },
    /// The tag was already present, so nothing was done. Mirrors the
    /// idempotency of `.github/workflows/tag-on-main.yml`, which skips when the
    /// tag exists rather than failing.
    Existed { tag: String },
    /// Dry-run: the tag that would be created.
    WouldCreate { tag: String },
    /// Dry-run: a tag would be created, but the version still needs bumping, so
    /// its name depends on the bump and cannot be named yet.
    WouldCreateAfterBump,
    /// Tagging was disabled (`--no-tag` / `tag_on_promote = false`) or there is
    /// no version to tag (no `Cargo.toml`).
    Skipped,
}

impl TagOutcome {
    /// The status line describing this outcome, or `None` when there is nothing
    /// worth saying (tagging was skipped entirely). Shared by the CLI and TUI
    /// renderers so both report a release identically.
    pub(crate) fn describe(&self) -> Option<String> {
        match self {
            TagOutcome::Created { tag, sha } => Some(format!("Tagged {tag} on {}", short_sha(sha))),
            TagOutcome::Existed { tag } => {
                Some(format!("note: tag {tag} already exists - not re-tagging"))
            }
            TagOutcome::WouldCreate { tag } => {
                Some(format!("Dry-run: would tag {tag} on the merge commit"))
            }
            TagOutcome::WouldCreateAfterBump => Some(
                "Dry-run: would tag the merge commit with the version once it is bumped"
                    .to_string(),
            ),
            TagOutcome::Skipped => None,
        }
    }

    /// The tag name when one was just created, for the caller's push prompt.
    pub(crate) fn created_tag(&self) -> Option<&str> {
        match self {
            TagOutcome::Created { tag, .. } => Some(tag.as_str()),
            _ => None,
        }
    }
}

/// Abbreviate a full SHA for display, matching git's default short length.
fn short_sha(sha: &str) -> &str {
    let end = sha.len().min(7);
    &sha[..end]
}

/// Compare the crate version on `source_ref` against `target_ref`, honouring
/// the `version_gate` config switch.
///
/// Returns `NotApplicable` — never an error — when the gate is off or the repo
/// has no `Cargo.toml`, so repos that do not version this way (the dotfiles
/// repo roll-flow was built for) are entirely unaffected.
pub(crate) fn version_check(
    config: &Config,
    source_ref: &str,
    target_ref: &str,
) -> Result<VersionCheck> {
    if !config.version_gate {
        return Ok(VersionCheck::not_applicable());
    }
    Ok(version::check(&config.repo_root, source_ref, target_ref)?)
}

/// The hard error a failing version gate produces. Shared by `verify` and
/// `promote` so both routes explain the failure — and the way out — identically.
pub(crate) fn version_gate_error(
    check: &VersionCheck,
    source: &str,
    target: &str,
) -> anyhow::Error {
    let head = check
        .head
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<unreadable>".to_string());
    let base = check
        .base
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<unreadable>".to_string());
    match check.status {
        VersionStatus::Unchanged => anyhow!(
            "Cargo.toml version ({head}) on '{source}' is unchanged from '{target}'; \
             every promotion must carry a version bump. \
             Re-run with --bump <patch|minor|major>, bump it by hand, or \
             --force --reason \"<why>\" to override"
        ),
        VersionStatus::Lower => anyhow!(
            "Cargo.toml version ({head}) on '{source}' is lower than '{target}' ({base}); \
             it must be bumped above the branch it is merging into"
        ),
        VersionStatus::Unreadable => {
            anyhow!("could not read a version from Cargo.toml (head='{head}', base='{base}')")
        }
        VersionStatus::Ok | VersionStatus::NotApplicable => {
            anyhow!("version check passed unexpectedly")
        }
    }
}

/// Raise the crate version, refresh the lockfile, and commit both.
///
/// The commit lands on whatever branch is checked out, which callers guarantee
/// is the branch being merged *from*. Deliberately a separate step the CLI runs
/// **before** the gates: a bump rewrites `Cargo.lock` too, and
/// `rolling_to_main_gates` contains `cargo update --workspace --locked`, which
/// would fail against a stale lockfile if the bump came afterwards.
pub(crate) fn apply_version_bump(
    config: &Config,
    level: BumpLevel,
    reason: &str,
) -> Result<(Semver, Semver)> {
    let repo = &config.repo_root;
    let current = version::read_version(repo)?
        .ok_or_else(|| anyhow!("no readable version in Cargo.toml to bump"))?;
    let next = current.bump(level);

    version::write_version(repo, next)?;
    refresh_lockfile(repo);

    let message = format!("chore(release): bump version to {next} for {reason}");
    git::commit_paths(repo, &["Cargo.toml", "Cargo.lock"], &message)
        .with_context(|| format!("failed to commit the version bump to {next}"))?;
    Ok((current, next))
}

/// Best-effort `Cargo.lock` refresh after a version rewrite.
///
/// A workspace member's own version appears in the lockfile, so it goes stale
/// the moment `Cargo.toml` changes. Failure is deliberately ignored: there may
/// be no lockfile, no network, or no cargo at all, and the configured
/// `cargo update --workspace --locked` gate is the real enforcement. Keeping it
/// non-fatal also lets the integration tests run offline against fixture
/// manifests that are not real crates.
fn refresh_lockfile(repo: &Path) {
    if !repo.join("Cargo.lock").exists() {
        return;
    }
    let offline = Command::new("cargo")
        .args(["update", "--workspace", "--offline"])
        .current_dir(repo)
        .status();
    if matches!(&offline, Ok(s) if s.success()) {
        return;
    }
    let _ = Command::new("cargo")
        .args(["update", "--workspace"])
        .current_dir(repo)
        .status();
}

/// Create the release tag for a completed promotion.
///
/// Tags the merge commit by SHA rather than by branch name: `run_merge` has
/// already returned to the branch we started on, and a SHA cannot drift.
fn tag_release(
    config: &Config,
    check: &VersionCheck,
    enabled: bool,
    included: &[String],
) -> Result<TagOutcome> {
    if !enabled || !config.tag_on_promote {
        return Ok(TagOutcome::Skipped);
    }
    let Some(version) = check.head else {
        return Ok(TagOutcome::Skipped);
    };
    let repo = &config.repo_root;
    let tag = version.tag();
    if git::tag_exists(repo, &tag) {
        return Ok(TagOutcome::Existed { tag });
    }
    let sha = git::rev_parse(repo, &config.stable_branch)?;
    let message = tag_message(&tag, included);
    git::create_annotated_tag(repo, &tag, &message, &sha)
        .with_context(|| format!("failed to create tag {tag}"))?;
    Ok(TagOutcome::Created { tag, sha })
}

/// Annotated-tag message. The subject is byte-identical to the one
/// `tag-on-main.yml` writes, so tags made by `rf` and by CI stay uniform; the
/// rolls this release carries are listed underneath.
fn tag_message(tag: &str, included: &[String]) -> String {
    let mut msg = format!("Release {tag}");
    if !included.is_empty() {
        msg.push_str("\n\nRolls:\n");
        for roll in included {
            msg.push_str(&format!("  {roll}\n"));
        }
    }
    msg
}

/// Branch names of the rolls a promotion carries: those graduated (or
/// re-graduated after diverging) into rolling but not yet on stable.
fn included_rolls(config: &Config) -> Result<Vec<String>> {
    Ok(branches::list_rolls(config)?
        .into_iter()
        .filter(|r| {
            matches!(
                r.state,
                branches::RollState::Graduated | branches::RollState::Diverged
            )
        })
        .map(|r| r.branch)
        .collect())
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
    /// Per-host verification results (empty when no host gates / no active hosts).
    pub host_results: Vec<HostResult>,
    /// Dry-run notices for the host gates.
    pub host_notices: Vec<GateNotice>,
    /// Active hosts whose gates failed. Non-empty ⇒ verify should fail; the CLI
    /// still renders the per-host summary (including the hosts that passed) first.
    pub failed_hosts: Vec<String>,
    /// Crate-version comparison of source against target. `NotApplicable` when
    /// the repo has no `Cargo.toml` or the gate is disabled.
    pub version: VersionCheck,
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

    // Checked before the gates so an unbumped version fails in milliseconds
    // rather than after a full `cargo test` run. Only the promotion route
    // carries the bump requirement — graduating a roll into rolling is
    // deliberately out of scope, matching what `rf promote` enforces.
    let version = match route {
        Route::Promote => version_check(config, &source, &target)?,
        Route::Graduate { .. } => VersionCheck::not_applicable(),
    };

    let report = run_gates(
        &config.repo_root,
        gates,
        dry_run,
        &ForceOpts::new(false, None)?,
    )?;

    // Host gates run on both routes (issue #106). `rf verify` never forces, so a
    // failed host is recorded and surfaced to the CLI, which turns it into a hard
    // error after printing the per-host summary.
    let host_report = run_host_gates(config, dry_run, &ForceOpts::new(false, None)?)?;
    let failed_hosts = host_report.failed_hosts();

    Ok(VerifyOutcome {
        source,
        target,
        diverged_note,
        gate_notices: report.notices,
        host_results: host_report.results,
        host_notices: host_report.notices,
        failed_hosts,
        version,
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
    let rolling_ref = ensure_local_target(config, rolling, dry_run)?;
    match classify_merge(repo, roll, &rolling_ref)? {
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

/// What a `rf promote` invocation should carry to stable.
///
/// Both variants merge a commit that is *on* the rolling branch, so the
/// "`main` only ever receives merges from `rolling`" invariant holds for
/// per-roll promotion too — a roll branch is never merged into stable directly.
pub(crate) enum PromoteTarget {
    /// Everything rolling has: one merge, one gate run.
    Rolling,
    /// Named graduated rolls. Stable is advanced to each roll's graduation merge
    /// in turn — one merge and one gate run per roll — which is why promoting a
    /// roll necessarily carries whatever graduated ahead of it, and why the
    /// result keeps stable a prefix of rolling rather than a divergent line.
    Rolls(Vec<String>),
}

/// One merge performed by [`promote`]: the whole rolling branch, or one roll.
pub(crate) struct PromoteStep {
    /// The roll this step promoted, or `None` for a whole-rolling promotion.
    pub roll: Option<String>,
    /// What was merged — a branch name, or a graduation commit hash. Reported in
    /// dry-runs, where naming the commit is the only way to show that a per-roll
    /// promotion merges a point on rolling rather than the roll branch.
    pub source: String,
    pub gate_notices: Vec<GateNotice>,
    /// Per-host verification results (empty when no host gates / no active hosts).
    pub host_results: Vec<HostResult>,
    /// Dry-run notices for the host gates.
    pub host_notices: Vec<GateNotice>,
    /// Crate-version comparison of rolling against stable.
    pub version: VersionCheck,
    /// What happened to the `vX.Y.Z` release tag.
    pub tag: TagOutcome,
}

/// Outcome of promoting into stable.
pub(crate) struct PromoteOutcome {
    pub rolling: String,
    pub stable: String,
    pub dry_run: bool,
    /// One entry per merge, in the order they were applied.
    pub steps: Vec<PromoteStep>,
    /// Rolls that were named but needed no work, with the reason — already
    /// promoted, or already contained in stable via an earlier step.
    pub skipped: Vec<SkippedRoll>,
}

/// A named roll that needed no promotion, and why.
pub(crate) struct SkippedRoll {
    pub roll: String,
    pub reason: String,
}

/// Promote into stable with structured `--no-ff` merges.
///
/// [`PromoteTarget::Rolling`] is one merge behind one gate run.
/// [`PromoteTarget::Rolls`] is one merge behind one gate run *per roll*, applied
/// in graduation order, so each intermediate state of stable is verified rather
/// than only the end state.
pub(crate) fn promote(
    config: &Config,
    target: &PromoteTarget,
    dry_run: bool,
    force: &ForceOpts,
    tag: bool,
) -> Result<PromoteOutcome> {
    let rolling = &config.rolling_branch;
    let stable = &config.stable_branch;

    let stable_ref = ensure_local_target(config, stable, dry_run)?;
    let plan = match target {
        PromoteTarget::Rolling => PromotePlan {
            steps: vec![plan_rolling_step(config, &stable_ref)?],
            skipped: Vec::new(),
        },
        PromoteTarget::Rolls(rolls) => plan_roll_steps(config, rolls, &stable_ref)?,
    };

    let mut steps = Vec::new();
    for step in plan.steps {
        steps.push(run_promote_step(
            config,
            &stable_ref,
            step,
            dry_run,
            force,
            tag,
        )?);
    }

    Ok(PromoteOutcome {
        rolling: rolling.clone(),
        stable: stable.clone(),
        dry_run,
        steps,
        skipped: plan.skipped,
    })
}

/// A resolved, ready-to-merge promotion step.
struct PlannedStep {
    roll: Option<String>,
    source: String,
    subject: String,
    body: Option<String>,
    /// The rolls this step carries, listed in the annotated tag message.
    /// Captured at planning time, before any merge: afterwards these rolls read
    /// as promoted rather than graduated, so the list would come back empty.
    included: Vec<String>,
}

/// The steps a promotion will perform, plus the rolls it found nothing to do
/// for. Named rather than a tuple because both halves are reported to the user.
struct PromotePlan {
    steps: Vec<PlannedStep>,
    skipped: Vec<SkippedRoll>,
}

/// Plan the single step of a whole-rolling promotion, reusing the existing
/// subject/body rules so the commit shape on stable is unchanged.
fn plan_rolling_step(config: &Config, stable_ref: &str) -> Result<PlannedStep> {
    let repo = &config.repo_root;
    let rolling = &config.rolling_branch;
    let stable = &config.stable_branch;

    match classify_merge(repo, rolling, stable_ref)? {
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

    let included = included_rolls(config)?;
    let (subject, body) = promote_subject_and_body(config, &included);
    Ok(PlannedStep {
        roll: None,
        source: rolling.clone(),
        subject,
        body,
        included,
    })
}

/// Resolve named rolls into ordered merge steps, plus the rolls that need no
/// work. Each step merges the roll's *graduation commit on rolling*, not its
/// branch, which is what keeps stable a prefix of rolling.
///
/// Ordering is by position on rolling, oldest first: promoting out of graduation
/// order is not expressible, since advancing stable to a later graduation
/// necessarily includes the earlier ones. Sorting rather than rejecting means
/// `--roll b --roll a` does the sane thing instead of erroring on argument
/// order.
fn plan_roll_steps(config: &Config, rolls: &[String], stable_ref: &str) -> Result<PromotePlan> {
    let repo = &config.repo_root;
    let stable = &config.stable_branch;
    let rolling = &config.rolling_branch;

    if rolls.is_empty() {
        bail!("no rolls named to promote");
    }

    let known = branches::list_rolls(config)?;
    let order = rolling_commit_order(config);

    let mut planned: Vec<(usize, PlannedStep)> = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();

    for name in rolls {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(info) = known.iter().find(|r| &r.branch == name) else {
            bail!(
                "no such roll: '{name}'. Run `rf list` to see the roll branches roll-flow knows about"
            );
        };
        let Some(graduation) = info.graduation_commit.clone() else {
            bail!(
                "'{name}' has not graduated, so there is nothing on '{rolling}' to promote. \
                 Run `rf graduate` from it first"
            );
        };
        if git::is_ancestor(repo, &graduation, stable_ref).unwrap_or(false) {
            skipped.push(SkippedRoll {
                roll: name.clone(),
                reason: format!("already contained in '{stable}'"),
            });
            continue;
        }

        planned.push((
            order.get(&graduation).copied().unwrap_or(usize::MAX),
            PlannedStep {
                roll: Some(name.clone()),
                source: graduation,
                subject: format!("Promote {name} to {stable}"),
                // Rolls riding along on an earlier graduation are attributed by
                // reachability (see `scan_promoted`), so the body only needs to
                // name this step's own roll.
                body: Some(format!("Rolls:\n  {name}\n")),
                included: vec![name.clone()],
            },
        ));
    }

    planned.sort_by_key(|(pos, _)| *pos);
    Ok(PromotePlan {
        steps: planned.into_iter().map(|(_, step)| step).collect(),
        skipped,
    })
}

/// Map each commit reachable from rolling to its distance from the tip, so
/// graduation commits can be ordered oldest-first. Commits missing from the map
/// sort last, which keeps an unexpectedly unreachable graduation from silently
/// jumping the queue.
fn rolling_commit_order(config: &Config) -> HashMap<String, usize> {
    let Some(rolling) = git::resolve_branch(&config.repo_root, &config.rolling_branch) else {
        return HashMap::new();
    };
    let Ok(out) = git::capture_git(&config.repo_root, &["rev-list", "--first-parent", &rolling])
    else {
        return HashMap::new();
    };
    // rev-list is newest-first, so reversing the index gives oldest-first order.
    let hashes: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = hashes.len();
    hashes
        .into_iter()
        .enumerate()
        .map(|(i, h)| (h.to_string(), total - i))
        .collect()
}

/// Run one planned step: stage the merge, gate the result, commit if it passes.
fn run_promote_step(
    config: &Config,
    stable_ref: &str,
    step: PlannedStep,
    dry_run: bool,
    force: &ForceOpts,
    tag: bool,
) -> Result<PromoteStep> {
    let repo = &config.repo_root;
    let stable = &config.stable_branch;

    // The version gate runs before the configured gates: it is nearly free, and
    // failing fast beats failing after a full build. The source is this step's
    // merge source, and the target is the *resolved* stable ref — which in
    // dry-run may be `origin/<stable>`, and which for a per-roll promotion has
    // already been advanced by the steps ahead of this one.
    let version = version_check(config, &step.source, stable_ref)?;
    let mut version_bypass = Vec::new();
    if !version.is_satisfied() {
        // `--dry-run` previews rather than enforces, exactly as it does for the
        // configured gates (which are printed, not executed). The rendered
        // status line still reports that the version would block a real run.
        if !force.enabled && !dry_run {
            return Err(version_gate_error(&version, &step.source, stable));
        }
        // Under --force the gate is recorded in the merge trailer exactly like a
        // bypassed shell gate, so the override leaves the same audit trail.
        if force.enabled {
            version_bypass.push(GateBypass {
                gate: format!("version bump check ({} vs {stable})", step.source),
                code: None,
            });
        }
    }

    // Gates run against the staged merge result, so `report` is produced inside
    // `merge_gated`. In dry-run nothing is staged and nothing is merged.
    let run_checks = || -> Result<(GateReport, HostReport)> {
        let report = run_gates(repo, &config.rolling_to_main_gates, dry_run, force)?;

        // Host gates block promotion when an active host fails (issue #106),
        // unless `--force`, in which case each failing host gate is recorded as
        // a bypass in the merge trailer alongside the route-gate bypasses.
        let host_report = run_host_gates(config, dry_run, force)?;
        if !force.enabled {
            let failed = host_report.failed_hosts();
            if !failed.is_empty() {
                bail!("host verification failed: {}", failed.join(", "));
            }
        }
        Ok((report, host_report))
    };

    if dry_run {
        let (report, host_report) = run_checks()?;
        let tag_outcome = match (tag && config.tag_on_promote, version.head) {
            // The version still has to move, so the eventual tag name is not
            // knowable here — claiming the current one would be wrong.
            (true, Some(_)) if !version.is_satisfied() => TagOutcome::WouldCreateAfterBump,
            (true, Some(v)) => TagOutcome::WouldCreate { tag: v.tag() },
            _ => TagOutcome::Skipped,
        };
        return Ok(PromoteStep {
            roll: step.roll,
            source: step.source,
            gate_notices: report.notices,
            host_results: host_report.results,
            host_notices: host_report.notices,
            version,
            tag: tag_outcome,
        });
    }

    // The force trailer depends on what the gates bypassed, which is only known
    // after they run — so the body is finalised inside the closure and the
    // commit message is assembled once the step returns.
    let (report, host_report) = merge_gated(
        repo,
        &step.source,
        stable_ref,
        &step.subject,
        step.body.as_deref(),
        run_checks,
    )?;

    let mut bypassed = version_bypass;
    bypassed.extend(report.bypassed);
    bypassed.extend(host_report.bypassed);
    if let Some(trailer) = force.trailer(&bypassed) {
        append_commit_trailer(repo, stable_ref, &trailer)?;
    }

    // Tag last of all, so it points at the commit the trailer amend produced
    // rather than the one it replaced.
    let tag_outcome = tag_release(config, &version, tag, &step.included)?;

    Ok(PromoteStep {
        roll: step.roll,
        source: step.source,
        gate_notices: report.notices,
        host_results: host_report.results,
        host_notices: host_report.notices,
        version,
        tag: tag_outcome,
    })
}

/// Append a `Forced-Bypass:` trailer to the tip of `branch_ref` after the fact.
///
/// Gates have to run before the commit exists (that is the whole point of
/// `merge_gated`), but which of them were bypassed is only known once they have
/// run — so the trailer is amended on rather than passed in. Amending the tip of
/// stable here is safe: it is the merge this step just created, moments ago, and
/// nothing else can have advanced it in between.
fn append_commit_trailer(repo: &Path, branch_ref: &str, trailer: &str) -> Result<()> {
    let original = git::current_branch(repo)?;
    git::run_git(repo, &["checkout", branch_ref]).with_context(|| {
        format!("failed to check out '{branch_ref}' to record the force trailer")
    })?;

    let existing = git::capture_git(repo, &["log", "-1", "--format=%B"])?;
    let message = format!("{}\n\n{trailer}\n", existing.trim_end());
    let amend = git::run_git(repo, &["commit", "--amend", "--no-edit", "-m", &message]);

    git::run_git(repo, &["checkout", &original]).with_context(|| {
        format!("recorded the force trailer, but checking out '{original}' again failed")
    })?;
    amend.with_context(|| format!("failed to record the force trailer on '{branch_ref}'"))?;
    Ok(())
}

/// Subject and body for a promotion merge. Exactly one graduated roll included
/// → subject names it; otherwise a generic subject with the rolls listed in the
/// body so promoted-state detection can attribute them.
fn promote_subject_and_body(config: &Config, included: &[String]) -> (String, Option<String>) {
    let subject = if included.len() == 1 {
        format!("Promote {} to {}", included[0], config.stable_branch)
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
        for roll in included {
            body.push_str(&format!("  {roll}\n"));
        }
        Some(body)
    };

    (subject, body)
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

// ── prune ───────────────────────────────────────────────────────────────────

/// What has to be true of a branch tip before it may be deleted — the question
/// "do these commits survive anywhere else?".
///
/// `rf prune` and `rf delete` ask it of the stable branch alone: they wind up
/// work that has landed, and landing is the whole point. `rf tidy` asks a
/// weaker question on purpose. It is reclaiming a machine's disk, not retiring
/// work, so a branch whose commits are still on `origin` is fair game — `git
/// fetch` brings it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Containment {
    /// Only the stable branch (local or `origin/<stable>`) counts.
    Stable,
    /// Stable, the rolling branch, or the branch's own `origin/<branch>` copy.
    Recoverable,
}

impl Containment {
    /// Whether this policy needs a pruning fetch even when no remote copy is
    /// being deleted.
    ///
    /// `Stable` does not: a stale `origin/<stable>` can only be *older* than
    /// the real one, which under-reports containment and errs toward keeping
    /// branches. `Recoverable` does, and load-bearingly so — it accepts
    /// `origin/<branch>` as proof the commits survive, and a cached ref for a
    /// branch already deleted upstream would certify exactly the branches whose
    /// only remaining copy is the local one.
    fn needs_fetch(self) -> bool {
        matches!(self, Containment::Recoverable)
    }
}

/// Which copies of a roll branch a deletion may touch, and how strict to be
/// about it.
pub(crate) struct PruneScope {
    pub local: bool,
    pub remote: bool,
    /// Delete even when the branch tip holds commits the policy below does not
    /// find anywhere else.
    pub force: bool,
    /// Refresh remote-tracking refs before planning.
    pub fetch: bool,
    /// What counts as "these commits survive elsewhere".
    pub containment: Containment,
}

impl PruneScope {
    /// The default `rf prune`: both copies, no force, refreshing first.
    pub fn both() -> Self {
        PruneScope {
            local: true,
            remote: true,
            force: false,
            fetch: true,
            containment: Containment::Stable,
        }
    }

    /// The default `rf tidy`: local copies only, judged against everywhere the
    /// commits could still be recovered from.
    pub fn tidy(force: bool) -> Self {
        PruneScope {
            local: true,
            remote: false,
            force,
            fetch: true,
            containment: Containment::Recoverable,
        }
    }
}

/// A promoted roll branch prune intends to delete, and which copies of it.
pub(crate) struct PruneCandidate {
    pub branch: String,
    pub number: u32,
    pub delete_local: bool,
    pub delete_remote: bool,
}

/// A branch copy prune declined to touch, with the reason to show the user.
/// Nothing is ever skipped silently.
pub(crate) struct PruneSkip {
    pub branch: String,
    pub reason: String,
}

/// What a prune would do, computed before anything is deleted so the CLI and the
/// TUI can both show it, confirm, then apply the very same plan.
pub(crate) struct PrunePlan {
    pub candidates: Vec<PruneCandidate>,
    pub skipped: Vec<PruneSkip>,
    /// Whether an `origin` remote exists at all — distinct from "no remote
    /// branches matched".
    pub has_remote: bool,
}

impl PrunePlan {
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// Branches whose `origin` copy this plan deletes.
    fn remote_targets(&self) -> Vec<String> {
        self.candidates
            .iter()
            .filter(|c| c.delete_remote)
            .map(|c| c.branch.clone())
            .collect()
    }
}

/// Per-branch outcome of applying a [`PrunePlan`]. A branch can partially
/// succeed (local gone, remote push failed), so both flags and the errors are
/// reported together rather than as either/or.
pub(crate) struct PruneResult {
    pub branch: String,
    pub local_deleted: bool,
    pub remote_deleted: bool,
    pub errors: Vec<String>,
}

/// Refs that count as "already in `branch`" for containment checks: the local
/// branch and `origin/<branch>`, whichever resolve.
///
/// Both are consulted because they routinely differ — a roll promoted upstream
/// is contained in `origin/main` while a stale local `main` still lacks it, and
/// checking only one side would skip branches that are genuinely safe to delete.
fn containment_refs(repo: &Path, branch: &str) -> Vec<String> {
    let mut refs = Vec::new();
    if git::ref_exists(repo, branch) {
        refs.push(branch.to_string());
    }
    let tracking = format!("origin/{branch}");
    if git::ref_exists(repo, &tracking) {
        refs.push(tracking);
    }
    refs
}

/// True if every commit reachable from `tip` is already in one of `refs`.
fn contained_in_any(repo: &Path, tip: &str, refs: &[String]) -> bool {
    refs.iter()
        .any(|reference| git::is_ancestor(repo, tip, reference).unwrap_or(false))
}

/// Branch name -> worktree path for every local branch checked out somewhere.
///
/// One `for-each-ref` for the whole repo rather than a call per branch. A
/// failure here is not fatal: without the map the guard simply does not fire
/// and git refuses the delete itself, with a worse message.
fn worktree_occupancy(repo: &Path) -> HashMap<String, String> {
    git::local_branch_details(repo)
        .map(|branches| {
            branches
                .into_iter()
                .filter(|b| b.is_checked_out())
                .map(|b| {
                    let path = b.worktree.trim().to_string();
                    (b.name, path)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Repo facts every deletion decision is made against, gathered once.
///
/// Building it performs the `git fetch --prune` when the scope touches the
/// remote, so a caller planning many branches pays for it a single time.
struct DeletionContext {
    has_remote: bool,
    stable_refs: Vec<String>,
    /// The rolling branch and `origin/<rolling>`, whichever resolve. Gathered
    /// only for [`Containment::Recoverable`], which is the one policy that
    /// counts them; empty otherwise rather than computed and ignored.
    rolling_refs: Vec<String>,
    current: String,
    /// Branches checked out in some worktree, mapped to that worktree's path.
    worktrees: HashMap<String, String>,
}

impl DeletionContext {
    fn build(config: &Config, scope: &PruneScope) -> Result<Self> {
        let repo = &config.repo_root;
        let has_remote = git::has_remote(repo, "origin");

        // Refresh first: `rf` is otherwise local-only, so `origin/*` refs can
        // claim branches that are already gone upstream — or, worse, name a tip
        // that is no longer the real one, which would judge containment against
        // stale history and delete commits the check never saw. That applies
        // whenever remote-tracking refs decide the outcome: because a remote
        // copy is being deleted, or because the policy reads `origin/<branch>`
        // to call a local copy recoverable.
        if scope.fetch && has_remote && (scope.remote || scope.containment.needs_fetch()) {
            git::fetch_prune(repo, "origin")
                .context("refreshing remote-tracking refs before deleting")?;
        }

        let rolling_refs = match scope.containment {
            Containment::Stable => Vec::new(),
            Containment::Recoverable => containment_refs(repo, &config.rolling_branch),
        };

        Ok(DeletionContext {
            has_remote,
            stable_refs: containment_refs(repo, &config.stable_branch),
            rolling_refs,
            current: git::current_branch(repo).unwrap_or_default(),
            worktrees: worktree_occupancy(repo),
        })
    }

    /// The worktree path a branch is checked out in, excluding the current
    /// branch — that one has its own, plainer refusal.
    fn other_worktree(&self, branch: &str) -> Option<&str> {
        if branch == self.current {
            return None;
        }
        self.worktrees.get(branch).map(String::as_str)
    }
}

/// True when every commit on the local `branch` already lives somewhere
/// `policy` accepts as survival.
fn local_survives(repo: &Path, branch: &str, ctx: &DeletionContext, policy: Containment) -> bool {
    if contained_in_any(repo, branch, &ctx.stable_refs) {
        return true;
    }
    if policy == Containment::Stable {
        return false;
    }
    if contained_in_any(repo, branch, &ctx.rolling_refs) {
        return true;
    }

    // The branch's own copy on origin. A local tip that is an ancestor of it
    // holds nothing that has not been pushed, so deleting the local copy loses
    // no commits — the branch is one `git fetch` away. This leg is why the
    // policy demands the pruning fetch above.
    let tracking = format!("origin/{branch}");
    ctx.has_remote
        && git::ref_exists(repo, &tracking)
        && git::is_ancestor(repo, branch, &tracking).unwrap_or(false)
}

/// Why a branch copy was declined. Kept as data rather than a formatted string
/// so [`decide_copies`] stays pure and its safety table is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The local copy is the checked-out branch.
    CheckedOut,
    /// The local copy is checked out in some *other* worktree. git refuses to
    /// delete it regardless, so it is reported rather than attempted.
    CheckedOutElsewhere,
    /// The local tip holds commits the containment policy found nowhere else.
    LocalUncontained,
    /// The `origin` tip holds commits stable lacks.
    RemoteUncontained,
}

/// The per-branch facts a deletion decision is made from, gathered by
/// [`plan_branch_deletion`] so [`decide_copies`] can stay pure.
#[derive(Debug, Clone, Copy, Default)]
struct CopyFacts {
    has_local: bool,
    has_remote_copy: bool,
    /// The local copy is the branch HEAD is on.
    is_current: bool,
    /// The local copy is checked out in a different worktree.
    in_worktree: bool,
    local_contained: bool,
    remote_contained: bool,
}

/// Which copies a deletion may touch, given facts already gathered from git.
///
/// The single implementation of the safety rules, shared by `rf prune`,
/// `rf delete` and `rf tidy`. Two of them matter most: a local copy that is
/// checked out anywhere is never deletable — those checks sit *before* the
/// force check, so `force` cannot reach past them — and a copy whose commits
/// the containment policy could not find elsewhere needs `force`.
///
/// Pure, so the whole table is testable without a repo. Note it takes the
/// *answers* about containment, not the policy: which refs were consulted is
/// [`local_survives`]'s business, and keeping that out of here is what lets one
/// table serve all three commands.
fn decide_copies(scope: &PruneScope, facts: CopyFacts) -> (bool, bool, Vec<SkipReason>) {
    let mut skips = Vec::new();
    let mut delete_local = false;
    let mut delete_remote = false;

    if scope.local && facts.has_local {
        if facts.is_current {
            skips.push(SkipReason::CheckedOut);
        } else if facts.in_worktree {
            skips.push(SkipReason::CheckedOutElsewhere);
        } else if scope.force || facts.local_contained {
            delete_local = true;
        } else {
            skips.push(SkipReason::LocalUncontained);
        }
    }

    if scope.remote && facts.has_remote_copy {
        if scope.force || facts.remote_contained {
            delete_remote = true;
        } else {
            skips.push(SkipReason::RemoteUncontained);
        }
    }

    (delete_local, delete_remote, skips)
}

/// Plan the deletion of one branch, appending a reason to `skipped` for every
/// copy declined. Shared by [`prune_plan`] (once per promoted roll) and
/// [`delete_branch_plan`] (one explicitly named branch), so the containment and
/// checked-out rules have exactly one implementation.
///
/// Returns `None` when no copy survives the checks — there is nothing to delete.
fn plan_branch_deletion(
    config: &Config,
    branch: &str,
    number: u32,
    scope: &PruneScope,
    ctx: &DeletionContext,
    skipped: &mut Vec<PruneSkip>,
) -> Option<PruneCandidate> {
    let repo = &config.repo_root;
    let remote_ref = format!("origin/{branch}");
    let has_local = git::ref_exists(repo, branch);
    let has_remote_copy = ctx.has_remote && git::ref_exists(repo, &remote_ref);
    let worktree = ctx.other_worktree(branch);

    // Containment is only asked about copies whose answer could change the
    // outcome: `is_ancestor` is a subprocess, and `force` or the checked-out
    // guards already decide those cases without it.
    let local_contained = has_local
        && scope.local
        && !scope.force
        && branch != ctx.current
        && worktree.is_none()
        && local_survives(repo, branch, ctx, scope.containment);
    let remote_contained = has_remote_copy
        && scope.remote
        && !scope.force
        && contained_in_any(repo, &remote_ref, &ctx.stable_refs);

    let (delete_local, delete_remote, skips) = decide_copies(
        scope,
        CopyFacts {
            has_local,
            has_remote_copy,
            is_current: branch == ctx.current,
            in_worktree: worktree.is_some(),
            local_contained,
            remote_contained,
        },
    );

    for reason in skips {
        skipped.push(PruneSkip {
            branch: branch.to_string(),
            reason: match reason {
                SkipReason::CheckedOut => {
                    "checked out — switch away to delete the local copy".to_string()
                }
                SkipReason::CheckedOutElsewhere => format!(
                    "checked out in worktree at {} — remove it to delete the branch",
                    worktree.unwrap_or("another worktree")
                ),
                // Worded from the policy that was actually applied, so the
                // reason names the refs that were consulted rather than a
                // fixed one the user would then go and check by hand.
                SkipReason::LocalUncontained => match scope.containment {
                    Containment::Stable => format!(
                        "local tip has commits not in '{}' — use --force to delete anyway",
                        config.stable_branch
                    ),
                    Containment::Recoverable => format!(
                        "local tip has commits not on origin, '{}' or '{}' — use --force to delete anyway",
                        config.rolling_branch, config.stable_branch
                    ),
                },
                SkipReason::RemoteUncontained => format!(
                    "origin copy has commits not in '{}' — use --force to delete anyway",
                    config.stable_branch
                ),
            },
        });
    }

    (delete_local || delete_remote).then(|| PruneCandidate {
        branch: branch.to_string(),
        number,
        delete_local,
        delete_remote,
    })
}

/// Decide what `rf prune` would delete, without deleting anything.
///
/// Promoted state alone is not treated as sufficient authority to delete: it is
/// inferred from commit *subjects* on stable, which says the roll was promoted
/// but not that this particular branch tip has nothing left on it (a roll can
/// take commits after its graduation merge). Every copy is additionally checked
/// for containment in stable, and anything that fails is skipped with a reason
/// unless `--force` is given.
pub(crate) fn prune_plan(config: &Config, scope: &PruneScope) -> Result<PrunePlan> {
    plan_rolls(config, scope, |state| {
        *state == branches::RollState::Promoted
    })
}

/// Decide what `rf tidy` would delete locally, without deleting anything.
///
/// The same plan as [`prune_plan`] behind the same safety table, differing in
/// the two dimensions tidy cares about: it looks at whichever roll `states` the
/// caller asked for rather than promoted ones only, and its `scope` carries
/// [`Containment::Recoverable`], so a branch counts as safe when its commits
/// are on rolling or still on `origin` — not only when they reached stable.
///
/// It takes a [`PruneScope`] like everything else here rather than a bespoke
/// one: there is deliberately a single deletion path, and tidy is a set of
/// arguments to it, not a second implementation.
pub(crate) fn tidy_plan(
    config: &Config,
    scope: &PruneScope,
    states: &[branches::RollState],
) -> Result<PrunePlan> {
    plan_rolls(config, scope, |state| states.contains(state))
}

/// The shared body of [`prune_plan`] and [`tidy_plan`]: walk every roll the
/// `accept` predicate keeps and plan each one's deletion against `scope`.
///
/// The [`DeletionContext`] is built once, so the pruning fetch and the worktree
/// scan are each paid for a single time no matter how many rolls match.
fn plan_rolls(
    config: &Config,
    scope: &PruneScope,
    accept: impl Fn(&branches::RollState) -> bool,
) -> Result<PrunePlan> {
    let ctx = DeletionContext::build(config, scope)?;

    let mut candidates = Vec::new();
    let mut skipped = Vec::new();

    for roll in branches::list_rolls(config)? {
        if !accept(&roll.state) {
            continue;
        }
        if let Some(candidate) =
            plan_branch_deletion(config, &roll.branch, roll.number, scope, &ctx, &mut skipped)
        {
            candidates.push(candidate);
        }
    }

    Ok(PrunePlan {
        candidates,
        skipped,
        has_remote: ctx.has_remote,
    })
}

/// Execute a [`PrunePlan`]. Never prompts and never decides — the caller has
/// already confirmed.
///
/// Remote deletions go out as one batched push (all-or-nothing for that batch);
/// local deletions run per branch so one failure does not abort the rest.
pub(crate) fn prune_apply(config: &Config, plan: &PrunePlan) -> Result<Vec<PruneResult>> {
    let repo = &config.repo_root;

    let remote_targets = plan.remote_targets();
    let remote_error = match git::delete_remote_branches(repo, "origin", &remote_targets) {
        Ok(()) => None,
        Err(err) => Some(err.to_string()),
    };

    let mut results = Vec::new();
    for candidate in &plan.candidates {
        let mut errors = Vec::new();
        let mut local_deleted = false;
        let mut remote_deleted = false;

        if candidate.delete_local {
            match git::delete_local_branch(repo, &candidate.branch) {
                Ok(()) => local_deleted = true,
                Err(err) => errors.push(err.to_string()),
            }
        }
        if candidate.delete_remote {
            match &remote_error {
                None => remote_deleted = true,
                Some(err) => errors.push(err.clone()),
            }
        }

        results.push(PruneResult {
            branch: candidate.branch.clone(),
            local_deleted,
            remote_deleted,
            errors,
        });
    }

    Ok(results)
}

/// Plan the deletion of a single named branch — what `rf delete` and the TUI's
/// `[d]elete` ask for.
///
/// Deliberately returns a [`PrunePlan`] so the caller applies it with
/// [`prune_apply`] and renders the outcome exactly as a prune does: one
/// execution path to the remote, one result vocabulary, no second way to delete
/// a branch.
///
/// Unlike [`prune_plan`] this does not filter on roll state — the user named
/// this branch, rather than the tool inferring it from commit subjects, so an
/// abandoned or never-promoted roll is a legitimate target. Every other rule is
/// identical: never the checked-out branch's local copy, and never a copy
/// holding commits stable lacks unless `scope.force`.
pub(crate) fn delete_branch_plan(
    config: &Config,
    branch: &str,
    scope: &PruneScope,
) -> Result<PrunePlan> {
    // The TUI can only reach roll rows, but this function is the safety
    // boundary and `rf delete` takes an arbitrary string.
    if branch == config.stable_branch || branch == config.rolling_branch {
        bail!("refusing to delete '{branch}' — it is a workflow branch, not a roll");
    }

    let ctx = DeletionContext::build(config, scope)?;
    let mut skipped = Vec::new();
    let number = branches::parse_roll_number(branch, &config.roll_prefix).unwrap_or(0);

    let candidates = match plan_branch_deletion(config, branch, number, scope, &ctx, &mut skipped) {
        Some(candidate) => vec![candidate],
        None => {
            // Nothing declined and nothing to delete means no copy resolved.
            // Report it rather than erroring: a TUI row can be stale, and a
            // fetch may have just removed the origin copy out from under it.
            if skipped.is_empty() {
                skipped.push(PruneSkip {
                    branch: branch.to_string(),
                    reason: "no matching branch in the requested scope".to_string(),
                });
            }
            Vec::new()
        }
    };

    Ok(PrunePlan {
        candidates,
        skipped,
        has_remote: ctx.has_remote,
    })
}

/// Commits each copy of `branch` holds that stable does not, as
/// `(local, origin)` — what deleting that copy would actually lose.
///
/// `None` means the copy does not exist or the count could not be taken; it is
/// deliberately distinct from `Some(0)` ("exists, loses nothing"), because the
/// caller uses this to decide whether to *warn*, and an unknown must not read
/// as safe. Advisory only: the authority to delete is re-derived inside
/// [`delete_branch_plan`] at apply time.
pub(crate) fn unmerged_commit_counts(config: &Config, branch: &str) -> (Option<u32>, Option<u32>) {
    let repo = &config.repo_root;
    let stable_refs = containment_refs(repo, &config.stable_branch);
    let remote_ref = format!("origin/{branch}");

    let count = |tip: &str| git::commits_not_in(repo, tip, &stable_refs).ok();

    let local = git::ref_exists(repo, branch)
        .then(|| count(branch))
        .flatten();
    let remote = git::ref_exists(repo, &remote_ref)
        .then(|| count(&remote_ref))
        .flatten();
    (local, remote)
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

#[cfg(test)]
mod tests {
    use super::{decide_copies, Containment, CopyFacts, PruneScope, SkipReason};

    /// A scope covering both copies, with `force` under test.
    fn both(force: bool) -> PruneScope {
        PruneScope {
            local: true,
            remote: true,
            force,
            fetch: false,
            containment: Containment::Stable,
        }
    }

    /// Both copies present, neither checked out, containment under test.
    fn facts(local_contained: bool, remote_contained: bool) -> CopyFacts {
        CopyFacts {
            has_local: true,
            has_remote_copy: true,
            local_contained,
            remote_contained,
            ..CopyFacts::default()
        }
    }

    #[test]
    fn deletes_both_copies_when_contained() {
        let (local, remote, skips) = decide_copies(&both(false), facts(true, true));
        assert!(local && remote);
        assert!(skips.is_empty(), "nothing to explain: {skips:?}");
    }

    #[test]
    fn uncontained_copies_need_force() {
        let (local, remote, skips) = decide_copies(&both(false), facts(false, false));
        assert!(!local && !remote, "neither copy is safe without force");
        assert_eq!(
            skips,
            vec![SkipReason::LocalUncontained, SkipReason::RemoteUncontained]
        );

        let (local, remote, skips) = decide_copies(&both(true), facts(false, false));
        assert!(local && remote, "force is the documented override");
        assert!(skips.is_empty());
    }

    #[test]
    fn force_never_deletes_the_checked_out_local_copy() {
        // The load-bearing rule: the checked-out guard sits ahead of the force
        // check, so `--force` cannot reach past it. Origin is still fair game.
        for force in [false, true] {
            let (local, remote, skips) = decide_copies(
                &both(force),
                CopyFacts {
                    is_current: true,
                    ..facts(true, true)
                },
            );
            assert!(!local, "checked-out local copy must survive force={force}");
            assert!(remote, "the origin copy is not checked out anywhere");
            assert_eq!(skips, vec![SkipReason::CheckedOut]);
        }
    }

    #[test]
    fn force_never_deletes_a_copy_checked_out_in_another_worktree() {
        // Same rule, second worktree: git refuses the delete either way, so the
        // guard exists to say *why* rather than to surface git's error. It also
        // sits ahead of the force check — `rf tidy --force` in a repo with a
        // worktree must not turn into a failed delete per branch.
        for force in [false, true] {
            let (local, remote, skips) = decide_copies(
                &both(force),
                CopyFacts {
                    in_worktree: true,
                    ..facts(true, true)
                },
            );
            assert!(
                !local,
                "worktree-held local copy must survive force={force}"
            );
            assert!(remote, "the origin copy is not checked out anywhere");
            assert_eq!(skips, vec![SkipReason::CheckedOutElsewhere]);
        }
    }

    #[test]
    fn the_current_branch_outranks_the_worktree_reason() {
        // The current branch is always in *some* worktree. Naming this repo's
        // own path back at the user reads as a puzzle; the plain refusal wins.
        let (local, _, skips) = decide_copies(
            &both(false),
            CopyFacts {
                is_current: true,
                in_worktree: true,
                ..facts(true, true)
            },
        );
        assert!(!local);
        assert_eq!(skips, vec![SkipReason::CheckedOut]);
    }

    #[test]
    fn a_copy_that_does_not_exist_is_neither_deleted_nor_skipped() {
        let (local, remote, skips) = decide_copies(
            &both(false),
            CopyFacts {
                has_local: false,
                ..facts(false, true)
            },
        );
        assert!(!local && remote);
        assert!(
            skips.is_empty(),
            "a missing copy is not a refusal to explain: {skips:?}"
        );
    }

    #[test]
    fn scope_flags_exclude_a_copy_entirely() {
        let local_only = PruneScope {
            remote: false,
            ..both(false)
        };
        // The origin copy is uncontained, but out of scope — so it is neither
        // deleted nor reported, rather than surfacing a confusing refusal.
        let (local, remote, skips) = decide_copies(&local_only, facts(true, false));
        assert!(local && !remote);
        assert!(skips.is_empty(), "{skips:?}");

        let remote_only = PruneScope {
            local: false,
            ..both(false)
        };
        let (local, remote, skips) = decide_copies(
            &remote_only,
            CopyFacts {
                is_current: true,
                ..facts(true, true)
            },
        );
        assert!(!local && remote);
        assert!(
            skips.is_empty(),
            "the checked-out local copy is out of scope, not refused: {skips:?}"
        );
    }

    #[test]
    fn tidy_scope_is_local_only_and_never_reaches_the_remote() {
        // `rf tidy` cleans a machine. Whatever else changes about it, it must
        // not acquire a path to the remote: that is prune's and delete's job,
        // under their own confirmations.
        for force in [false, true] {
            let scope = PruneScope::tidy(force);
            assert!(scope.local && !scope.remote);
            assert_eq!(scope.containment, Containment::Recoverable);
            assert!(scope.fetch, "the recoverable policy reads origin/<branch>");

            let (_, remote, skips) = decide_copies(&scope, facts(true, true));
            assert!(!remote, "tidy must never delete an origin copy");
            assert!(skips.is_empty(), "{skips:?}");
        }
    }

    #[test]
    fn only_the_recoverable_policy_forces_a_fetch() {
        // `Stable` consults `origin/<stable>`, but a stale copy of it is only
        // ever *older*, which under-reports containment and keeps branches.
        // `Recoverable` reads `origin/<branch>` as proof commits survive, so a
        // stale ref there would delete the last copy of them.
        assert!(!Containment::Stable.needs_fetch());
        assert!(Containment::Recoverable.needs_fetch());
    }
}
