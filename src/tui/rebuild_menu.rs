//! The `n` rebuild menu: which entries it offers, when a rebuild queued
//! behind a branch switch may start, and running one with the terminal
//! handed over.
//!
//! Entries come from `rebuild_menu` in `.roll-flow.toml`; an empty list means
//! the built-in plain menu ([`default_entries`]), so any NixOS flake repo gets
//! a working `n` without configuring anything. A repo with its own rebuild
//! scripts — the dotfiles repo, whose scripts write the commits `core::rebuild`
//! reads — points the entries at those instead.
//!
//! Nothing here runs inside the output panel. A rebuild asks for a sudo
//! password, its scripts prompt with `read -p`, and nix draws a live progress
//! display, all of which need the real terminal — the same reason `gg` hands
//! the terminal over rather than streaming.

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::config::{Config, RebuildEntry};

/// How often the background `sudo -n -v` refreshes the credential while a
/// rebuild runs — well inside sudo's default 5- and 15-minute timeouts, the
/// same interval the dotfiles justfile's `sudo-keepalive` uses.
const SUDO_REFRESH: Duration = Duration::from_secs(60);

/// The entries `n` offers: the configured `rebuild_menu`, or
/// [`default_entries`] when that is empty. `{host}` and `{user}` in every
/// command are substituted here, once, so what the menu shows is what runs.
///
/// `Err` names why there is no menu at all — only possible for the defaults,
/// which need a host to build: a configured menu is used as written.
pub fn entries(config: &Config, host_id: Option<&str>) -> Result<Vec<RebuildEntry>, String> {
    let host = host_id.map(flake_target);
    let raw = if config.rebuild_menu.is_empty() {
        let Some(host) = host else {
            return Err(
                "no rebuild_menu in .roll-flow.toml, and this host could not be detected".into(),
            );
        };
        if config.username.is_empty() {
            return Err(
                "no rebuild_menu in .roll-flow.toml, and no username to build home-manager for"
                    .into(),
            );
        }
        default_entries(host, &config.username)
    } else {
        config.rebuild_menu.clone()
    };
    Ok(raw
        .into_iter()
        .map(|mut entry| {
            entry.commands = entry
                .commands
                .iter()
                .map(|c| substitute(c, host, &config.username))
                .collect();
            entry
        })
        .collect())
}

/// The built-in menu, for a repo that configures none: plain `nixos-rebuild`
/// and `home-manager` against the flake in the repo root. Unlike the dotfiles
/// repo's own scripts these write no rebuild commit, so the `build` column has
/// nothing to show for them — that is what pointing `rebuild_menu` at such
/// scripts is for.
///
/// `s` is both halves in order — the same pairing the dotfiles justfile calls
/// `switch` — and `r` is the system half alone.
pub fn default_entries(host: &str, user: &str) -> Vec<RebuildEntry> {
    let nixos = |mode: &str| format!("sudo nixos-rebuild {mode} --flake .#{host}");
    let home = |verb: &str| format!("home-manager {verb} --flake .#{user}@{host}");
    vec![
        RebuildEntry {
            key: 's',
            label: "switch: NixOS, then home-manager".into(),
            commands: vec![nixos("switch"), home("switch")],
            sudo: true,
        },
        RebuildEntry {
            key: 'r',
            label: "rebuild: NixOS switch".into(),
            commands: vec![nixos("switch")],
            sudo: true,
        },
        RebuildEntry {
            key: 't',
            label: "NixOS test (not added to the bootloader)".into(),
            commands: vec![nixos("test")],
            sudo: true,
        },
        RebuildEntry {
            key: 'b',
            label: "NixOS boot (active from the next boot)".into(),
            commands: vec![nixos("boot")],
            sudo: true,
        },
        RebuildEntry {
            key: 'h',
            label: "home-manager switch".into(),
            commands: vec![home("switch")],
            sudo: false,
        },
        RebuildEntry {
            key: 'H',
            label: "home-manager test (build only)".into(),
            commands: vec![home("build")],
            sudo: false,
        },
    ]
}

/// The flake target part of a host identifier — `wsl` of `wsl@merlins-pc`,
/// the same split `core::rebuild::detect` gates on.
fn flake_target(host_id: &str) -> &str {
    host_id.split('@').next().unwrap_or(host_id)
}

fn substitute(command: &str, host: Option<&str>, user: &str) -> String {
    let command = match host {
        Some(host) => command.replace("{host}", host),
        None => command.to_string(),
    };
    command.replace("{user}", user)
}

/// What a rebuild waiting on its branch switch should do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// Something is still running or queued — the switch, or whatever was
    /// pressed after it. Check again next tick.
    Wait,
    /// Nothing is pending and the chosen branch is checked out.
    Run,
    /// Nothing is pending, but the chosen branch is not checked out: the
    /// switch failed, or a later command moved HEAD on. Building now would
    /// build the wrong branch.
    Abandon,
}

/// Decide a pending rebuild. Waits for the queue to drain *entirely* rather
/// than just the switch: anything pressed meanwhile was pressed expecting to
/// run, and the rebuild takes the terminal away from all of it.
pub fn readiness(job_running: bool, queue_empty: bool, current: &str, target: &str) -> Readiness {
    if job_running || !queue_empty {
        Readiness::Wait
    } else if current == target {
        Readiness::Run
    } else {
        Readiness::Abandon
    }
}

/// Run `entry`'s commands in order in `repo`, on the real terminal (the
/// caller has already suspended the TUI), stopping at the first that fails.
///
/// Each runs under `sh -c`, the way gates do, not under `$SHELL`: the
/// commands live in a repo config shared by everyone who clones it, so what
/// they mean must not depend on who is running them (`$SHELL` is nu here,
/// where `#` and `&&` do not mean what they do in sh).
///
/// With `entry.sudo`, the password is asked for once, up front, and kept
/// fresh in the background until the last command finishes — so a prompt
/// buried after minutes of evaluation never sits waiting behind another
/// window.
pub fn run(entry: &RebuildEntry, repo: &Path) -> Result<(), String> {
    let _keepalive = if entry.sudo {
        let status = Command::new("sudo").arg("-v").status();
        match status {
            Ok(s) if s.success() => Some(SudoKeepalive::start()),
            Ok(s) => return Err(format!("sudo -v exited with {s}")),
            Err(err) => return Err(format!("could not run sudo: {err}")),
        }
    } else {
        None
    };
    for command in &entry.commands {
        println!("\n$ {command}");
        let status: ExitStatus = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(repo)
            .status()
            .map_err(|err| format!("could not run '{command}': {err}"))?;
        if !status.success() {
            return Err(format!("'{command}' exited with {status}"));
        }
    }
    Ok(())
}

/// A background thread running `sudo -n -v` every [`SUDO_REFRESH`] until
/// dropped. Non-interactive (`-n`) and silent: it only ever refreshes a
/// credential the foreground `sudo -v` already obtained, and must never
/// prompt over the rebuild's own output.
struct SudoKeepalive {
    stop: Arc<AtomicBool>,
}

impl SudoKeepalive {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !flag.load(Ordering::Relaxed) {
                // Short sleeps so dropping the guard ends the thread promptly
                // rather than up to a minute after the rebuild.
                std::thread::sleep(Duration::from_millis(500));
                if last.elapsed() >= SUDO_REFRESH {
                    let _ = Command::new("sudo")
                        .args(["-n", "-v"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    last = Instant::now();
                }
            }
        });
        SudoKeepalive { stop }
    }
}

impl Drop for SudoKeepalive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s_is_both_halves_and_r_is_the_system_alone() {
        let entries = default_entries("merlin", "gig");
        let by_key = |k| entries.iter().find(|e| e.key == k).unwrap();
        assert_eq!(
            by_key('s').commands,
            vec![
                "sudo nixos-rebuild switch --flake .#merlin",
                "home-manager switch --flake .#gig@merlin",
            ]
        );
        assert_eq!(
            by_key('r').commands,
            vec!["sudo nixos-rebuild switch --flake .#merlin"]
        );
        assert!(!by_key('h').sudo, "home-manager never needs sudo");
        let mut keys: Vec<char> = entries.iter().map(|e| e.key).collect();
        keys.dedup();
        assert_eq!(keys.len(), entries.len(), "every default key is distinct");
    }

    #[test]
    fn the_wsl_physical_host_suffix_is_not_part_of_the_flake_target() {
        assert_eq!(flake_target("wsl@merlins-pc"), "wsl");
        assert_eq!(flake_target("merlin"), "merlin");
    }

    #[test]
    fn placeholders_are_substituted_in_configured_commands() {
        assert_eq!(
            substitute("just test-home {host} # {user}", Some("wsl"), "gig"),
            "just test-home wsl # gig"
        );
        // No host detected: `{host}` is left for the command to fail on
        // visibly, rather than silently becoming an empty argument.
        assert_eq!(substitute("x {host}", None, "gig"), "x {host}");
    }

    #[test]
    fn a_pending_rebuild_waits_for_everything_then_checks_the_branch() {
        assert_eq!(readiness(true, true, "a", "a"), Readiness::Wait);
        assert_eq!(readiness(false, false, "a", "a"), Readiness::Wait);
        assert_eq!(readiness(false, true, "a", "a"), Readiness::Run);
        assert_eq!(readiness(false, true, "a", "b"), Readiness::Abandon);
    }
}
