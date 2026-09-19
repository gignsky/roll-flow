# Architecture

Pure Rust binary. No shell script intermediary layer.

- **CLI**: `clap` with subcommands
- **TUI**: `ratatui` for `rf status` and `rf list` (interactive drill-down)
- **Git operations**: `std::process::Command` shelling out to `git` binary, via
  `core::proc` so output can be captured or inherited
- **Nix operations**: `std::process::Command` shelling out to `nix` binary
- **Config**: `serde` + TOML at `<repo>/.roll-flow.toml` (repo-local, not `~/.config`)

## Module layout

```
src/
  main.rs              entry point, CLI dispatch
  error.rs             RfError (thiserror); everything above core uses anyhow
  cli/
    mod.rs             clap definitions, confirmation prompts
    status.rs          → tui::rolls::run, or prints the plain/JSON table
    clean.rs
  tui/
    mod.rs             terminal enter/exit, and suspend/resume for lazygit
    rolls.rs           the rolls view: state, event loop, table, modals
    output.rs          background jobs and the floating output panel
  core/
    mod.rs
    config.rs          Config struct, auto-detection from flake.nix
    git.rs             low-level git subprocess wrappers, tracking state
    proc.rs            subprocess execution with a per-thread output sink
    branches.rs        branch listing, numbering, location (L/R/B/-)
    ops.rs             create/graduate/promote/update/prune/delete, gates
    sync.rs            pull/push/fetch planning and execution
    clean.rs           stale-branch detection across all remotes
    version.rs         version gate and release tags
```

Two notes on that layout:

- There is one TUI view, not one per command. `rf status` and `rf list` are the
  same screen with different defaults; `tui::rolls` serves both.
- `core` returns data and never prints. The single exception is child-process
  output, which is why `core::proc` exists: it routes a subprocess's stdio to the
  terminal for the CLI and to `tui::output`'s panel for the TUI, chosen by a
  thread-local sink rather than by a parameter threaded through every signature.

## Shell environment

The developer uses Nushell. When testing manually, run commands as:

```nu
cargo run -- status
cargo run -- list
```

The binary itself has no Nushell dependency — it is a plain executable.
