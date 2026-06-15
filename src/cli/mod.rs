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

    /// Create a new roll branch from rolling: roll/N-MMDD-slug.
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

    /// Verify current branch can be promoted and run configured gates.
    Verify {
        #[arg(long)]
        dry_run: bool,
    },

    /// Promote current branch upward (`roll/* -> rolling` or `rolling -> main`).
    #[command(visible_alias = "graduate")]
    Promote {
        #[arg(long)]
        dry_run: bool,
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

    /// Print program version.
    Version,
}
