//! Shell-command history for the TUI's `:` command runner.
//!
//! Stored outside the repo, alongside the global config
//! (`~/.config/roll-flow/command_history.toml`), since it is a per-user habit
//! across every repo `rf` manages rather than anything a single checkout
//! should know about.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RfError;

/// How many commands are kept, most-recent-first. Old entries fall off the
/// end rather than letting the file grow without bound over a long-lived
/// machine.
const CAP: usize = 200;

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    commands: Vec<String>,
}

/// `$XDG_CONFIG_HOME/roll-flow/command_history.toml`, or
/// `~/.config/roll-flow/command_history.toml`. `None` when neither variable
/// is usable — mirrors
/// [`crate::core::config::Config::global_config_path`], which is the same
/// question for the global config file.
pub fn history_path() -> Option<PathBuf> {
    history_path_from(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// The path-joining logic behind [`history_path`], taking the two candidate
/// bases as plain values rather than reading the environment itself — so the
/// join can be tested without mutating process-global env vars, which would
/// race against every other test reading them concurrently.
fn history_path_from(xdg_config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".config")))?;
    Some(base.join("roll-flow").join("command_history.toml"))
}

/// Load the saved history, most-recent-first. A missing or unreadable file is
/// empty history, not an error — mirrors [`crate::core::config::Config::load`]'s
/// treatment of its own optional global file.
pub fn load() -> Vec<String> {
    match history_path() {
        Some(path) => load_from(&path),
        None => Vec::new(),
    }
}

fn load_from(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    toml::from_str::<HistoryFile>(&text)
        .map(|f| f.commands)
        .unwrap_or_default()
}

/// Write `history` to disk, creating the parent directory if it doesn't exist
/// yet. Unlike the repo config's directory (which `rf init` already created),
/// nothing guarantees this one exists before the first command is ever run.
pub fn save(history: &[String]) -> Result<(), RfError> {
    match history_path() {
        Some(path) => save_to(&path, history),
        None => Ok(()),
    }
}

fn save_to(path: &Path, history: &[String]) -> Result<(), RfError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = HistoryFile {
        commands: history.to_vec(),
    };
    let text = toml::to_string_pretty(&file).map_err(|e| RfError::Parse(e.to_string()))?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Record `cmd` as just run: move it to the front if already present
/// (dedupe), otherwise insert it at the front, then cap the list.
///
/// A blank command (whitespace only) is dropped rather than recorded — there
/// is nothing useful to find it by later.
pub fn record(history: &mut Vec<String>, cmd: &str) {
    if cmd.trim().is_empty() {
        return;
    }
    history.retain(|c| c != cmd);
    history.insert(0, cmd.to_string());
    history.truncate(CAP);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_config_home_wins_when_absolute() {
        let path = history_path_from(
            Some(PathBuf::from("/xdg")),
            Some(PathBuf::from("/home/gig")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/xdg/roll-flow/command_history.toml"));
    }

    #[test]
    fn a_relative_xdg_config_home_is_ignored_in_favour_of_home() {
        let path = history_path_from(
            Some(PathBuf::from("relative")),
            Some(PathBuf::from("/home/gig")),
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/gig/.config/roll-flow/command_history.toml")
        );
    }

    #[test]
    fn falls_back_to_home_dot_config() {
        let path = history_path_from(None, Some(PathBuf::from("/home/gig"))).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/gig/.config/roll-flow/command_history.toml")
        );
    }

    #[test]
    fn neither_variable_usable_is_none_not_an_error() {
        assert_eq!(history_path_from(None, None), None);
    }

    #[test]
    fn recording_a_new_command_puts_it_at_the_front() {
        let mut history = vec!["old".to_string()];
        record(&mut history, "new");
        assert_eq!(history, vec!["new".to_string(), "old".to_string()]);
    }

    #[test]
    fn recording_a_repeat_moves_it_to_the_front_instead_of_duplicating() {
        let mut history = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        record(&mut history, "b");
        assert_eq!(
            history,
            vec!["b".to_string(), "a".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn a_blank_command_is_not_recorded() {
        let mut history = vec!["a".to_string()];
        record(&mut history, "   ");
        assert_eq!(history, vec!["a".to_string()]);
    }

    #[test]
    fn recording_past_the_cap_drops_the_oldest() {
        let mut history: Vec<String> = (0..CAP).map(|i| i.to_string()).collect();
        record(&mut history, "newest");
        assert_eq!(history.len(), CAP);
        assert_eq!(history[0], "newest");
        assert!(!history.contains(&(CAP - 1).to_string()));
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("command_history.toml");
        let history = vec!["git status".to_string(), "ls -la".to_string()];
        save_to(&path, &history).expect("save");
        assert_eq!(load_from(&path), history);
    }

    #[test]
    fn loading_a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        assert_eq!(load_from(&path), Vec::<String>::new());
    }
}
