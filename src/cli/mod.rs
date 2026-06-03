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
    },

    /// Start a new roll branch: roll/N-<theme>.
    Start { theme: String },

    /// Merge a feature branch into the current roll.
    Integrate { branch: String },

    /// Graduate rolls from the current branch into rolling.
    Graduate {
        /// Specific roll branches to graduate (defaults to interactive selection).
        branches: Vec<String>,
        #[arg(long)]
        all: bool,
    },

    /// Promote graduated rolls from rolling into main.
    Promote {
        /// Specific roll branches to promote (defaults to interactive selection).
        branches: Vec<String>,
        #[arg(long)]
        all: bool,
    },

    /// Show current roll-flow status.
    Status {
        #[arg(long)]
        no_tui: bool,
    },

    /// List all rolls with verification state.
    List {
        #[arg(long)]
        no_tui: bool,
        /// Include dependency column in the table.
        #[arg(long)]
        deps: bool,
    },

    /// Merge main into all ungraduated local roll branches.
    Update,

    /// Run flake check + host builds and record results.
    #[command(name = "test-all")]
    TestAll,
}
