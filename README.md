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
rf verify [--dry-run] [--bump <patch|minor|major>] [--yes]
rf graduate [--dry-run] [--force --reason <text>]
rf promote [--roll <branch>]... [--dry-run] [--force --reason <text>] [--bump <patch|minor|major>] [--no-tag] [--yes]
rf status [--no-tui] [--no-deps] [--json]
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

## Versioning and release tags

When the repository has a `Cargo.toml`, `rf` enforces the same release policy as
this project's CI, so promoting locally and promoting through a pull request are
equivalent:

- **Version gate** — `rf verify` and `rf promote` require the `[package]` version
  on the branch being merged to be strictly greater than the version on the
  branch it merges into. This mirrors
  `.github/workflows/version-bump-check.yml`. When the version is unchanged, both
  commands offer to bump it and commit
  `chore(release): bump version to X.Y.Z for <branch>`; a version *lower* than the
  target is always a hard error. The bump is applied before the configured gates
  run, because it rewrites `Cargo.lock` and the gates include
  `cargo update --workspace --locked`.
- **Release tag** — a successful `rf promote` creates an annotated `vX.Y.Z` tag on
  the promotion merge commit, with the promoted rolls listed in the tag body. The
  subject matches the one `.github/workflows/tag-on-main.yml` writes, and, like
  that workflow, an existing tag is left alone rather than treated as an error.
- **Pushing the tag** — `rf promote` then asks before running
  `git push origin <tag>`. It never happens without confirmation or `--yes`.

A `--roll` promotion is several merges, so it gates and tags each one as it is
reached. Only the whole-branch route offers a bump, since only it merges a branch
a bump commit could land on.

Repos without a `Cargo.toml` — including the dotfiles repo roll-flow was built
for — skip all of this silently. `version_gate`, `tag_on_promote`, and `push_tag`
in [docs/config.md](docs/config.md) turn each piece off.

See [CONTRIBUTING.md](CONTRIBUTING.md) for how this repo uses roll-flow on itself,
and [CLAUDE.md](CLAUDE.md) for the internal domain model.

## Testing

```bash
cargo test
```

## Caveats

- no *automatic* fetch or push — every one is a keypress or an explicit command.
  The remote is touched by: `rf prune` and `rf delete` (and the TUI's `[x]` and
  `[d]`), which fetch and delete branches on `origin`; `rf clean`, which fetches
  from every remote and deletes there only with `--with-remote`; the confirmed
  release-tag push at the end of `rf promote`; and the TUI's `[p]`/`[P]`/`[f]`,
  which pull, push and fetch the selected branch
- no daemon; `rf` only ever runs when invoked. The `status`/`list` TUI drives the
  workflow (`G` graduate, `m` promote, `u` update, `x` prune, `d` delete) and
  syncs the selected branch (`p` pull, `P` push, `f` fetch, `gg` lazygit). A
  force *push* is available there behind a confirmation; every other forced
  operation stays CLI-only by design
