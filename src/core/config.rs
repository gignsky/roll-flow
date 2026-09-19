use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RfError;

const CONFIG_NAME: &str = ".roll-flow.toml";

/// The config schema this rf writes and expects. Bumped only when a key's
/// meaning changes in a way an older file cannot express; adding keys with
/// defaults does not count. A file carrying a different number still loads —
/// it is warned about, never refused, because the same file is read by
/// whichever rf happens to be on `PATH` on each machine.
pub const CONFIG_VERSION: u32 = 1;

/// How rf relates to the repo's workflow.
///
/// - [`Mode::Manage`] (default): rf drives the workflow — it creates roll
///   branches, performs graduation/promotion merges, and owns branch state.
/// - [`Mode::Assist`]: the human drives the workflow by hand; rf reports and
///   derives state without taking the wheel.
///
/// The mode is persisted in `.roll-flow.toml` (as a lowercase string) and is
/// currently informational: it is round-tripped and exposed via
/// [`Config::is_assist`] so later work can gate behavior on it (see the note at
/// that method) without a config migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Manage,
    Assist,
}

impl Mode {
    /// Parse a user-supplied `--mode` value, with a clear error on anything else.
    pub fn parse(s: &str) -> Result<Self, RfError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "manage" => Ok(Mode::Manage),
            "assist" => Ok(Mode::Assist),
            other => Err(RfError::Config(format!(
                "invalid --mode '{other}' (expected 'manage' or 'assist')"
            ))),
        }
    }
}

/// What the TUI's `[p]` key does to the checked-out branch.
///
/// Configurable because the right answer depends on the repo, not on rf: a roll
/// branch that silently grows a merge commit from a surprise `git pull` is
/// exactly the history that graduation detection has to reason about later, so
/// the default refuses rather than merging. `Merge` and `Rebase` are there for
/// repos that prefer git's own defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PullMode {
    /// `git pull --ff-only` — fast-forward or fail, never a merge commit.
    #[default]
    FfOnly,
    /// `git pull` — git's default, merging on divergence.
    Merge,
    /// `git pull --rebase` — replay local commits on the upstream.
    Rebase,
}

impl PullMode {
    /// Parse a user-supplied value, with a clear error on anything else.
    /// Accepts `ff-only` and `ff_only` since both spellings read naturally in
    /// TOML.
    ///
    /// Serde handles the config file itself; this exists for the `--pull-mode`
    /// style override a later CLI flag will want, and is exercised by tests so it
    /// cannot rot.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Result<Self, RfError> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "ff-only" => Ok(PullMode::FfOnly),
            "merge" => Ok(PullMode::Merge),
            "rebase" => Ok(PullMode::Rebase),
            other => Err(RfError::Config(format!(
                "invalid pull_mode '{other}' (expected 'ff-only', 'merge' or 'rebase')"
            ))),
        }
    }

    /// The flag this mode adds to `git pull`, if any.
    pub fn flag(&self) -> Option<&'static str> {
        match self {
            PullMode::FfOnly => Some("--ff-only"),
            PullMode::Merge => None,
            PullMode::Rebase => Some("--rebase"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    /// Ignored on load — always re-read from git — and kept only so a rendered
    /// file says where it was written. Optional for that reason.
    #[serde(default)]
    pub repo_root: PathBuf,
    pub rolling_branch: String,
    pub stable_branch: String,
    pub roll_prefix: String,

    /// Require a version bump in `Cargo.toml` before `rf verify` / `rf promote`
    /// will pass, mirroring `.github/workflows/version-bump-check.yml`. Repos
    /// with no `Cargo.toml` skip the check regardless of this setting.
    #[serde(default = "default_true")]
    pub version_gate: bool,
    /// Create an annotated `vX.Y.Z` tag on the promotion merge commit, mirroring
    /// `.github/workflows/tag-on-main.yml`.
    #[serde(default = "default_true")]
    pub tag_on_promote: bool,
    /// After creating a release tag, offer to push it to `origin`. The push is
    /// always confirmed interactively (or with `--yes`); this only controls
    /// whether the offer is made at all.
    #[serde(default = "default_true")]
    pub push_tag: bool,
    /// Workflow ownership mode. Defaults to [`Mode::Manage`] for configs that
    /// predate this field (via `#[serde(default)]`).
    #[serde(default)]
    pub mode: Mode,
    /// Informational today; see `docs/config.md`. Optional so a machine-wide
    /// config can supply it and a repo file can leave it out.
    #[serde(default)]
    pub username: String,
    /// Order for `host_active`; may be empty, in which case the table's keys
    /// are used. Optional for the same reason as `username`.
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub host_active: BTreeMap<String, bool>,
    #[serde(default)]
    pub roll_to_rolling_gates: Vec<String>,
    #[serde(default)]
    pub rolling_to_main_gates: Vec<String>,
    /// Per-host verification gate templates (issue #106). Each entry is a shell
    /// command in which the token `{host}` is substituted with each active host
    /// name before running. Empty by default, so configs that predate this field
    /// (and repos that want no host gating) are a clean no-op.
    #[serde(default)]
    pub host_gates: Vec<String>,
    /// Branches `rf clean` must never delete, on top of the stable and rolling
    /// branches it already protects. Empty by default, so configs that predate
    /// this field are a clean no-op.
    ///
    /// The hardcoded fallback (`main`/`master`/`develop` plus each remote's
    /// HEAD) only applies to repos with no config at all; a repo that has one
    /// names its own protected branches here.
    #[serde(default)]
    pub clean_protect: Vec<String>,
    /// What the TUI's `[p]` key runs on the checked-out branch. Defaults to
    /// [`PullMode::FfOnly`] for configs that predate this field.
    #[serde(default)]
    pub pull_mode: PullMode,
    /// The command the TUI's `gg` binding launches. Overridable so a wrapper
    /// script, a flake app, or an absolute path can stand in for a bare
    /// `lazygit` on `PATH`.
    #[serde(default = "default_lazygit")]
    pub lazygit_command: String,
}

impl Config {
    /// Every top-level key the struct knows. The single list the unknown-key
    /// warning and the config-docs test both read, so a field added to the
    /// struct without being added here is caught by `known_keys_match_struct`.
    pub const KEYS: &'static [&'static str] = &[
        "config_version",
        "repo_root",
        "rolling_branch",
        "stable_branch",
        "roll_prefix",
        "version_gate",
        "tag_on_promote",
        "push_tag",
        "mode",
        "username",
        "hosts",
        "host_active",
        "roll_to_rolling_gates",
        "rolling_to_main_gates",
        "host_gates",
        "clean_protect",
        "pull_mode",
        "lazygit_command",
    ];

    /// Hosts that are currently active (inactive ones are offline/rebuilding).
    ///
    /// `host_active` is the source of truth. `hosts` only fixes the order (and
    /// may list a host the map does not mention, which counts as active); when
    /// it is empty the map's keys are used in their own order. The empty-`hosts`
    /// case is what a repo whose `vars/hosts.nix` is a bare `{ host = bool; }`
    /// attrset produces, and treating it as "no hosts" silently switched every
    /// host gate off for exactly the repo this tool was written for.
    pub fn active_hosts(&self) -> Vec<String> {
        let ordered: Vec<&String> = if self.hosts.is_empty() {
            self.host_active.keys().collect()
        } else {
            self.hosts.iter().collect()
        };
        ordered
            .into_iter()
            .filter(|h| self.host_active.get(h.as_str()).copied().unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Load the effective config for the repo at `.`: the machine-wide file at
    /// [`Self::global_config_path`], if any, with `<repo>/.roll-flow.toml` laid
    /// over it key by key.
    ///
    /// The repo file is required — it is what marks a repo as roll-flow's —
    /// but it may be as small as the three branch keys, with everything else
    /// coming from the global file. That is the file a Home Manager module can
    /// write: a git checkout is nowhere a Nix module can put a file, but
    /// `~/.config/roll-flow/config.toml` is.
    ///
    /// Loud but forgiving: anything a file gets wrong that can be worked
    /// around is reported on stderr and worked around, so a typo is visible on
    /// every run without an older rf refusing a file a newer one wrote. Only a
    /// file that cannot be parsed at all, or a merged result missing a key with
    /// no default, is an error — and that error says how to regenerate it.
    pub fn load() -> Result<Self, RfError> {
        let repo_root = crate::core::git::repo_root(Path::new("."))?;
        let repo_path = Self::config_path(&repo_root);
        if !repo_path.exists() {
            return Err(RfError::Config(format!(
                "no roll-flow config found at {}; run `rf init` first",
                repo_path.display()
            )));
        }
        let repo_text = std::fs::read_to_string(&repo_path)?;

        let global = Self::global_config_path().filter(|p| p.exists());
        let global_text = match &global {
            Some(path) => Some(std::fs::read_to_string(path)?),
            None => None,
        };

        let mut layers: Vec<(String, &str)> = Vec::new();
        if let (Some(path), Some(text)) = (&global, &global_text) {
            layers.push((path.display().to_string(), text.as_str()));
        }
        layers.push((repo_path.display().to_string(), repo_text.as_str()));
        let layers: Vec<(&str, &str)> = layers.iter().map(|(l, t)| (l.as_str(), *t)).collect();

        let (config, warnings) = Self::from_layers(&layers, &repo_root)?;
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
        Ok(config)
    }

    /// The machine-wide defaults file: `$XDG_CONFIG_HOME/roll-flow/config.toml`,
    /// or `~/.config/roll-flow/config.toml`. `None` when neither variable is
    /// usable, which is not an error — it just means there is no global layer.
    pub fn global_config_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("roll-flow").join("config.toml"))
    }

    /// Parse one config file's text on its own — the repo file with no global
    /// layer. The simple entry point the unit tests use; production goes
    /// through [`Self::load`] and [`Self::from_layers`].
    #[cfg(test)]
    pub fn parse(content: &str, repo_root: &Path) -> Result<(Self, Vec<String>), RfError> {
        Self::from_layers(&[(CONFIG_NAME, content)], repo_root)
    }

    /// Parse and merge config layers, lowest precedence first, returning the
    /// config and every warning it earned.
    ///
    /// Merging is by top-level key: a key present in a later layer replaces the
    /// earlier one whole — an array or the `[host_active]` table included —
    /// rather than being spliced into it, so what a repo file says is exactly
    /// what applies and nothing from the global file leaks through a gap in it.
    /// Each layer's unknown keys are reported against that layer's label.
    ///
    /// `repo_root` always comes from git, never from a file: a file's value is
    /// what `rf init` wrote on whichever machine ran it, and the checkout may
    /// since have moved.
    pub fn from_layers(
        layers: &[(&str, &str)],
        repo_root: &Path,
    ) -> Result<(Self, Vec<String>), RfError> {
        let mut warnings = Vec::new();
        let mut merged = toml::Table::new();

        for (label, content) in layers {
            // Unknown keys first, from the raw table, because serde's default
            // is to drop them without a word — which turns `clean_protct = [...]`
            // into a setting that silently never applies.
            let raw: toml::Table = toml::from_str(content)
                .map_err(|e| RfError::Config(format!("{label}: {}", e.to_string().trim_end())))?;
            for (key, value) in raw {
                if Self::KEYS.contains(&key.as_str()) {
                    merged.insert(key, value);
                } else {
                    warnings.push(format!("unknown key '{key}' in {label} is ignored"));
                }
            }
        }

        let mut config: Config = merged.try_into().map_err(|e: toml::de::Error| {
            RfError::Config(format!(
                "{}; run `rf init` to regenerate the file",
                e.to_string().trim_end()
            ))
        })?;
        config.repo_root = repo_root.to_path_buf();

        if config.config_version != CONFIG_VERSION {
            warnings.push(format!(
                "config_version is {} but this rf writes {CONFIG_VERSION}; it still loads",
                config.config_version
            ));
        }
        if !config.roll_prefix.ends_with('/') {
            warnings.push(format!(
                "roll_prefix '{}' does not end in '/'; using '{}/'",
                config.roll_prefix, config.roll_prefix
            ));
            config.roll_prefix.push('/');
        }
        for gate in &config.host_gates {
            if !gate.contains("{host}") {
                warnings.push(format!(
                    "host gate '{gate}' has no {{host}} placeholder and will run identically for every host"
                ));
            }
        }
        for host in &config.hosts {
            if !config.host_active.contains_key(host) {
                warnings.push(format!(
                    "host '{host}' is listed in hosts but not in [host_active]; treated as active"
                ));
            }
        }

        Ok((config, warnings))
    }

    /// Serialize this config to its canonical TOML representation.
    ///
    /// The single source of truth for how a config is rendered on disk, shared
    /// by [`Self::save`] and by the idempotency comparison in `rf init`, so a
    /// re-run compares like-for-like against the existing file.
    pub fn to_toml_string(&self) -> Result<String, RfError> {
        toml::to_string_pretty(self).map_err(|e| RfError::Config(e.to_string()))
    }

    /// Write current config to `<repo>/.roll-flow.toml`.
    pub fn save(&self) -> Result<(), RfError> {
        let path = Self::config_path(&self.repo_root);
        std::fs::write(&path, self.to_toml_string()?)?;
        Ok(())
    }

    /// Detect config from the current git repo without a config file.
    ///
    /// Branch names come from `git branch --list` (the first of
    /// `rolling`/`develop`/`integration`, and `main`/`master`), hosts from
    /// `vars/hosts.nix`, and the username from `vars/default.nix`, then `$USER`,
    /// then git's `user.name`. No `nix` is run: a text scan of two small files
    /// is enough, and it works in a repo where `nix eval` would not.
    pub fn auto_detect() -> Result<Self, RfError> {
        let repo_root = crate::core::git::repo_root(Path::new("."))?;

        let (rolling_branch, stable_branch) = detect_branches(&repo_root)
            .unwrap_or_else(|| ("rolling".to_string(), "main".to_string()));

        let (hosts, host_active) = detect_hosts(&repo_root).unwrap_or_default();
        let username = detect_username(&repo_root).unwrap_or_default();

        Ok(Config {
            config_version: default_config_version(),
            repo_root,
            rolling_branch,
            stable_branch,
            roll_prefix: "roll/".to_string(),
            version_gate: default_true(),
            tag_on_promote: default_true(),
            push_tag: default_true(),
            mode: Mode::default(),
            username,
            hosts,
            host_active,
            roll_to_rolling_gates: vec![],
            rolling_to_main_gates: vec![],
            host_gates: vec![],
            clean_protect: vec![],
            pull_mode: PullMode::default(),
            lazygit_command: default_lazygit(),
        })
    }

    pub fn with_overrides(
        &self,
        rolling_branch: Option<String>,
        stable_branch: Option<String>,
        roll_prefix: Option<String>,
        username: Option<String>,
        hosts: Option<String>,
    ) -> Self {
        let mut updated = self.clone();
        if let Some(v) = rolling_branch {
            updated.rolling_branch = v;
        }
        if let Some(v) = stable_branch {
            updated.stable_branch = v;
        }
        if let Some(v) = roll_prefix {
            updated.roll_prefix = if v.ends_with('/') { v } else { format!("{v}/") };
        }
        if let Some(v) = username {
            updated.username = v;
        }
        if let Some(v) = hosts {
            let parsed: Vec<String> = v
                .split(',')
                .map(|h| h.trim())
                .filter(|h| !h.is_empty())
                .map(ToString::to_string)
                .collect();
            if !parsed.is_empty() {
                updated.hosts = parsed;
            }
        }
        updated
    }

    pub fn config_path(repo_root: &Path) -> PathBuf {
        repo_root.join(CONFIG_NAME)
    }

    /// True when rf is configured to assist rather than drive the workflow.
    ///
    /// Currently informational. This is the intended gate for future
    /// behavioral divergence: e.g. in assist mode, mutating commands
    /// (`create`/`graduate`/`promote`) could refuse to perform merges and
    /// instead report the state and the exact git commands the human should
    /// run. Kept minimal and bounded on purpose (issue #18).
    #[allow(dead_code)]
    pub fn is_assist(&self) -> bool {
        self.mode == Mode::Assist
    }
}

fn default_config_version() -> u32 {
    1
}

fn default_lazygit() -> String {
    "lazygit".to_string()
}

fn default_true() -> bool {
    true
}

// ── Auto-detection helpers ────────────────────────────────────────────────────

/// Heuristically find rolling/stable branch names by inspecting the local
/// branches of the repo.  Falls back to ("rolling", "main").
fn detect_branches(repo_root: &Path) -> Option<(String, String)> {
    use crate::core::git::capture_git;
    let branches = capture_git(repo_root, &["branch", "--list"]).ok()?;
    let names: Vec<&str> = branches
        .lines()
        .map(|l| l.trim().trim_start_matches("* "))
        .collect();

    let rolling = ["rolling", "develop", "integration"]
        .iter()
        .find(|&&c| names.contains(&c))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "rolling".to_string());

    let stable = ["main", "master"]
        .iter()
        .find(|&&c| names.contains(&c))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "main".to_string());

    Some((rolling, stable))
}

/// Read `vars/hosts.nix` for the host list and `host_active` map, or `None`
/// when the file is absent or says nothing usable.
///
/// Two shapes are accepted, because the real file and the one this parser was
/// first written against differ:
///
/// ```nix
/// # what the dotfiles repo actually has: a bare attrset of host → bool
/// { merlin = true; wsl = true; ganoslal = false; }
///
/// # the legacy shape: explicit lists
/// { hosts = [ "merlin" "wsl" ]; host_active = { merlin = true; wsl = false; }; }
/// ```
///
/// A line-by-line scan rather than a Nix parser: the file is tiny and flat,
/// and shelling out to `nix eval` would make `rf init` need a working flake.
/// Comments are stripped first so a commented-out host is not read as one.
fn detect_hosts(repo_root: &Path) -> Option<(Vec<String>, BTreeMap<String, bool>)> {
    let content = std::fs::read_to_string(repo_root.join("vars/hosts.nix")).ok()?;
    parse_hosts_nix(&content)
}

/// The pure half of [`detect_hosts`]; see there for the shapes.
pub(crate) fn parse_hosts_nix(content: &str) -> Option<(Vec<String>, BTreeMap<String, bool>)> {
    let content = strip_nix_comments(content);

    // Legacy shape: an explicit `hosts = [ ... ]` list is authoritative for
    // order, with `host_active = { ... }` alongside.
    if let Some(hosts) = parse_nix_string_list(&content, "hosts") {
        let active = parse_nix_bool_attrs(&content, "host_active");
        return Some((hosts, active));
    }

    // Bare shape: the whole file is the attrset. Every `name = bool;` at the
    // top level is a host; order is the file's, which is what `hosts` keeps.
    let active = parse_nix_bool_attrs_bare(&content);
    if active.is_empty() {
        return None;
    }
    let hosts = active.iter().map(|(h, _)| h.clone()).collect();
    Some((hosts, active.into_iter().collect()))
}

/// The user rolls are attributed to, from the first of: `vars/default.nix`'s
/// `username = "..."`, `$USER`, git's `user.name`. Nothing is hardcoded — the
/// old fallback of `"gig"` was right on one machine and wrong on every other.
fn detect_username(repo_root: &Path) -> Option<String> {
    if let Ok(vars) = std::fs::read_to_string(repo_root.join("vars/default.nix")) {
        if let Some(name) = parse_nix_string_value(&strip_nix_comments(&vars), "username") {
            return Some(name);
        }
    }
    if let Ok(user) = std::env::var("USER") {
        if !user.trim().is_empty() {
            return Some(user);
        }
    }
    crate::core::git::capture_git(repo_root, &["config", "user.name"])
        .ok()
        .filter(|n| !n.is_empty())
}

/// Drop `# ...` comments so a commented-out host or username is not read.
fn strip_nix_comments(content: &str) -> String {
    content
        .lines()
        .map(|l| match l.find('#') {
            Some(at) => &l[..at],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_nix_string_list(content: &str, key: &str) -> Option<Vec<String>> {
    let marker = format!("{key} = [");
    let start = content.find(&marker)? + marker.len();
    let end = content[start..].find(']')? + start;
    let slice = &content[start..end];
    let values = slice
        .split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, s)| s.to_string())
        .collect();
    Some(values)
}

fn parse_nix_bool_attrs(content: &str, key: &str) -> BTreeMap<String, bool> {
    let marker = format!("{key} = {{");
    let Some(start) = content.find(&marker) else {
        return BTreeMap::new();
    };
    let start = start + marker.len();
    let Some(rel_end) = content[start..].find('}') else {
        return BTreeMap::new();
    };
    parse_bool_bindings(&content[start..start + rel_end])
        .into_iter()
        .collect()
}

/// `name = bool;` bindings at the top level of a bare attrset, in file order.
fn parse_nix_bool_attrs_bare(content: &str) -> Vec<(String, bool)> {
    let inner = match (content.find('{'), content.rfind('}')) {
        (Some(open), Some(close)) if close > open => &content[open + 1..close],
        _ => content,
    };
    parse_bool_bindings(inner)
}

/// `name = true;` / `name = false;` pairs, in order; anything else is skipped.
fn parse_bool_bindings(slice: &str) -> Vec<(String, bool)> {
    slice
        .split(';')
        .filter_map(|part| {
            let (name, val) = part.trim().split_once('=')?;
            let name = name.trim();
            let value = match val.trim() {
                "true" => true,
                "false" => false,
                _ => return None,
            };
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-');
            valid.then(|| (name.to_string(), value))
        })
        .collect()
}

fn parse_nix_string_value(content: &str, key: &str) -> Option<String> {
    let marker = format!("{key} = \"");
    let start = content.find(&marker)? + marker.len();
    let end = content[start..].find('"')? + start;
    Some(content[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{default_lazygit, parse_hosts_nix, Config, PullMode, CONFIG_VERSION};
    use std::collections::BTreeMap;

    #[test]
    fn overrides_hosts_and_prefix() {
        let cfg = Config {
            config_version: 1,
            repo_root: PathBuf::from("/tmp/repo"),
            rolling_branch: "rolling".to_string(),
            stable_branch: "main".to_string(),
            roll_prefix: "roll/".to_string(),
            version_gate: true,
            tag_on_promote: true,
            push_tag: true,
            mode: super::Mode::default(),
            username: "old".to_string(),
            hosts: vec!["x".to_string()],
            host_active: Default::default(),
            roll_to_rolling_gates: vec![],
            rolling_to_main_gates: vec![],
            host_gates: vec![],
            clean_protect: vec![],
            pull_mode: PullMode::default(),
            lazygit_command: default_lazygit(),
        };
        let updated = cfg.with_overrides(
            Some("rolling".to_string()),
            Some("main".to_string()),
            Some("roll".to_string()),
            Some("me".to_string()),
            Some("a,b".to_string()),
        );
        assert_eq!(updated.roll_prefix, "roll/");
        assert_eq!(updated.username, "me");
        assert_eq!(updated.hosts, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn mode_round_trips_through_toml() {
        use super::Mode;
        let cfg = Config {
            config_version: 1,
            repo_root: PathBuf::from("/tmp/repo"),
            rolling_branch: "rolling".to_string(),
            stable_branch: "main".to_string(),
            roll_prefix: "roll/".to_string(),
            version_gate: true,
            tag_on_promote: true,
            push_tag: true,
            mode: Mode::Assist,
            username: "me".to_string(),
            hosts: vec![],
            host_active: Default::default(),
            roll_to_rolling_gates: vec![],
            rolling_to_main_gates: vec![],
            host_gates: vec![],
            clean_protect: vec![],
            pull_mode: PullMode::default(),
            lazygit_command: default_lazygit(),
        };
        let rendered = cfg.to_toml_string().expect("render");
        assert!(
            rendered.contains("mode = \"assist\""),
            "mode should serialize as a lowercase string: {rendered}"
        );
        let parsed: Config = toml::from_str(&rendered).expect("parse");
        assert_eq!(parsed.mode, Mode::Assist);
    }

    #[test]
    fn mode_defaults_to_manage_when_absent() {
        use super::Mode;
        // A config written before the `mode` field existed still loads.
        let legacy = r#"
            config_version = 1
            repo_root = "/tmp/repo"
            rolling_branch = "rolling"
            stable_branch = "main"
            roll_prefix = "roll/"
            username = "me"
            hosts = []
        "#;
        let parsed: Config = toml::from_str(legacy).expect("parse legacy");
        assert_eq!(parsed.mode, Mode::Manage);
        assert!(!parsed.is_assist());
        // Fields added later must not make an older config unloadable.
        assert!(parsed.clean_protect.is_empty());
        assert_eq!(parsed.pull_mode, PullMode::FfOnly);
        assert_eq!(parsed.lazygit_command, "lazygit");
    }

    #[test]
    fn pull_mode_round_trips_through_toml_in_kebab_case() {
        for (text, mode) in [
            ("ff-only", PullMode::FfOnly),
            ("merge", PullMode::Merge),
            ("rebase", PullMode::Rebase),
        ] {
            let toml_src = format!(
                r#"
                config_version = 1
                repo_root = "/tmp/repo"
                rolling_branch = "rolling"
                stable_branch = "main"
                roll_prefix = "roll/"
                username = "me"
                hosts = []
                pull_mode = "{text}"
                "#
            );
            let parsed: Config = toml::from_str(&toml_src).expect("parse");
            assert_eq!(parsed.pull_mode, mode, "for {text}");
            // And back out again: `rf init` compares serialized output for
            // idempotency, so a mode that does not round-trip would make every
            // init report a spurious change.
            assert!(
                parsed.to_toml_string().expect("serialize").contains(text),
                "{text} missing from serialized config"
            );
        }
    }

    #[test]
    fn pull_mode_parse_accepts_both_separators_and_rejects_junk() {
        assert_eq!(PullMode::parse("ff_only").unwrap(), PullMode::FfOnly);
        assert_eq!(PullMode::parse("  Rebase ").unwrap(), PullMode::Rebase);
        let err = PullMode::parse("ff").expect_err("should reject");
        assert!(err.to_string().contains("expected"), "{err}");
    }

    #[test]
    fn pull_mode_flags_match_the_git_invocations() {
        assert_eq!(PullMode::FfOnly.flag(), Some("--ff-only"));
        assert_eq!(PullMode::Merge.flag(), None);
        assert_eq!(PullMode::Rebase.flag(), Some("--rebase"));
    }

    const MINIMAL: &str = r#"
        config_version = 1
        repo_root = "/tmp/repo"
        rolling_branch = "rolling"
        stable_branch = "main"
        roll_prefix = "roll/"
        username = "me"
        hosts = []
    "#;

    fn parse(text: &str) -> (Config, Vec<String>) {
        Config::parse(text, std::path::Path::new("/real/checkout")).expect("parse")
    }

    #[test]
    fn known_keys_match_struct() {
        // `KEYS` is what the unknown-key warning and the docs test read. If a
        // field is added to the struct without being listed, the rendered
        // config will carry a key that then warns about itself.
        let (cfg, _) = parse(MINIMAL);
        let rendered = cfg.to_toml_string().expect("render");
        let table: toml::Table = toml::from_str(&rendered).expect("table");
        let mut rendered_keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut known: Vec<&str> = Config::KEYS.to_vec();
        rendered_keys.sort();
        known.sort();
        assert_eq!(rendered_keys, known);
    }

    #[test]
    fn an_unknown_key_warns_and_is_ignored_rather_than_failing() {
        let text = format!("{MINIMAL}\nclean_protct = [\"staging\"]\n");
        let (cfg, warnings) = parse(&text);
        assert!(cfg.clean_protect.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("unknown key 'clean_protct'"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_clean_config_earns_no_warnings() {
        let (_, warnings) = parse(MINIMAL);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_different_config_version_warns_but_loads() {
        let text = MINIMAL.replace("config_version = 1", "config_version = 3");
        let (cfg, warnings) = parse(&text);
        assert_eq!(cfg.config_version, 3, "the file's own value is kept");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("config_version is 3")
                    && w.contains(&CONFIG_VERSION.to_string())),
            "{warnings:?}"
        );
    }

    #[test]
    fn repo_root_comes_from_git_not_the_file() {
        // The file says wherever `rf init` last ran; the checkout may have
        // moved. This is why the repo's own file can say /home/user/... and
        // still work everywhere.
        let (cfg, _) = parse(MINIMAL);
        assert_eq!(cfg.repo_root, PathBuf::from("/real/checkout"));
    }

    #[test]
    fn a_roll_prefix_without_a_slash_is_normalized_with_a_warning() {
        let text = MINIMAL.replace("roll_prefix = \"roll/\"", "roll_prefix = \"roll\"");
        let (cfg, warnings) = parse(&text);
        assert_eq!(cfg.roll_prefix, "roll/");
        assert!(
            warnings.iter().any(|w| w.contains("roll_prefix")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_missing_required_key_says_how_to_regenerate() {
        let text = MINIMAL.replace("rolling_branch = \"rolling\"\n", "");
        let err = Config::parse(&text, std::path::Path::new("/r")).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("rolling_branch"), "{msg}");
        assert!(msg.contains("rf init"), "{msg}");
    }

    #[test]
    fn a_host_gate_without_the_placeholder_is_flagged() {
        let text = format!("{MINIMAL}\nhost_gates = [\"just test-rebuild merlin\"]\n");
        let (_, warnings) = parse(&text);
        assert!(
            warnings.iter().any(|w| w.contains("{host}")),
            "{warnings:?}"
        );
    }

    #[test]
    fn active_hosts_come_from_host_active_when_hosts_is_empty() {
        // The dotfiles shape: `hosts = []` and a populated table. Treating that
        // as "no hosts" silently disabled every host gate there.
        let text =
            format!("{MINIMAL}\n[host_active]\nmerlin = true\nwsl = true\nganoslal = false\n");
        let (cfg, _) = parse(&text);
        assert_eq!(
            cfg.active_hosts(),
            vec!["merlin".to_string(), "wsl".to_string()]
        );

        // With `hosts` given, it fixes the order and may add a host the map
        // does not mention, which counts as active (and is warned about).
        let text = format!(
            "{}\n[host_active]\nmerlin = true\nwsl = false\n",
            MINIMAL.replace("hosts = []", "hosts = [\"wsl\", \"spacedock\", \"merlin\"]")
        );
        let (cfg, warnings) = parse(&text);
        assert_eq!(
            cfg.active_hosts(),
            vec!["spacedock".to_string(), "merlin".to_string()]
        );
        assert!(
            warnings.iter().any(|w| w.contains("spacedock")),
            "{warnings:?}"
        );
    }

    #[test]
    fn hosts_nix_is_read_in_both_shapes() {
        // The real dotfiles file: a bare attrset, with a comment, in file order.
        let real = "# Per-host active status for roll-flow and other tooling.\n\
                    {\n  merlin = true;\n  wsl = true;\n  ganoslal = false;\n  # spare = true;\n}\n";
        let (hosts, active) = parse_hosts_nix(real).expect("bare shape parses");
        assert_eq!(hosts, vec!["merlin", "wsl", "ganoslal"]);
        assert_eq!(active.get("ganoslal"), Some(&false));
        assert!(
            !active.contains_key("spare"),
            "a commented-out host was read"
        );

        // The legacy shape the parser was first written for still works.
        let legacy =
            "{\n  hosts = [ \"a\" \"b\" ];\n  host_active = { a = true; b = false; };\n}\n";
        let (hosts, active) = parse_hosts_nix(legacy).expect("legacy shape parses");
        assert_eq!(hosts, vec!["a", "b"]);
        assert_eq!(
            active,
            BTreeMap::from([("a".to_string(), true), ("b".to_string(), false)])
        );

        // A file that describes no hosts is `None`, not an empty success —
        // `rf init` must not overwrite real hosts with nothing.
        assert!(parse_hosts_nix("{ description = \"x\"; }").is_none());
    }

    #[test]
    fn a_repo_file_overrides_the_global_layer_key_by_key() {
        let global = r#"
            rolling_branch = "develop"
            stable_branch = "main"
            roll_prefix = "roll/"
            username = "me"
            lazygit_command = "lg"
            clean_protect = ["staging", "demo"]
            [host_active]
            merlin = true
            wsl = true
        "#;
        // The repo file can be tiny: it overrides what it names and inherits the rest.
        let repo = r#"
            stable_branch = "master"
            clean_protect = ["only-this"]
        "#;
        let (cfg, warnings) = Config::from_layers(
            &[("global", global), ("repo", repo)],
            std::path::Path::new("/r"),
        )
        .expect("layers merge");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.rolling_branch, "develop", "inherited from global");
        assert_eq!(cfg.stable_branch, "master", "repo wins");
        assert_eq!(cfg.lazygit_command, "lg");
        // Replaced whole, not spliced: nothing of the global array leaks through.
        assert_eq!(cfg.clean_protect, vec!["only-this"]);
        assert_eq!(cfg.active_hosts(), vec!["merlin", "wsl"], "table inherited");
    }

    #[test]
    fn unknown_keys_are_reported_against_the_file_that_has_them() {
        let global =
            "rolling_branch = \"r\"\nstable_branch = \"m\"\nroll_prefix = \"roll/\"\nbogus = 1\n";
        let repo = "clean_protct = []\n";
        let (_, warnings) = Config::from_layers(
            &[
                ("~/.config/roll-flow/config.toml", global),
                (".roll-flow.toml", repo),
            ],
            std::path::Path::new("/r"),
        )
        .expect("parse");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("'bogus' in ~/.config/roll-flow/config.toml")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("'clean_protct' in .roll-flow.toml")),
            "{warnings:?}"
        );
    }

    #[test]
    fn only_the_branch_keys_are_required() {
        // username and hosts used to be required too, which made a global file
        // pointless: the repo file had to repeat them anyway.
        let minimal =
            "rolling_branch = \"rolling\"\nstable_branch = \"main\"\nroll_prefix = \"roll/\"\n";
        let (cfg, warnings) = parse(minimal);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(cfg.username.is_empty());
        assert!(cfg.hosts.is_empty());
        assert_eq!(cfg.repo_root, PathBuf::from("/real/checkout"));
    }

    #[test]
    fn config_path_is_repo_local() {
        let repo = PathBuf::from("/tmp/repo");
        let path = Config::config_path(&repo);
        assert_eq!(path, PathBuf::from("/tmp/repo/.roll-flow.toml"));
    }

    #[test]
    fn release_flags_default_on_for_legacy_configs() {
        // A config written before the release fields existed still loads, and
        // opts in to the version gate and tagging by default.
        let legacy = r#"
            config_version = 1
            repo_root = "/tmp/repo"
            rolling_branch = "develop"
            stable_branch = "main"
            roll_prefix = "roll/"
            username = "me"
            hosts = []
        "#;
        let parsed: Config = toml::from_str(legacy).expect("parse legacy");
        assert!(parsed.version_gate);
        assert!(parsed.tag_on_promote);
        assert!(parsed.push_tag);
    }

    #[test]
    fn release_flags_round_trip_through_toml() {
        let legacy = r#"
            config_version = 1
            repo_root = "/tmp/repo"
            rolling_branch = "develop"
            stable_branch = "main"
            roll_prefix = "roll/"
            username = "me"
            hosts = []
            version_gate = false
            tag_on_promote = false
            push_tag = false
        "#;
        let parsed: Config = toml::from_str(legacy).expect("parse");
        assert!(!parsed.version_gate);
        assert!(!parsed.tag_on_promote);
        assert!(!parsed.push_tag);

        let rendered = parsed.to_toml_string().expect("render");
        let reparsed: Config = toml::from_str(&rendered).expect("reparse");
        assert!(!reparsed.version_gate);
        assert!(!reparsed.tag_on_promote);
        assert!(!reparsed.push_tag);
    }
}
