pub mod status;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rf",
    about = "roll-flow: structured NixOS dotfiles workflow manager",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
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
        #[arg(long)]
        force: bool,
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

    /// Print program version.
    Version,
}
