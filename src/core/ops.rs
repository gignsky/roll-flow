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

use crate::core::version::{BumpLevel, Semver, VersionCheck, VersionStatus};
use crate::core::{branches, config::Config, git, version};

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
            let status = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(&config.repo_root)
                .status()
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

/// Outcome of promoting the rolling branch into stable.
pub(crate) struct PromoteOutcome {
    pub rolling: String,
    pub stable: String,
    pub dry_run: bool,
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

/// Promote the rolling branch into stable with a structured `--no-ff` merge.
pub(crate) fn promote(
    config: &Config,
    dry_run: bool,
    force: &ForceOpts,
    tag: bool,
) -> Result<PromoteOutcome> {
    let repo = &config.repo_root;
    let rolling = &config.rolling_branch;
    let stable = &config.stable_branch;

    let stable_ref = ensure_local_target(config, stable, dry_run)?;
    match classify_merge(repo, rolling, &stable_ref)? {
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

    // The version gate runs before the configured gates: it is nearly free, and
    // failing fast beats failing after a full build. Compared against the
    // *resolved* stable ref, which in dry-run may be `origin/<stable>`.
    let version = version_check(config, rolling, &stable_ref)?;
    let mut version_bypass = Vec::new();
    if !version.is_satisfied() {
        // `--dry-run` previews rather than enforces, exactly as it does for the
        // configured gates (which are printed, not executed). The rendered
        // status line still reports that the version would block a real run.
        if !force.enabled && !dry_run {
            return Err(version_gate_error(&version, rolling, stable));
        }
        // Under --force the gate is recorded in the merge trailer exactly like a
        // bypassed shell gate, so the override leaves the same audit trail.
        if force.enabled {
            version_bypass.push(GateBypass {
                gate: format!("version bump check ({rolling} vs {stable})"),
                code: None,
            });
        }
    }

    let report = run_gates(repo, &config.rolling_to_main_gates, dry_run, force)?;

    // Host gates block promotion when an active host fails (issue #106), unless
    // `--force`, in which case each failing host gate is recorded as a bypass in
    // the merge trailer alongside the route-gate bypasses.
    let host_report = run_host_gates(config, dry_run, force)?;
    if !force.enabled {
        let failed = host_report.failed_hosts();
        if !failed.is_empty() {
            bail!("host verification failed: {}", failed.join(", "));
        }
    }

    if dry_run {
        let tag_outcome = match (tag && config.tag_on_promote, version.head) {
            // The version still has to move, so the eventual tag name is not
            // knowable here — claiming the current one would be wrong.
            (true, Some(_)) if !version.is_satisfied() => TagOutcome::WouldCreateAfterBump,
            (true, Some(v)) => TagOutcome::WouldCreate { tag: v.tag() },
            _ => TagOutcome::Skipped,
        };
        return Ok(PromoteOutcome {
            rolling: rolling.clone(),
            stable: stable.clone(),
            dry_run: true,
            gate_notices: report.notices,
            host_results: host_report.results,
            host_notices: host_report.notices,
            version,
            tag: tag_outcome,
        });
    }

    let mut bypassed = version_bypass;
    bypassed.extend(report.bypassed);
    bypassed.extend(host_report.bypassed);

    // Captured before the merge: afterwards these rolls read as promoted, not
    // graduated, so the list would come back empty.
    let included = included_rolls(config)?;

    let (subject, body) = promote_subject_and_body(config, &included);
    let body = ForceOpts::append_trailer(body, force.trailer(&bypassed));
    run_merge(repo, rolling, stable, &subject, body.as_deref())?;

    // Tag after the merge lands, pointing at the merge commit now on stable.
    let tag_outcome = tag_release(config, &version, tag, &included)?;

    Ok(PromoteOutcome {
        rolling: rolling.clone(),
        stable: stable.clone(),
        dry_run: false,
        gate_notices: report.notices,
        host_results: host_report.results,
        host_notices: host_report.notices,
        version,
        tag: tag_outcome,
    })
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

/// Which copies of a promoted roll branch `rf prune` may delete, and how strict
/// to be about it.
pub(crate) struct PruneScope {
    pub local: bool,
    pub remote: bool,
    /// Delete even when the branch tip holds commits the stable branch lacks.
    pub force: bool,
    /// Refresh remote-tracking refs before planning. Only meaningful with
    /// `remote`.
    pub fetch: bool,
}

impl PruneScope {
    /// The default `rf prune`: both copies, no force, refreshing first.
    pub fn both() -> Self {
        PruneScope {
            local: true,
            remote: true,
            force: false,
            fetch: true,
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

/// Refs that count as "already in stable" for containment checks: the local
/// stable branch and `origin/<stable>`, whichever resolve.
///
/// Both are consulted because they routinely differ — a roll promoted upstream
/// is contained in `origin/main` while a stale local `main` still lacks it, and
/// checking only one side would skip branches that are genuinely safe to delete.
fn stable_containment_refs(config: &Config) -> Vec<String> {
    let repo = &config.repo_root;
    let mut refs = Vec::new();
    if git::ref_exists(repo, &config.stable_branch) {
        refs.push(config.stable_branch.clone());
    }
    let remote_stable = format!("origin/{}", config.stable_branch);
    if git::ref_exists(repo, &remote_stable) {
        refs.push(remote_stable);
    }
    refs
}

/// True if every commit reachable from `tip` is already in one of `stable_refs`.
fn contained_in_stable(repo: &Path, tip: &str, stable_refs: &[String]) -> bool {
    stable_refs
        .iter()
        .any(|stable| git::is_ancestor(repo, tip, stable).unwrap_or(false))
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
    let repo = &config.repo_root;
    let has_remote = git::has_remote(repo, "origin");

    // Refresh first: `rf` is otherwise local-only, so `origin/*` refs can claim
    // branches that are already gone upstream — or miss ones that are not.
    if scope.remote && has_remote && scope.fetch {
        git::fetch_prune(repo, "origin")
            .context("refreshing remote-tracking refs before pruning")?;
    }

    let stable_refs = stable_containment_refs(config);
    let current = git::current_branch(repo).unwrap_or_default();

    let mut candidates = Vec::new();
    let mut skipped = Vec::new();

    for roll in branches::list_rolls(config)? {
        if roll.state != branches::RollState::Promoted {
            continue;
        }

        let remote_ref = format!("origin/{}", roll.branch);
        let has_local_copy = git::ref_exists(repo, &roll.branch);
        let has_remote_copy = has_remote && git::ref_exists(repo, &remote_ref);

        let mut delete_local = false;
        let mut delete_remote = false;

        if scope.local && has_local_copy {
            if roll.branch == current {
                skipped.push(PruneSkip {
                    branch: roll.branch.clone(),
                    reason: "checked out — switch away to prune the local copy".to_string(),
                });
            } else if scope.force || contained_in_stable(repo, &roll.branch, &stable_refs) {
                delete_local = true;
            } else {
                skipped.push(PruneSkip {
                    branch: roll.branch.clone(),
                    reason: format!(
                        "local tip has commits not in '{}' — use --force to delete anyway",
                        config.stable_branch
                    ),
                });
            }
        }

        if scope.remote && has_remote_copy {
            if scope.force || contained_in_stable(repo, &remote_ref, &stable_refs) {
                delete_remote = true;
            } else {
                skipped.push(PruneSkip {
                    branch: roll.branch.clone(),
                    reason: format!(
                        "origin copy has commits not in '{}' — use --force to delete anyway",
                        config.stable_branch
                    ),
                });
            }
        }

        if delete_local || delete_remote {
            candidates.push(PruneCandidate {
                branch: roll.branch,
                number: roll.number,
                delete_local,
                delete_remote,
            });
        }
    }

    Ok(PrunePlan {
        candidates,
        skipped,
        has_remote,
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
