//! Detects this machine's most recent local NixOS / home-manager rebuild,
//! read entirely from commit messages the dotfiles repo's own rebuild
//! scripts already write (`scripts/nixos-rebuild.sh`, `scripts/home-switch.sh`).
//! `rf` never shells out to `nixos-rebuild`/`home-manager` or reads profile
//! symlinks here — this is pure git-log scanning, consistent with `rf`'s
//! "never runs nix" stance.
//!
//! Not to be confused with the "Verification" / `core/verification.rs` design
//! sketched in `docs/internals/algorithms.md` (Epic #6, never built — see
//! that doc). That design is about gating promotion on structured test
//! commits written by an `rf test-all` that does not exist. This module
//! answers a narrower, purely informational question — "what did *this*
//! machine last rebuild, and on which roll/branch" — and gates nothing.

use std::collections::HashSet;
use std::path::Path;

use crate::core::branches::RollInfo;
use crate::core::config::Config;
use crate::core::git;

/// Overrides host-identity auto-detection. Doubles as the seam integration
/// tests use to fake a host deterministically (the real binary otherwise
/// detects whatever machine is actually running it), and as a manual escape
/// hatch when auto-detection gets a real machine wrong.
pub const HOST_IDENTIFIER_ENV: &str = "RF_HOST_IDENTIFIER";

/// Number of gutter slots [`gutter_glyphs`] renders — nixos switch/test/boot,
/// home switch/test.
pub const GUTTER_WIDTH: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildMarker {
    /// The roll's branch name, or `config.rolling_branch`/`config.stable_branch`
    /// when the commit landed directly there rather than on any roll branch
    /// that still has a ref.
    pub branch: String,
    pub commit: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelStatus {
    pub switch: Option<RebuildMarker>,
    pub test: Option<RebuildMarker>,
    /// Always `None` for home-manager — there is no "boot" concept there.
    pub boot: Option<RebuildMarker>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildStatus {
    pub host_id: String,
    pub nixos: ChannelStatus,
    pub home: ChannelStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebuildMode {
    Switch,
    Boot,
    Test,
}

// ── Host identity ────────────────────────────────────────────────────────────

/// This machine's identifier, in the same form dotfiles' own
/// `scripts/host-detection-lib.sh` writes into its rebuild commits:
/// `<flake-target>@<physical-host>` on WSL, else the plain flake target
/// (hostname, mapped `nixos` → `wsl` under WSL).
pub fn current_host_identifier(repo_root: &Path) -> Option<String> {
    if let Ok(over) = std::env::var(HOST_IDENTIFIER_ENV) {
        let over = over.trim();
        if !over.is_empty() {
            return Some(over.to_string());
        }
    }
    let hostname = read_hostname()?;
    let wsl = is_wsl_environment();
    let flake_target = if wsl && hostname == "nixos" {
        "wsl".to_string()
    } else {
        hostname.clone()
    };
    if wsl {
        Some(format!(
            "{flake_target}@{}",
            physical_host(repo_root, &hostname)
        ))
    } else {
        Some(flake_target)
    }
}

fn read_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_wsl_environment() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() {
        return true;
    }
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        if version.to_ascii_lowercase().contains("microsoft") {
            return true;
        }
    }
    which_cmd_exe().is_some()
}

fn which_cmd_exe() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("cmd.exe"))
        .find(|p| p.is_file())
}

/// `cmd.exe /c hostname`, else `<repo_root>/.physical-host`, else the plain
/// `hostname` — the same fallback chain `detect_physical_host` in
/// `host-detection-lib.sh` uses.
fn physical_host(repo_root: &Path, hostname: &str) -> String {
    if let Some(cmd_exe) = which_cmd_exe() {
        if let Ok(out) = std::process::Command::new(cmd_exe)
            .args(["/c", "hostname"])
            .output()
        {
            if out.status.success() {
                let name = String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .to_ascii_lowercase();
                if !name.is_empty() {
                    return name;
                }
            }
        }
    }
    if let Ok(contents) = std::fs::read_to_string(repo_root.join(".physical-host")) {
        let name = contents.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    hostname.to_string()
}

// ── Commit-message parsing ───────────────────────────────────────────────────

/// `"<host_id>: generation <N> (<mode>)"` or `"<host_id>: unknown generation (<mode>)"`.
fn parse_nixos_marker(subject: &str, host_id: &str) -> Option<RebuildMode> {
    let rest = subject.strip_prefix(host_id)?.strip_prefix(": ")?;
    let paren = if let Some(after) = rest.strip_prefix("unknown generation ") {
        after
    } else {
        let after = rest.strip_prefix("generation ")?;
        let digits_end = after.find(|c: char| !c.is_ascii_digit())?;
        if digits_end == 0 {
            return None;
        }
        after[digits_end..].trim_start()
    };
    let mode = paren.strip_prefix('(')?.strip_suffix(')')?;
    match mode {
        "switch" => Some(RebuildMode::Switch),
        "boot" => Some(RebuildMode::Boot),
        "test" => Some(RebuildMode::Test),
        _ => None,
    }
}

/// `"<username>@<host_id>: test build (no generation created)"` (test build)
/// or `"<username>@<host_id>: <home-manager generations line>"` (switch),
/// the latter recognized by containing `" id "` and `"->"` — a precision
/// guard against an unrelated commit that happens to share the prefix.
fn parse_home_marker(subject: &str, username: &str, host_id: &str) -> Option<RebuildMode> {
    if username.is_empty() {
        return None;
    }
    let prefix = format!("{username}@{host_id}: ");
    let rest = subject.strip_prefix(&prefix)?;
    if rest == "test build (no generation created)" {
        return Some(RebuildMode::Test);
    }
    if rest.contains(" id ") && rest.contains("->") {
        return Some(RebuildMode::Switch);
    }
    None
}

// ── Discovery ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Discovered {
    nixos_switch: Option<String>,
    nixos_test: Option<String>,
    nixos_boot: Option<String>,
    home_switch: Option<String>,
    home_test: Option<String>,
}

impl Discovered {
    fn is_empty(&self) -> bool {
        self.nixos_switch.is_none()
            && self.nixos_test.is_none()
            && self.nixos_boot.is_none()
            && self.home_switch.is_none()
            && self.home_test.is_none()
    }

    fn complete(&self, home_enabled: bool) -> bool {
        self.nixos_switch.is_some()
            && self.nixos_test.is_some()
            && self.nixos_boot.is_some()
            && (!home_enabled || (self.home_switch.is_some() && self.home_test.is_some()))
    }
}

/// Scan the *whole* repo (`git log --all`, already newest-first) once for the
/// most recent commit matching each of the five marker shapes. Kept separate
/// from attribution below: a commit whose owning roll has since been pruned
/// belongs to no ownership set there, so folding discovery into that walk
/// would silently miss the very commit this feature is about.
fn discover(repo: &Path, host_id: &str, username: &str) -> Discovered {
    let mut found = Discovered::default();
    let home_enabled = !username.is_empty();
    let Ok(out) = git::capture_git(repo, &["log", "--all", "--format=%H%x09%s"]) else {
        return found;
    };
    for line in out.lines() {
        let Some((hash, subject)) = line.split_once('\t') else {
            continue;
        };
        if let Some(mode) = parse_nixos_marker(subject, host_id) {
            match mode {
                RebuildMode::Switch if found.nixos_switch.is_none() => {
                    found.nixos_switch = Some(hash.to_string())
                }
                RebuildMode::Boot if found.nixos_boot.is_none() => {
                    found.nixos_boot = Some(hash.to_string())
                }
                RebuildMode::Test if found.nixos_test.is_none() => {
                    found.nixos_test = Some(hash.to_string())
                }
                _ => {}
            }
        } else if home_enabled {
            if let Some(mode) = parse_home_marker(subject, username, host_id) {
                match mode {
                    RebuildMode::Switch if found.home_switch.is_none() => {
                        found.home_switch = Some(hash.to_string())
                    }
                    RebuildMode::Test if found.home_test.is_none() => {
                        found.home_test = Some(hash.to_string())
                    }
                    _ => {}
                }
            }
        }
        if found.complete(home_enabled) {
            break;
        }
    }
    found
}

// ── Attribution ───────────────────────────────────────────────────────────────

/// `(branch name, its first-parent commit set)`.
type Bucket = (String, HashSet<String>);

/// Whichever of stable, rolling, or a roll owns `hash`, checked in that
/// order — **stable first, then rolling, then rolls** — against each ref's
/// plain [`git::first_parent_ancestors`] set (no exclusion needed: this
/// codebase's merges are always `--no-ff`, so content merged in from
/// elsewhere — a roll's own work via its graduation, a dependency via
/// `[i]`/`rf integrate` — sits on the *second*-parent side and never appears
/// on another ref's first-parent chain, regardless of graduation/promotion
/// order or how long ago it happened).
///
/// Checking stable and rolling *before* any roll is what keeps this sound: a
/// commit made directly on stable long before any of today's rolls existed
/// is still on *every* roll's first-parent chain too (every roll forks from
/// stable — ancient shared history, not anything the roll contributed), and
/// would otherwise misreport as belonging to whichever roll happens to be
/// checked first. Checking stable/rolling first catches it there instead;
/// a roll's own commits are never found on stable's or rolling's
/// first-parent chain (the same `--no-ff` guarantee above), so this reordering
/// never costs a roll its own, genuinely-owned commit.
///
/// `None` when none of the three contain it — see [`detect`]'s separate
/// fallback for the common remaining case that still is (a pruned roll's own
/// commit, reachable from stable/rolling only through a merge's second
/// parent); what's left unattributed after that is narrower still: a commit
/// still only on an unlanded hotfix branch (not an attribution candidate
/// today).
fn attribute_hash(
    hash: &str,
    stable: &Bucket,
    rolling: &Bucket,
    roll_buckets: &[(u32, Bucket)],
) -> Option<RebuildMarker> {
    if stable.1.contains(hash) {
        return Some(RebuildMarker {
            branch: stable.0.clone(),
            commit: hash.to_string(),
        });
    }
    if rolling.1.contains(hash) {
        return Some(RebuildMarker {
            branch: rolling.0.clone(),
            commit: hash.to_string(),
        });
    }
    for (_, (branch, set)) in roll_buckets {
        if set.contains(hash) {
            return Some(RebuildMarker {
                branch: branch.clone(),
                commit: hash.to_string(),
            });
        }
    }
    None
}

/// Last-resort fallback for a commit [`attribute_hash`] finds in none of its
/// buckets: a commit introduced to rolling/stable only through a merge's
/// *second* parent — overwhelmingly, a roll's own rebuild commit, once that
/// roll has graduated (or promoted) and its branch has since been pruned, so
/// there is no roll bucket left to claim it either — never appears on
/// either tier's first-parent-only chain, by construction of
/// `--first-parent`. Rather than let the marker vanish, a plain ancestor
/// check here names the tier it genuinely did reach. This cannot reintroduce
/// the false-positive [`attribute_hash`] avoids: it only ever resolves to
/// `rolling`/`stable`, never to a specific roll, and only runs once every
/// still-existing roll's own bucket has already failed to claim the commit.
///
/// Checked **stable before rolling**, same as [`attribute_hash`]: once a
/// commit has been promoted all the way, it is a plain ancestor of both, and
/// the most settled, final description — stable — is the more useful answer
/// than where it originated.
fn attribute_fallback(
    repo: &Path,
    hash: &str,
    stable_ref: Option<&str>,
    rolling_ref: Option<&str>,
    stable_branch: &str,
    rolling_branch: &str,
) -> Option<RebuildMarker> {
    if let Some(s) = stable_ref {
        if git::is_ancestor(repo, hash, s).unwrap_or(false) {
            return Some(RebuildMarker {
                branch: stable_branch.to_string(),
                commit: hash.to_string(),
            });
        }
    }
    if let Some(r) = rolling_ref {
        if git::is_ancestor(repo, hash, r).unwrap_or(false) {
            return Some(RebuildMarker {
                branch: rolling_branch.to_string(),
                commit: hash.to_string(),
            });
        }
    }
    None
}

fn supersede(repo: &Path, candidate: &mut Option<RebuildMarker>, switch: &Option<RebuildMarker>) {
    if let (Some(c), Some(s)) = (candidate.as_ref(), switch.as_ref()) {
        if git::is_ancestor(repo, &c.commit, &s.commit).unwrap_or(false) {
            *candidate = None;
        }
    }
}

/// Detect this machine's rebuild status, given its already-resolved
/// `host_id` (see [`current_host_identifier`] — kept separate so this, the
/// actual logic, is testable without touching the real machine or its
/// environment).
///
/// `None` when `host_id`'s flake-target prefix (before any `@physical-host`
/// suffix) is not known to `config.hosts`/`config.host_active` at all — not
/// `active_hosts()`, deliberately: a host mid-reinstall and marked inactive
/// should still show what it last rebuilt. A repo that does not model hosts
/// (`hosts = []`, `host_active = {}`, as roll-flow's own `.roll-flow.toml`
/// does) always yields `None` here, so this feature is invisible there.
pub fn detect(
    config: &Config,
    repo: &Path,
    rolls: &[RollInfo],
    host_id: &str,
) -> Option<RebuildStatus> {
    let flake_target = host_id.split('@').next().unwrap_or(host_id);
    let known = config.hosts.iter().any(|h| h == flake_target)
        || config.host_active.contains_key(flake_target);
    if !known {
        return None;
    }

    let found = discover(repo, host_id, &config.username);
    if found.is_empty() {
        return Some(RebuildStatus {
            host_id: host_id.to_string(),
            nixos: ChannelStatus::default(),
            home: ChannelStatus::default(),
        });
    }

    let rolling_ref = git::resolve_branch(repo, &config.rolling_branch);
    let stable_ref = git::resolve_branch(repo, &config.stable_branch);

    let mut roll_buckets: Vec<(u32, Bucket)> = rolls
        .iter()
        .filter_map(|r| {
            let roll_ref = git::resolve_branch(repo, &r.branch)?;
            let set = git::first_parent_ancestors(repo, &roll_ref);
            Some((r.number, (r.branch.clone(), set)))
        })
        .collect();
    roll_buckets.sort_by_key(|(number, _)| *number);

    let rolling_bucket: Bucket = (
        config.rolling_branch.clone(),
        rolling_ref
            .as_deref()
            .map(|r| git::first_parent_ancestors(repo, r))
            .unwrap_or_default(),
    );
    let stable_bucket: Bucket = (
        config.stable_branch.clone(),
        stable_ref
            .as_deref()
            .map(|s| git::first_parent_ancestors(repo, s))
            .unwrap_or_default(),
    );

    let attribute = |hash: &Option<String>| -> Option<RebuildMarker> {
        let hash = hash.as_ref()?;
        attribute_hash(hash, &stable_bucket, &rolling_bucket, &roll_buckets).or_else(|| {
            attribute_fallback(
                repo,
                hash,
                stable_ref.as_deref(),
                rolling_ref.as_deref(),
                &config.stable_branch,
                &config.rolling_branch,
            )
        })
    };

    let mut nixos = ChannelStatus {
        switch: attribute(&found.nixos_switch),
        test: attribute(&found.nixos_test),
        boot: attribute(&found.nixos_boot),
    };
    let mut home = ChannelStatus {
        switch: attribute(&found.home_switch),
        test: attribute(&found.home_test),
        boot: None,
    };

    // Hide a test/boot marker once a newer switch has already caught up to or
    // passed it — it no longer tells the user anything the switch doesn't.
    // `test`/`boot` are never compared against each other: they are parallel
    // states (ran-but-not-persisted vs staged-for-next-boot), not competing.
    supersede(repo, &mut nixos.test, &nixos.switch);
    supersede(repo, &mut nixos.boot, &nixos.switch);
    supersede(repo, &mut home.test, &home.switch);

    Some(RebuildStatus {
        host_id: host_id.to_string(),
        nixos,
        home,
    })
}

// ── Display ───────────────────────────────────────────────────────────────────

/// One-line plain-text summary, e.g.
/// `"last rebuild (merlin): nixos switch→rolling · home switch→rolling"`.
/// `None` when there is nothing to show at all (a recognized host that has
/// never rebuilt here, or every marker got superseded).
pub fn format_summary_line(status: &RebuildStatus) -> Option<String> {
    let mut parts = Vec::new();
    push_channel_parts(&mut parts, "nixos", &status.nixos, true);
    push_channel_parts(&mut parts, "home", &status.home, false);
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "last rebuild ({}): {}",
        status.host_id,
        parts.join(" · ")
    ))
}

fn push_channel_parts(
    parts: &mut Vec<String>,
    label: &str,
    channel: &ChannelStatus,
    has_boot: bool,
) {
    if let Some(m) = &channel.switch {
        parts.push(format!("{label} switch→{}", m.branch));
    }
    if let Some(m) = &channel.test {
        parts.push(format!("{label} test→{}", m.branch));
    }
    if has_boot {
        if let Some(m) = &channel.boot {
            parts.push(format!("{label} boot→{}", m.branch));
        }
    }
}

/// Fixed [`GUTTER_WIDTH`]-char slot string for `branch`'s row: `N`/`n`/`b` for
/// nixos switch/test/boot, `H`/`h` for home switch/test, space when that slot
/// doesn't point at this branch. Shared by the TUI and both plain-table
/// renderers so the glyph scheme is defined in exactly one place.
pub fn gutter_glyphs(status: &RebuildStatus, branch: &str) -> String {
    let slot = |marker: &Option<RebuildMarker>, glyph: char| -> char {
        if marker.as_ref().is_some_and(|m| m.branch == branch) {
            glyph
        } else {
            ' '
        }
    };
    [
        slot(&status.nixos.switch, 'N'),
        slot(&status.nixos.test, 'n'),
        slot(&status.nixos.boot, 'b'),
        slot(&status.home.switch, 'H'),
        slot(&status.home.test, 'h'),
    ]
    .iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nixos_generation_shapes() {
        assert_eq!(
            parse_nixos_marker("merlin: generation 298 (switch)", "merlin"),
            Some(RebuildMode::Switch)
        );
        assert_eq!(
            parse_nixos_marker("wsl@ganoslal: generation 217 (boot)", "wsl@ganoslal"),
            Some(RebuildMode::Boot)
        );
        assert_eq!(
            parse_nixos_marker("spacedock: generation 6 (test)", "spacedock"),
            Some(RebuildMode::Test)
        );
        assert_eq!(
            parse_nixos_marker("merlin: unknown generation (switch)", "merlin"),
            Some(RebuildMode::Switch)
        );
    }

    #[test]
    fn rejects_nixos_subjects_for_a_different_host_or_shape() {
        assert_eq!(
            parse_nixos_marker("merlin: generation 298 (switch)", "wsl"),
            None
        );
        // Pre-subcommand-suffix legacy shape from real history — no literal
        // "generation" word at all.
        assert_eq!(
            parse_nixos_marker("merlin: 123 current  2024-01-01", "merlin"),
            None
        );
        assert_eq!(
            parse_nixos_marker("chore: unrelated commit", "merlin"),
            None
        );
    }

    #[test]
    fn parses_home_manager_shapes() {
        assert_eq!(
            parse_home_marker(
                "gig@merlin: 2026-10-13 03:52 : id 237 -> /nix/store/x-home-manager-generation (current)",
                "gig",
                "merlin",
            ),
            Some(RebuildMode::Switch)
        );
        assert_eq!(
            parse_home_marker(
                "gig@wsl@ganoslal: test build (no generation created)",
                "gig",
                "wsl@ganoslal",
            ),
            Some(RebuildMode::Test)
        );
    }

    #[test]
    fn home_parsing_is_skipped_with_no_username_and_rejects_lookalikes() {
        assert_eq!(
            parse_home_marker("gig@merlin: id 1 -> x", "", "merlin"),
            None
        );
        // Starts with the right prefix but isn't either real shape.
        assert_eq!(
            parse_home_marker("gig@merlin: unrelated note", "gig", "merlin"),
            None
        );
    }

    fn marker(branch: &str) -> Option<RebuildMarker> {
        Some(RebuildMarker {
            branch: branch.to_string(),
            commit: "deadbeef".to_string(),
        })
    }

    #[test]
    fn gutter_glyphs_marks_only_the_matching_slots() {
        let status = RebuildStatus {
            host_id: "merlin".to_string(),
            nixos: ChannelStatus {
                switch: marker("rolling"),
                test: marker("roll/41-x"),
                boot: None,
            },
            home: ChannelStatus {
                switch: marker("rolling"),
                test: None,
                boot: None,
            },
        };
        assert_eq!(gutter_glyphs(&status, "rolling"), "N  H ");
        assert_eq!(gutter_glyphs(&status, "roll/41-x"), " n   ");
        assert_eq!(gutter_glyphs(&status, "main"), "     ");
        assert_eq!(gutter_glyphs(&status, "rolling").len(), GUTTER_WIDTH);
    }

    #[test]
    fn summary_line_omits_absent_channels_and_slots() {
        let status = RebuildStatus {
            host_id: "merlin".to_string(),
            nixos: ChannelStatus {
                switch: marker("rolling"),
                test: marker("roll/41-x"),
                boot: None,
            },
            home: ChannelStatus::default(),
        };
        assert_eq!(
            format_summary_line(&status),
            Some("last rebuild (merlin): nixos switch→rolling · nixos test→roll/41-x".to_string())
        );
        let empty = RebuildStatus {
            host_id: "merlin".to_string(),
            nixos: ChannelStatus::default(),
            home: ChannelStatus::default(),
        };
        assert_eq!(format_summary_line(&empty), None);
    }
}
