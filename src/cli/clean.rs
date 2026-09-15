//! `rf clean` — render the repo-wide branch cleanup that `core::clean` plans.
//!
//! The one command that runs without a `.roll-flow.toml`, so it resolves the
//! repo root itself rather than going through `Config::load`.

use std::path::Path;

use anyhow::{bail, Result};

use crate::cli::{confirm, Confirm};
use crate::core::{
    clean::{self, CleanItem, CleanOutcome, CleanResult, CleanSkip},
    config::Config,
    git,
};

pub fn run(dry_run: bool, yes: bool, force: bool, with_remote: bool, no_fetch: bool) -> Result<()> {
    // Order matters: `Config::load` resolves the repo root *first*, so
    // `.ok()`-ing it wholesale would swallow "not a git repository" too and
    // leave clean planning against the current directory. Find the repo first
    // and fail hard on that; only the config is optional.
    let repo = git::repo_root(Path::new("."))?;
    let config = Config::load().ok();

    let scope = clean::CleanScope {
        with_remote,
        force,
        fetch: !no_fetch,
    };
    let outcome = clean::plan(&repo, config.as_ref(), &scope)?;

    render_prune(&outcome);
    render_notes(&outcome);

    if outcome.is_empty() {
        if outcome.pruned_count() == 0 {
            println!("nothing to clean");
        }
        render_skips(&outcome.skipped);
        return Ok(());
    }

    render_plan(&outcome);
    render_skips(&outcome.skipped);

    if dry_run {
        if outcome.pruned_count() > 0 {
            println!("\nnote: remote-tracking refs were refreshed; no branches deleted");
        }
        println!("\nDry-run: nothing deleted");
        return Ok(());
    }

    // `--force` widens *what* may be deleted; only `--yes` skips the prompt.
    match confirm(yes, "\nDelete these branches? [y/N] ")? {
        Confirm::Yes => {}
        Confirm::Declined => {
            println!("Nothing deleted.");
            return Ok(());
        }
        Confirm::Unattended => {
            println!("\nNothing deleted. Re-run with --yes to apply.");
            return Ok(());
        }
    }

    println!();
    let results = clean::apply(&repo, &outcome)?;
    let failures = render_results(&results);
    if failures > 0 {
        bail!(
            "{failures} branch{} could not be deleted",
            if failures == 1 { "" } else { "es" }
        );
    }
    Ok(())
}

// ── Prune reporting ───────────────────────────────────────────────────────────

/// Report the stale remote-tracking refs each prune dropped.
///
/// This is the half that clears what lazygit shows in its remote view, so it is
/// reported even when no local branch turns out to be deletable.
fn render_prune(outcome: &CleanOutcome) {
    for pruned in &outcome.pruned {
        if let Some(err) = &pruned.error {
            eprintln!("warning: could not fetch '{}': {err}", pruned.remote);
        }
    }

    let total = outcome.pruned_count();
    if total == 0 {
        return;
    }
    println!(
        "Pruned {total} stale remote-tracking ref{}:",
        if total == 1 { "" } else { "s" }
    );
    for pruned in &outcome.pruned {
        for name in &pruned.removed {
            println!("  {name}");
        }
    }
    println!();
}

/// Explain what clean could not consider, and why.
fn render_notes(outcome: &CleanOutcome) {
    if !outcome.had_config {
        println!("note: no .roll-flow.toml — promoted-roll cleanup skipped");
    }
    if outcome.no_remotes {
        println!("note: no remotes configured");
    }
    match &outcome.base {
        Some(base) => {
            if !outcome.had_config {
                println!(
                    "note: base branch '{}' ({})",
                    base.name,
                    base.source.label()
                );
            }
        }
        None => println!(
            "note: no base branch found (try 'git remote set-head origin -a') \
             — merged-branch cleanup skipped"
        ),
    }
    if outcome.shallow {
        eprintln!("warning: shallow repository — merge detection may be incomplete");
    }
    if outcome.pruned.iter().any(|p| p.error.is_some()) {
        println!("note: results reflect the last successful fetch");
    }
}

// ── Plan table ────────────────────────────────────────────────────────────────

/// Render the branches clean would delete, why, and which copies.
fn render_plan(outcome: &CleanOutcome) {
    let name_w = outcome
        .items
        .iter()
        .map(|i| i.branch.len())
        .max()
        .unwrap_or(6)
        .max(6);
    // Widest label is "promoted"; the forced form appends a commit count.
    let why_w = outcome
        .items
        .iter()
        .map(|i| why_column(i).len())
        .max()
        .unwrap_or(8)
        .max(3);

    println!("Branches to delete:");
    println!();
    println!(
        "  {name:<nw$}  {why:<ww$}  delete",
        name = "branch",
        why = "why",
        nw = name_w,
        ww = why_w,
    );
    println!(
        "  {sep_n}  {sep_w}  ──────",
        sep_n = "─".repeat(name_w),
        sep_w = "─".repeat(why_w),
    );
    for item in &outcome.items {
        println!(
            "  {name:<nw$}  {why:<ww$}  {copies}",
            name = item.branch,
            why = why_column(item),
            copies = copies_column(item),
            nw = name_w,
            ww = why_w,
        );
    }
}

/// The `why` cell: the category, plus what a forced delete would discard.
fn why_column(item: &CleanItem) -> String {
    match item.unmerged {
        Some(n) if n > 0 => format!(
            "{} ({n} commit{})",
            item.reason.label(),
            if n == 1 { "" } else { "s" }
        ),
        _ => item.reason.label().to_string(),
    }
}

fn copies_column(item: &CleanItem) -> String {
    match (item.delete_local, item.delete_remote, &item.remote) {
        (true, true, Some(remote)) => format!("local + {remote}"),
        (false, true, Some(remote)) => remote.clone(),
        _ => "local".to_string(),
    }
}

fn render_skips(skipped: &[CleanSkip]) {
    if skipped.is_empty() {
        return;
    }
    println!();
    println!("Skipped:");
    for skip in skipped {
        println!("  {}: {}", skip.branch, skip.reason);
    }
}

// ── Results ───────────────────────────────────────────────────────────────────

/// Report what was actually deleted. Returns the number of branches that failed,
/// so the caller can set a non-zero exit status.
fn render_results(results: &[CleanResult]) -> usize {
    let mut failures = 0;
    let mut cleaned = 0;
    for result in results {
        if !result.errors.is_empty() {
            failures += 1;
            eprintln!("warning: could not fully delete '{}'", result.branch);
            for err in &result.errors {
                eprintln!("  {err}");
            }
        }
        let copies = match (result.local_deleted, result.remote_deleted) {
            (true, true) => "local + remote",
            (true, false) => "local",
            (false, true) => "remote",
            (false, false) => continue,
        };
        cleaned += 1;
        println!("deleted '{}' ({copies})", result.branch);
    }
    if cleaned > 0 {
        println!();
        println!(
            "Cleaned {cleaned} branch{}",
            if cleaned == 1 { "" } else { "es" }
        );
    }
    failures
}
