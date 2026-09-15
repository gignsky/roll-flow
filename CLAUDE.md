# CLAUDE.md

Guidance for AI agents working in this repository.

## What this is

`roll-flow` (`rf`) is a Rust CLI+TUI tool that manages the roll-flow git workflow — a
structured multi-host NixOS dotfiles workflow. It is the successor to a monolithic
Nushell script (`scripts/roll-flow` in the dotfiles repo) and is packaged via gigpkgs.

The dotfiles repo is at `~/.dotfiles`. The gigpkgs repo is at `~/local_repos/gigpkgs`.

This repo also dogfoods its own model on itself (see `.roll-flow.toml` at the
repo root and [CONTRIBUTING.md](CONTRIBUTING.md)) — distinct from, and not to
be confused with, its role managing the dotfiles repo described below.

## Build and test

```bash
cargo build
cargo test
cargo run -- status
cargo run -- list --no-tui
```

## Detailed guidance

The sections below are split into their own files so that two rolls editing
different areas do not conflict. They are imported, not merely linked, so they
load into agent context as if they were written here. Add a new rule to the file
whose topic it belongs to rather than appending to the end of one long list.

@./docs/internals/architecture.md
@./docs/internals/domain-model.md
@./docs/internals/algorithms.md
@./docs/internals/invariants.md
@./docs/internals/dotfiles-integration.md

User-facing command documentation lives in [`docs/commands/`](docs/commands),
one file per subcommand, and is enforced against the real CLI surface by
`tests/docs_sync.rs`. A new subcommand needs a new file there and a line in the
`## Commands` block in [README.md](README.md).
