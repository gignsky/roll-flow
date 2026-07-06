use std::io::IsTerminal;

use anyhow::Result;

use crate::core::{
    branches::{self, BranchLocation, RollInfo},
    config::Config,
    git,
};

pub fn run(no_tui: bool) -> Result<()> {
    let config = Config::load()?;
    let repo = &config.repo_root;
    let current_branch = git::current_branch(repo)?;
    let rolls = branches::list_rolls(&config)?;

    if !no_tui && std::io::stdout().is_terminal() {
        return crate::tui::rolls::run(crate::tui::rolls::TuiContext {
            config: &config,
            current_branch: &current_branch,
            rolls: &rolls,
            show_deps: false,
        });
    }

    let current_roll = branches::get_current_roll(&config)?;
    print_header(&config, &current_branch);
    print_current_roll_line(&config, &current_roll);
    println!();

    if rolls.is_empty() {
        println!("  (no roll branches found)");
    } else {
        print_rolls_table(&rolls);
    }

    Ok(())
}

// ── Header ────────────────────────────────────────────────────────────────────

fn print_header(config: &Config, current_branch: &str) {
    println!("=== Roll Flow Status ===");
    println!(
        "Current: {current_branch}  |  Rolling: {}  |  Stable: {}",
        config.rolling_branch, config.stable_branch
    );
}

fn print_current_roll_line(config: &Config, current_roll: &Option<String>) {
    match current_roll {
        Some(roll) => {
            let state =
                if branches::check_graduated(&config.repo_root, roll, &config.rolling_branch) {
                    if branches::check_diverged(&config.repo_root, roll, &config.rolling_branch) {
                        "graduated — ⚠ DIVERGED (has new commits since graduation)"
                    } else {
                        "graduated — merged to rolling"
                    }
                } else {
                    "active — not yet graduated"
                };
            println!("✓ On roll: {roll}  [{state}]");
        }
        None => {
            println!("  (not on a roll branch)");
        }
    }
}

// ── Roll table ────────────────────────────────────────────────────────────────

fn print_rolls_table(rolls: &[RollInfo]) {
    // Compute column widths dynamically.
    let name_w = rolls
        .iter()
        .map(|r| r.branch.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let num_w = rolls
        .iter()
        .map(|r| digits(r.number))
        .max()
        .unwrap_or(1)
        .max(1);
    let state_w = "✓ graduated".len(); // longest label

    // Header
    println!(
        "  {num:>nw$}  {name:<ew$}  {loc:<3}  state",
        num = "#",
        name = "roll",
        loc = "loc",
        nw = num_w,
        ew = name_w,
    );
    println!(
        "  {sep_n}  {sep_e}  ───  {sep_s}",
        sep_n = "─".repeat(num_w),
        sep_e = "─".repeat(name_w),
        sep_s = "─".repeat(state_w),
    );

    for roll in rolls {
        let cur_marker = if roll.is_current { ">" } else { " " };
        println!(
            "{cur} {num:>nw$}  {name:<ew$}  {loc:<3}  {state}",
            cur = cur_marker,
            num = roll.number,
            name = roll.branch,
            loc = location_symbol(&roll.location),
            state = roll.state.label(),
            nw = num_w,
            ew = name_w,
        );
    }
    println!();
    println!("  loc: L=local  R=remote  B=both");
}

fn location_symbol(loc: &BranchLocation) -> &'static str {
    match loc {
        BranchLocation::Local => "L",
        BranchLocation::Remote => "R",
        BranchLocation::Both => "B",
        BranchLocation::Neither => "-",
    }
}

fn digits(n: u32) -> usize {
    if n == 0 {
        1
    } else {
        (n as f64).log10().floor() as usize + 1
    }
}
