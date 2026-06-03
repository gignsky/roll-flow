mod cli;
mod core;
mod error;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Init { .. } => todo!("rf init"),
        Cmd::Start { .. } => todo!("rf start"),
        Cmd::Integrate { .. } => todo!("rf integrate"),
        Cmd::Graduate { .. } => todo!("rf graduate"),
        Cmd::Promote { .. } => todo!("rf promote"),
        Cmd::Status { .. } => todo!("rf status"),
        Cmd::List { .. } => todo!("rf list"),
        Cmd::Update => todo!("rf update"),
        Cmd::TestAll => todo!("rf test-all"),
    }
}
