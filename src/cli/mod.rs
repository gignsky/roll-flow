pub mod clean;
pub mod status;

use std::io::IsTerminal;

use clap::{Parser, Subcommand};

// ── Interactive confirmation ──────────────────────────────────────────────────

/// Prompt on stdout and read a yes/no answer from stdin. `y`/`yes`
/// (case-insensitive) is affirmative; anything else is negative.
pub(crate) fn prompt_yes(msg: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let ans = line.trim().to_ascii_lowercase();
    Ok(ans == "y" || ans == "yes")
}

/// How a destructive command's confirmation resolved.
///
/// [`Confirm::Declined`] and [`Confirm::Unattended`] both mean "do nothing", but
/// they are distinct so the caller can explain *why*: a user who answered `n`
/// knows what they did, whereas an unattended run needs to be told `--yes`
/// exists.
pub(crate) enum Confirm {
    Yes,
    Declined,
    Unattended,
}

/// Resolve whether a destructive action may proceed.
///
/// `--yes` always wins; an interactive terminal is prompted; an unattended run
/// without `--yes` declines. That last case is deliberately not an error — it
/// exits 0 having changed nothing, so a command that lands in someone's CI
/// reports rather than deletes.
pub(crate) fn confirm(yes: bool, msg: &str) -> anyhow::Result<Confirm> {
    if yes {
        return Ok(Confirm::Yes);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(Confirm::Unattended);
    }
    if prompt_yes(msg)? {
        Ok(Confirm::Yes)
    } else {
        Ok(Confirm::Declined)
    }
}

#[derive(Parser)]
#[command(
    name = "rf",
    about = "roll-flow: structured NixOS dotfiles workflow manager",
    version
)]
pub struct Cli {
    /// Subcommand to run. When omitted, `rf` shows the status dashboard —
    /// exactly as `rf status` does (issue #100). `--help`/`--version` are still
    /// intercepted by clap before subcommand resolution.
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Initialize roll-flow configuration for this repo.
    Init {
        #[arg(long)]
        rolling_branch: Option<String>,
        #[arg(long)]
        stable_branch: Option<String>,
        #[arg(long)]
        roll_prefix: Option<String>,
        #[arg(long)]
        username: Option<String>,
        /// Comma-separated list of hosts (e.g. ganoslal,merlin,wsl)
        #[arg(long)]
        hosts: Option<String>,
        /// Workflow mode: `manage` (rf drives the workflow) or `assist` (human
        /// drives; rf reports/derives state). Preserved on re-init if omitted.
        #[arg(long)]
        mode: Option<String>,
        /// Overwrite the config even when it already matches, skipping the diff
        /// prompt.
        #[arg(long)]
        force: bool,
        /// Apply detected changes without prompting (non-interactive-safe).
        #[arg(long)]
        yes: bool,
    },

    /// Create a new roll branch off the stable branch: roll/N-MMDD-slug.
    #[command(visible_alias = "start")]
    Create {
        slug: String,
        #[arg(long)]
        date: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },

    /// Merge a feature branch into the current roll.
    Integrate { branch: String },

    /// Create a hotfix branch off stable (hotfix/N-MMDD-slug), or land the
    /// current hotfix into stable and reintegrate rolling with `--land`.
    Hotfix {
        /// Slug for the new hotfix. Required when creating; ignored with --land.
        slug: Option<String>,
        #[arg(long)]
        date: Option<String>,
        /// Land the current hotfix: --no-ff merge into stable, then reintegrate
        /// stable into rolling. Run from a hotfix branch.
        #[arg(long)]
        land: bool,
        #[arg(long)]
        dry_run: bool,
    },

    /// Verify current branch can be promoted and run configured gates.
    Verify {
        #[arg(long)]
        dry_run: bool,
    },

    /// Graduate the current roll branch into rolling (--no-ff merge).
    Graduate {
        #[arg(long)]
        dry_run: bool,
        /// Proceed past failing gates, recording the bypass in the merge commit.
        /// Requires --reason.
        #[arg(long)]
        force: bool,
        /// Justification recorded as `Force-Reason:` in the merge commit.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Promote rolling into the stable branch (--no-ff merge). On a roll
    /// branch, redirects to graduate.
    Promote {
        #[arg(long)]
        dry_run: bool,
        /// Proceed past failing gates, recording the bypass in the merge commit.
        /// Requires --reason.
        #[arg(long)]
        force: bool,
        /// Justification recorded as `Force-Reason:` in the merge commit.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Show current roll-flow status.
    Status {
        #[arg(long)]
        no_tui: bool,
        #[arg(long)]
        json: bool,
    },

    /// List all rolls with verification state.
    List {
        #[arg(long)]
        no_tui: bool,
        /// Include dependency column in the table.
        #[arg(long)]
        deps: bool,
        #[arg(long)]
        json: bool,
    },

    /// Merge the stable branch into all active local roll branches.
    Update {
        #[arg(long)]
        dry_run: bool,
    },

    /// Delete roll branches already promoted to the stable branch, locally and
    /// on origin.
    Prune {
        #[arg(long)]
        dry_run: bool,
        /// Delete only local branches, leaving origin untouched.
        #[arg(long, conflicts_with = "remote")]
        local: bool,
        /// Delete only branches on origin, leaving local ones untouched.
        #[arg(long, conflicts_with = "local")]
        remote: bool,
        /// Delete without prompting for confirmation.
        #[arg(long)]
        yes: bool,
        /// Delete even when a branch has commits not contained in the stable
        /// branch.
        #[arg(long)]
        force: bool,
        /// Skip the `git fetch --prune origin` refresh that precedes planning.
        #[arg(long)]
        no_fetch: bool,
    },

    /// Delete a single roll branch locally, on origin, or both.
    ///
    /// Unlike `prune` this does not require the roll to be promoted — the
    /// branch is named explicitly. Copies holding commits the stable branch
    /// lacks still need `--force`, and the checked-out branch is never deleted.
    Delete {
        /// The roll branch to delete.
        branch: String,
        #[arg(long)]
        dry_run: bool,
        /// Delete only the local branch, leaving origin untouched.
        #[arg(long, conflicts_with = "remote")]
        local: bool,
        /// Delete only the branch on origin, leaving the local one untouched.
        #[arg(long, conflicts_with = "local")]
        remote: bool,
        /// Delete without prompting for confirmation.
        #[arg(long)]
        yes: bool,
        /// Delete even when the branch has commits not contained in the stable
        /// branch.
        #[arg(long)]
        force: bool,
        /// Skip the `git fetch --prune origin` refresh that precedes planning.
        #[arg(long)]
        no_fetch: bool,
    },

    /// Delete stale branches: prune every remote, then remove local branches
    /// whose upstream is gone, that are merged into the base branch, or — in a
    /// roll-flow repo — whose roll is already promoted.
    ///
    /// Works in any git repository, with or without a `.roll-flow.toml`.
    Clean {
        #[arg(long)]
        dry_run: bool,
        /// Delete without prompting for confirmation.
        #[arg(long)]
        yes: bool,
        /// Delete even when a branch holds commits the base branch lacks.
        #[arg(long)]
        force: bool,
        /// Also delete the branches on their remote. Note this *extends* clean,
        /// where `rf prune --remote` *narrows* prune to the remote side only.
        #[arg(long)]
        with_remote: bool,
        /// Skip the `git fetch --prune` refresh that precedes planning.
        #[arg(long)]
        no_fetch: bool,
    },

    /// Print program version.
    Version,
}
