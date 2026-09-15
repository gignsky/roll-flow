# roll-flow

A production-safe CLI for promoting code through a strict branch pipeline:

```text
main             <- stable
  └─ rolling     <- integration (can also be `develop` if configured)
       └─ roll/N-MMDD-slug <- numbered work branches
```

`roll-flow` is local-first and merge-based: every graduation and promotion is a
`--no-ff` merge with a structured commit subject, so workflow state can always
be re-derived from git history alone.

## Project ethos

Whenever possible, use Git itself as storage/state/history because it is durable
and auditable. roll-flow follows this by keeping workflow state in branches and
a repository-local config file tracked in the repo.

## Install

Requirements:

- Rust (stable)
- Git

Build:

```bash
cargo build --release
```

Run:

```bash
./target/release/rf --help
```

## Commands

```text
rf init [--rolling-branch <name>] [--stable-branch <name>] [--roll-prefix <prefix>] [--username <user>] [--hosts <h1,h2>] [--mode <manage|assist>] [--force] [--yes]
rf create <slug> [--date MMDD] [--dry-run]            (alias: rf start)
rf integrate <branch>
rf hotfix [<slug>] [--date MMDD] [--land] [--dry-run]
rf verify [--dry-run]
rf graduate [--dry-run] [--force --reason <text>]
rf promote [--dry-run] [--force --reason <text>]
rf status [--no-tui] [--json]
rf list [--no-tui] [--deps] [--json]
rf update [--dry-run]
rf prune [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
rf delete <branch> [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
rf clean [--dry-run] [--yes] [--force] [--with-remote] [--no-fetch]
rf version
```

Each command is documented in its own file under [`docs/commands/`](docs/commands):

| Command | What it does |
|---|---|
| [`init`](docs/commands/init.md) | Write `.roll-flow.toml` and detect branch defaults |
| [`create`](docs/commands/create.md) | Start `roll/N-MMDD-slug` off the stable branch |
| [`integrate`](docs/commands/integrate.md) | Merge a feature branch into the current roll |
| [`hotfix`](docs/commands/hotfix.md) | Branch off stable for urgent fixes, and land them |
| [`verify`](docs/commands/verify.md) | Check readiness and run the configured gates |
| [`graduate`](docs/commands/graduate.md) | Merge the current roll into rolling |
| [`promote`](docs/commands/promote.md) | Merge rolling into the stable branch |
| [`status`](docs/commands/status.md) | Current branch, tier, pending rolls, readiness |
| [`list`](docs/commands/list.md) | All roll branches and their states |
| [`update`](docs/commands/update.md) | Bring every roll up to the current baseline |
| [`prune`](docs/commands/prune.md) | Delete rolls already promoted to stable |
| [`delete`](docs/commands/delete.md) | Delete one named roll branch |
| [`clean`](docs/commands/clean.md) | Repo-wide stale branch janitor; needs no config |
| [`version`](docs/commands/version.md) | Print the crate version |

## Config

See [docs/config.md](docs/config.md) for `.roll-flow.toml` and its gate and
`clean_protect` settings.

See [CONTRIBUTING.md](CONTRIBUTING.md) for how this repo uses roll-flow on itself,
and [CLAUDE.md](CLAUDE.md) for the internal domain model.

## Testing

```bash
cargo test
```

## Caveats

- local-only behavior (no automatic fetch/push), except `rf prune` and
  `rf delete` (and the TUI's `[x]` and `[d]`), which fetch and delete branches
  on `origin`, and `rf clean`, which fetches from every remote and deletes there
  only with `--with-remote`
- no daemon; `rf` only ever runs when invoked. The `status`/`list` TUI does drive
  the workflow (`g` graduate, `p` promote, `u` update, `x` prune, `d` delete),
  but forced operations stay CLI-only by design
