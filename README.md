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
rf init [--rolling-branch <name>] [--stable-branch <name>] [--roll-prefix <prefix>] [--username <user>] [--hosts <h1,h2>] [--force]
rf create <slug> [--date MMDD] [--dry-run]
rf verify [--dry-run]
rf graduate [--dry-run]
rf promote [--dry-run]
rf status [--json]
rf list [--json]
rf version
```

### `init`

- Writes `.roll-flow.toml` at repository root
- Detects branch defaults from repo (`rolling`/`develop`/`integration`, and `main`/`master`)
- Ensures the rolling branch exists (creates it from stable branch when absent)

### `create`

- Requires a clean working tree
- Creates `roll/N-MMDD-slug` from rolling
- Computes `N` as next highest roll number
- Supports `--dry-run`

### `verify`

Checks graduation/promotion readiness for the current branch:

- `roll/* -> rolling`
- `rolling -> main`

Validation includes:

- clean tree
- non-detached HEAD
- mergeability (common history, something new to merge; divergence is fine and
  only produces an informational note)
- configured gate command execution

### `graduate`

Merges the current roll branch into rolling with `--no-ff` and a structured
subject (`Graduate roll/N-slug into rolling`), then returns to the roll branch.
Divergence between the roll and rolling is handled by the merge; a conflicting
merge is aborted and the original branch restored, leaving the repo clean.

### `promote`

Merges rolling into the stable branch with `--no-ff` and a structured subject
(`Promote roll/N-slug to main`, or `Promote rolling to main` with the included
rolls listed in the body when several graduated rolls ride along). Run from a
roll branch it redirects to graduation. Conflicts abort and restore, same as
`graduate`.

### `status`

Shows current branch, tier, cleanliness, pending rolls, and promotion readiness.
Use `--json` for machine-readable output.

### `list`

Lists roll branches and states. Use `--json` for machine-readable output.

## Config

`.roll-flow.toml` (repo-local):

```toml
config_version = 1
repo_root = "/absolute/path/to/repo"
rolling_branch = "rolling"
stable_branch = "main"
roll_prefix = "roll/"
username = "gig"
hosts = []
roll_to_rolling_gates = []
rolling_to_main_gates = []
```

Gate entries are shell commands run in repo root. Any failure blocks verify/promote.

See [CONTRIBUTING.md](CONTRIBUTING.md) for how this repo uses roll-flow on itself.

## Testing

```bash
cargo test
```

## 0.1.0 caveats

- local-only behavior (no automatic fetch/push)
- no daemon/TUI workflow controls
