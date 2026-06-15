# roll-flow

A production-safe CLI for promoting code through a strict branch pipeline:

```text
main             <- stable
  └─ rolling     <- integration (can also be `develop` if configured)
       └─ roll/N-MMDD-slug <- numbered work branches
```

`roll-flow` 0.1.0 is local-first, non-interactive, and fast-forward-only.

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

Checks promotion readiness for current branch:

- `roll/* -> rolling`
- `rolling -> main`

Validation includes:

- clean tree
- non-detached HEAD
- fast-forward-only ancestry requirement
- configured gate command execution

### `promote`

Runs the same checks as `verify`, then fast-forwards target branch.
No merge commits are created.

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

## Testing

```bash
cargo test
```

## 0.1.0 caveats

- local-only behavior (no automatic fetch/push)
- no daemon/TUI workflow controls
- no merge-based or rebase-based promotion modes
- fast-forward-only promotion enforced
