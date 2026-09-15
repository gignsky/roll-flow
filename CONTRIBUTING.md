# Contributing to roll-flow

roll-flow (`rf`) manages its own development using the same roll-flow model it
implements. See [README.md](README.md) for command reference and
[CLAUDE.md](CLAUDE.md) for the full domain model — this document only covers
how that model applies to *this* repository.

Each topic lives in its own file under [`docs/contributing/`](docs/contributing)
so that changes to unrelated areas do not conflict:

- **[Branch model and day-to-day workflow](docs/contributing/workflow.md)** —
  how `main`/`develop`/`roll/*` relate here, and the create → verify → graduate
  → promote → prune loop.
- **[Two `rf` binaries](docs/contributing/binaries.md)** — why `rf` on `PATH` is
  not your working tree, and when to use `cargo run --` instead.
- **[Documentation is a gate](docs/contributing/docs-gate.md)** — `tests/docs_sync.rs`
  checks the docs against the real CLI surface, and fails `rf verify`/`graduate`/`promote`.
- **[Releases and version bumps](docs/contributing/releases.md)** — every PR into
  `develop` or `main` must raise `version` in `Cargo.toml`.
- **[What CI checks](docs/contributing/ci.md)** — the `build · test · fmt · clippy`
  job and the version-bump check.
- **[Branch protection](docs/contributing/branch-protection.md)** — settings a repo
  admin still needs to apply by hand.

## Adding a subcommand

Adding or changing a subcommand or a flag requires a matching documentation
update, enforced by `tests/docs_sync.rs`. A new subcommand needs both:

1. a line in the `## Commands` block in [README.md](README.md) listing every long flag, and
2. a file at `docs/commands/<name>.md` headed `` # `<name>` ``.
