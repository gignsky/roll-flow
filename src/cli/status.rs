use std::io::IsTerminal;

use anyhow::Result;

use crate::core::{
    branches::{self, BranchLocation, RollInfo},
    config::Config,
    git,
};

/// `show_deps` drives the `deps` / `dependants` columns. Unlike `rf list`, where
/// they are opt-in via `--deps`, status shows them by default — knowing what a
/// roll is waiting on is the point of the dashboard — and `--no-deps` hides
/// them.
pub fn run(no_tui: bool, show_deps: bool) -> Result<()> {
    let config = Config::load()?;
    let repo = &config.repo_root;
    let current_branch = git::current_branch(repo)?;
    let rolls = branches::list_rolls(&config)?;

    if !no_tui && std::io::stdout().is_terminal() {
        return crate::tui::rolls::run(config, current_branch, rolls, show_deps);
    }

    let current_roll = branches::get_current_roll(&config)?;
    print_header(&config, &current_branch);
    print_current_roll_line(&config, &current_roll);
    println!();

    if rolls.is_empty() {
        println!("  (no roll branches found)");
    } else {
        print_rolls_table(&config, &rolls, show_deps);
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

fn print_rolls_table(config: &Config, rolls: &[RollInfo], show_deps: bool) {
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
    let (dep_w, dependant_w) = crate::dep_column_widths(rolls);
    // Empty in a repo with no `Cargo.toml`, which drops the column entirely.
    let (versions, ver_w) = crate::version_column(config, rolls);
    // Padded only when the deps columns follow it. Like `dependants`, a last
    // column is left unpadded so rows carry no trailing whitespace.
    let version_col = |branch: &str| {
        if versions.is_empty() {
            return String::new();
        }
        let v = versions.get(branch).map(String::as_str).unwrap_or("—");
        if show_deps {
            format!("  {v:<ver_w$}")
        } else {
            format!("  {v}")
        }
    };

    // Header
    println!(
        "  {num:>nw$}  {name:<ew$}  {loc:<3}  {state:<sw$}{ver_hdr}{deps_hdr}",
        num = "#",
        name = "roll",
        loc = "loc",
        state = "state",
        ver_hdr = if versions.is_empty() {
            String::new()
        } else if show_deps {
            format!("  {:<ver_w$}", crate::VERSION_HDR)
        } else {
            format!("  {}", crate::VERSION_HDR)
        },
        deps_hdr = if show_deps {
            format!("  {:<dep_w$}  {}", crate::DEPS_HDR, crate::DEPENDANTS_HDR)
        } else {
            String::new()
        },
        nw = num_w,
        ew = name_w,
        sw = state_w,
    );
    println!(
        "  {sep_n}  {sep_e}  ───  {sep_s}{sep_v}{sep_d}",
        sep_n = "─".repeat(num_w),
        sep_e = "─".repeat(name_w),
        sep_s = "─".repeat(state_w),
        sep_v = if versions.is_empty() {
            String::new()
        } else {
            format!("  {}", "─".repeat(ver_w))
        },
        sep_d = if show_deps {
            format!("  {}  {}", "─".repeat(dep_w), "─".repeat(dependant_w))
        } else {
            String::new()
        },
    );

    for roll in rolls {
        let cur_marker = if roll.is_current { ">" } else { " " };
        let deps_col = if show_deps {
            format!(
                "  {:<dep_w$}  {}",
                branches::format_roll_numbers(&roll.deps),
                branches::format_roll_numbers(&roll.dependents),
            )
        } else {
            String::new()
        };
        println!(
            "{cur} {num:>nw$}  {name:<ew$}  {loc:<3}  {state:<sw$}{ver_col}{deps_col}",
            cur = cur_marker,
            ver_col = version_col(&roll.branch),
            num = roll.number,
            name = roll.branch,
            loc = location_symbol(&roll.location),
            state = roll.state.label(),
            nw = num_w,
            ew = name_w,
            sw = state_w,
        );
    }
    println!();
    println!("  loc: L=local  R=remote  B=both");
    if show_deps {
        println!("  deps: rolls this one integrated  |  dependants: rolls that integrated it");
    }
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
