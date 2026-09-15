# Architecture

Pure Rust binary. No shell script intermediary layer.

- **CLI**: `clap` with subcommands
- **TUI**: `ratatui` for `rf status` and `rf list` (interactive drill-down)
- **Git operations**: `std::process::Command` shelling out to `git` binary
- **Nix operations**: `std::process::Command` shelling out to `nix` binary
- **Config**: `serde` + TOML at `~/.config/roll-flow/config.toml`

## Planned module layout

```
src/
  main.rs              entry point, CLI dispatch
  error.rs             error types (thiserror)
  cli/
    mod.rs             clap definitions, top-level dispatch
    init.rs
    start.rs
    integrate.rs
    graduate.rs
    promote.rs
    status.rs          → calls tui::status or prints table
    list.rs            → calls tui::list or prints table
    update.rs
  tui/
    mod.rs
    status.rs          ratatui full-screen status view
    list.rs            ratatui list with drill-down to roll detail
    widgets/           reusable ratatui components
  core/
    mod.rs
    config.rs          Config struct, auto-detection from flake.nix
    git.rs             low-level git subprocess wrappers
    branches.rs        branch listing, numbering, location (L/R/B/-)
    scope.rs           scope detection, file categorization
    verification.rs    verification checking (two sources)
    dependencies.rs    dependency resolution (four methods)
    quasi_rolls.rs     quasi-roll detection and analysis
```

## Shell environment

The developer uses Nushell. When testing manually, run commands as:

```nu
cargo run -- status
cargo run -- list
```

The binary itself has no Nushell dependency — it is a plain executable.
