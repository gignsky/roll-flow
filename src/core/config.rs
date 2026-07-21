use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RfError;

const CONFIG_NAME: &str = ".roll-flow.toml";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    pub repo_root: PathBuf,
    pub rolling_branch: String,
    pub stable_branch: String,
    pub roll_prefix: String,
    /// Workflow ownership mode. Defaults to [`Mode::Manage`] for configs that
    /// predate this field (via `#[serde(default)]`).
    #[serde(default)]
    pub mode: Mode,
    pub username: String,
    pub hosts: Vec<String>,
    #[serde(default)]
    pub host_active: BTreeMap<String, bool>,
    #[serde(default)]
    pub roll_to_rolling_gates: Vec<String>,
    #[serde(default)]
    pub rolling_to_main_gates: Vec<String>,
}

impl Config {
    /// Hosts that are currently active (inactive ones are offline/rebuilding).
    /// Consumed by the multi-source verification work in later epics.
    #[allow(dead_code)]
    pub fn active_hosts(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|h| self.host_active.get(h.as_str()).copied().unwrap_or(true))
            .cloned()
            .collect()
    }

    /// Load config from `<repo>/.roll-flow.toml`, or auto-detect if it does not
    /// exist yet.
    pub fn load() -> Result<Self, RfError> {
        let repo_root = crate::core::git::repo_root(Path::new("."))?;
        let path = Self::config_path(&repo_root);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let mut config: Config =
                toml::from_str(&content).map_err(|e| RfError::Config(e.to_string()))?;
            config.repo_root = repo_root;
            Ok(config)
        } else {
            Err(RfError::Config(format!(
                "no roll-flow config found at {}; run `rf init` first",
                path.display()
            )))
        }
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
    /// Reads `flake.nix` branch names and `vars/hosts.nix` for host list.
    pub fn auto_detect() -> Result<Self, RfError> {
        let repo_root = crate::core::git::repo_root(Path::new("."))?;

        let (rolling_branch, stable_branch) = detect_branches(&repo_root)
            .unwrap_or_else(|| ("rolling".to_string(), "main".to_string()));

        let (hosts, host_active, username) = detect_hosts_and_user(&repo_root)
            .unwrap_or_else(|| (vec![], BTreeMap::new(), "gig".to_string()));

        Ok(Config {
            config_version: default_config_version(),
            repo_root,
            rolling_branch,
            stable_branch,
            roll_prefix: "roll/".to_string(),
            mode: Mode::default(),
            username,
            hosts,
            host_active,
            roll_to_rolling_gates: vec![],
            rolling_to_main_gates: vec![],
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

/// Parse `vars/hosts.nix` for the host list and `host_active` map.
/// Returns (hosts, host_active, username).  Returns None on parse failure.
fn detect_hosts_and_user(
    repo_root: &Path,
) -> Option<(Vec<String>, BTreeMap<String, bool>, String)> {
    // Parse vars/hosts.nix with a simple regex-free approach:
    // the file is expected to look like:
    //
    //   {
    //     hosts = [ "ganoslal" "merlin" "wsl" ];
    //     host_active = { ganoslal = true; merlin = true; wsl = false; };
    //     username = "gig";
    //   }
    //
    // We use a lightweight line-by-line scan rather than a full Nix parser.
    let hosts_nix = repo_root.join("vars/hosts.nix");
    let content = std::fs::read_to_string(&hosts_nix).ok()?;

    let hosts = parse_nix_string_list(&content, "hosts")?;
    let host_active = parse_nix_bool_attrs(&content, "host_active");
    let username =
        parse_nix_string_value(&content, "username").unwrap_or_else(|| "gig".to_string());

    Some((hosts, host_active, username))
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
    let mut map = BTreeMap::new();
    let marker = format!("{key} = {{");
    let Some(start) = content.find(&marker) else {
        return map;
    };
    let start = start + marker.len();
    let Some(rel_end) = content[start..].find('}') else {
        return map;
    };
    let slice = &content[start..start + rel_end];
    for part in slice.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((name, val)) = part.split_once('=') {
            let name = name.trim().to_string();
            let active = val.trim() == "true";
            map.insert(name, active);
        }
    }
    map
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

    use super::Config;

    #[test]
    fn overrides_hosts_and_prefix() {
        let cfg = Config {
            config_version: 1,
            repo_root: PathBuf::from("/tmp/repo"),
            rolling_branch: "rolling".to_string(),
            stable_branch: "main".to_string(),
            roll_prefix: "roll/".to_string(),
            mode: super::Mode::default(),
            username: "old".to_string(),
            hosts: vec!["x".to_string()],
            host_active: Default::default(),
            roll_to_rolling_gates: vec![],
            rolling_to_main_gates: vec![],
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
            mode: Mode::Assist,
            username: "me".to_string(),
            hosts: vec![],
            host_active: Default::default(),
            roll_to_rolling_gates: vec![],
            rolling_to_main_gates: vec![],
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
    }

    #[test]
    fn config_path_is_repo_local() {
        let repo = PathBuf::from("/tmp/repo");
        let path = Config::config_path(&repo);
        assert_eq!(path, PathBuf::from("/tmp/repo/.roll-flow.toml"));
    }
}
