use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RfError;

const CONFIG_NAME: &str = ".roll-flow.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    pub repo_root: PathBuf,
    pub rolling_branch: String,
    pub stable_branch: String,
    pub roll_prefix: String,
    pub username: String,
    pub hosts: Vec<String>,
    #[serde(default)]
    pub host_active: HashMap<String, bool>,
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
        let repo_root =
            crate::core::git::capture_git(Path::new("."), &["rev-parse", "--show-toplevel"])
                .map(PathBuf::from)?;
        let path = Self::config_path(&repo_root);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let mut config: Config =
                toml::from_str(&content).map_err(|e| RfError::Config(e.to_string()))?;
            config.repo_root = repo_root;
            Ok(config)
        } else {
            Self::auto_detect()
        }
    }

    /// Write current config to `<repo>/.roll-flow.toml`.
    pub fn save(&self) -> Result<(), RfError> {
        let path = Self::config_path(&self.repo_root);
        let content = toml::to_string_pretty(self).map_err(|e| RfError::Config(e.to_string()))?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Detect config from the current git repo without a config file.
    /// Reads `flake.nix` branch names and `vars/hosts.nix` for host list.
    pub fn auto_detect() -> Result<Self, RfError> {
        let repo_root =
            crate::core::git::capture_git(Path::new("."), &["rev-parse", "--show-toplevel"])
                .map(PathBuf::from)?;

        let (rolling_branch, stable_branch) = detect_branches(&repo_root)
            .unwrap_or_else(|| ("rolling".to_string(), "main".to_string()));

        let (hosts, host_active, username) = detect_hosts_and_user(&repo_root)
            .unwrap_or_else(|| (vec![], HashMap::new(), "gig".to_string()));

        Ok(Config {
            config_version: default_config_version(),
            repo_root,
            rolling_branch,
            stable_branch,
            roll_prefix: "roll/".to_string(),
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
fn detect_hosts_and_user(repo_root: &Path) -> Option<(Vec<String>, HashMap<String, bool>, String)> {
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

fn parse_nix_bool_attrs(content: &str, key: &str) -> HashMap<String, bool> {
    let mut map = HashMap::new();
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
    fn config_path_is_repo_local() {
        let repo = PathBuf::from("/tmp/repo");
        let path = Config::config_path(&repo);
        assert_eq!(path, PathBuf::from("/tmp/repo/.roll-flow.toml"));
    }
}
