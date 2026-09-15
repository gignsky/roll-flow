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
rf clean [--dry-run] [--yes] [--force] [--with-remote] [--no-fetch]
rf version
```

### `init`

- Writes `.roll-flow.toml` at repository root
- Detects branch defaults from repo (`rolling`/`develop`/`integration`, and `main`/`master`)
- Ensures the rolling branch exists (creates it from stable branch when absent)
- `--mode` selects `manage` (rf drives the workflow) or `assist` (a human drives;
  rf reports and derives state). Preserved across re-init when omitted
- `--force` overwrites the config even when it already matches, skipping the diff
  prompt; `--yes` applies detected changes without prompting, for non-interactive use

### `create`

- Requires a clean working tree
- Creates `roll/N-MMDD-slug` off the stable branch (so the roll starts from a clean
  baseline; rolling and other rolls become dependencies only via `rf integrate`)
- Computes `N` as next highest roll number
- Supports `--dry-run`

### `integrate`

Merges a feature branch into the current roll branch, for work split finer than
one roll.

### `hotfix`

Creates `hotfix/N-MMDD-slug` off the stable branch, for urgent fixes that cannot
wait for the next promotion. `--land`, run from the hotfix branch, merges it into
stable with `--no-ff` and then reintegrates stable into rolling so the two do not
drift. `--date` and `--dry-run` behave as they do for `create`.

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

`--force` proceeds past failing gates and requires `--reason <text>`, which is
recorded as a `Force-Reason:` trailer in the merge commit so the bypass stays
auditable in git history.

### `promote`

Merges rolling into the stable branch with `--no-ff` and a structured subject
(`Promote roll/N-slug to main`, or `Promote rolling to main` with the included
rolls listed in the body when several graduated rolls ride along). Run from a
roll branch it redirects to graduation. Conflicts abort and restore, same as
`graduate`, and `--force`/`--reason` behave the same way.

### `status`

Shows current branch, tier, cleanliness, pending rolls, and promotion readiness.
Runs as a full-screen TUI by default; `--no-tui` prints a plain table instead and
`--json` emits machine-readable output.

The TUI table pins the stable and rolling branches above the rolls, so `[space]`
switches to them the same way it switches to a roll. A base branch that exists
neither locally nor on `origin` is not listed.

### `list`

Lists roll branches and states, with the same `--no-tui`/`--json` options as
`status`. `--deps` adds a dependency column to the table.

### `update`

Merges the stable branch into every active local roll branch, bringing them all
up to the current baseline in one pass. Supports `--dry-run`.

### `prune`

Deletes roll branches that have already been promoted to the stable branch,
removing both the local branch and its copy on `origin`. Reachable from the TUI
with `[x]`.

Being promoted is not by itself treated as permission to delete. Promotion is
inferred from commit subjects on stable, which establishes that the roll landed
but not that its branch tip has nothing left on it — a roll can take commits
after its graduation merge. So each copy is additionally checked for containment:
the tip must be an ancestor of the stable branch or of `origin/<stable>`.
Anything failing that check is listed as skipped, with the reason, and is only
deleted with `--force`. The checked-out branch is never deleted locally.

Because `rf` is otherwise local-only, prune runs `git fetch --prune origin` first
so it never acts on stale remote-tracking refs (`--no-fetch` opts out).

- `--dry-run` — show the plan, delete nothing
- `--local` / `--remote` — narrow to one side (default: both)
- `--yes` — skip the confirmation prompt. Run unattended without it, prune
  reports what it would do, deletes nothing, and exits 0
- `--force` — also delete branches whose commits are not contained in stable.
  This widens *what* may be deleted; it does not skip the prompt

### `clean`

The repo-wide branch janitor, and the one command that runs **without** a
`.roll-flow.toml` — it works in any git repository.

The case it exists for: one host creates and pushes `roll/101`, a *different*
host promotes it, and the branch is deleted on the remote. Back on the
originating host nothing notices. The local branch remains, and the cached
`refs/remotes/origin/roll/101` still claims the branch is live, so tools like
lazygit keep offering it in both their local and their remote views. `rf clean`
clears both halves.

It runs `git fetch --prune` against **every** configured remote (not just
`origin`, as `rf prune` does), then deletes local branches in three categories:

| Category | Meaning |
|---|---|
| `gone` | the upstream this branch tracked has been deleted |
| `merged` | fully contained in the base branch, with or without an upstream |
| `promoted` | a roll the config reports as promoted — needs `.roll-flow.toml` |

The base branch is resolved in order: the configured `stable_branch`; else the
remote's own `HEAD` (so a fresh clone works with no local base branch); else the
first of `main`/`master`/`develop`/`trunk` that exists; else the current branch.
With no base at all, merged detection is skipped rather than guessed at.

Every category is then gated on containment: the tip must be an ancestor of the
base branch or its remote-tracking counterpart. A deleted upstream is *not*
proof the local tip is contained, so this gate applies to `gone` branches too —
it is what stands between this command and silent data loss. Anything failing it
is listed as skipped with the reason and deleted only with `--force`, which
reports how many commits would be lost before the prompt.

Never deleted: the current branch, any branch checked out in another worktree
(named with its path), the stable and rolling branches, each remote's default
branch, and anything in `clean_protect`. `--force` does not override these —
they are not containment questions.

- `--dry-run` — show the plan, delete nothing. Remote-tracking refs are still
  refreshed, because without the prune nothing would ever *report* as gone and
  the preview would be misleading. `--no-fetch` opts out of that too
- `--yes` — skip the confirmation prompt. Run unattended without it, clean
  reports what it would do, deletes nothing, and exits 0
- `--force` — delete even when a branch holds commits the base branch lacks.
  This widens *what* may be deleted; it does not skip the prompt
- `--with-remote` — also delete the branches on their remote, using each
  branch's own upstream remote rather than assuming `origin`. Note this
  *extends* clean, whereas `rf prune --remote` *narrows* prune to the remote
  side only
- `--no-fetch` — skip the pruning fetch entirely and plan against cached refs

An unreachable remote is a warning, not a failure: clean carries on against the
last successful fetch's data, and the containment gate still guards every
deletion.

### `version`

Prints the crate version — the same value `Cargo.toml` carries and the Nix
package derives from.

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
clean_protect = []
```

Gate entries are shell commands run in repo root. Any failure blocks verify/promote.

`clean_protect` names branches `rf clean` must never delete, on top of the
stable and rolling branches and each remote's default branch, which it already
protects. Empty by default.

See [CONTRIBUTING.md](CONTRIBUTING.md) for how this repo uses roll-flow on itself.

## Testing

```bash
cargo test
```

## Caveats

- local-only behavior (no automatic fetch/push), except `rf prune`, which
  fetches and deletes branches on `origin`, and `rf clean`, which fetches from
  every remote and deletes there only with `--with-remote`
- no daemon; `rf` only ever runs when invoked. The `status`/`list` TUI does drive
  the workflow (`g` graduate, `p` promote, `u` update, `x` prune), but forced
  operations stay CLI-only by design
