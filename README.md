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
rf promote [--roll <branch>]... [--dry-run] [--force --reason <text>]
rf status [--no-tui] [--no-deps] [--json]
rf list [--no-tui] [--deps] [--json]
rf update [--dry-run]
rf prune [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
rf delete <branch> [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
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

The gates run against the *staged merge result* rather than whatever was checked
out, so what they check is what lands on stable. A gate that modifies tracked
files mid-merge aborts the promotion rather than committing content the gates
never saw.

`--roll <branch>` promotes one graduated roll instead of the whole branch, by
advancing stable to that roll's graduation commit on rolling. Stable therefore
stays a prefix of rolling, and `main` still only ever receives merges from
`rolling` — a roll branch is never merged into stable directly. The flag is
repeatable, works from any branch, and orders the rolls it is given by
graduation, so promoting a roll necessarily carries whatever graduated ahead of
it; a roll already contained in stable is reported as skipped rather than
failing.

Each `--roll` is its own merge behind its own gate run, so a two-roll promotion
runs the gates twice and verifies both intermediate states of stable. Promoting
the whole branch is a single merge, so one gate run covers it. If a later step's
gates fail, the merge is aborted and the earlier steps stay committed.

### `status`

Shows current branch, tier, cleanliness, pending rolls, and promotion readiness.
Runs as a full-screen TUI by default; `--no-tui` prints a plain table instead and
`--json` emits machine-readable output.

The table carries a `deps` column (roll numbers this roll integrated) and a
`dependants` column (roll numbers that integrated it) — `--no-deps` hides both.
They are shown whatever the roll's state, so a roll that has already graduated
still reports what it depends on and what depends on it. Press `[enter]` on a
roll for the detail overlay, which breaks the same two relationships out with
per-dependency blocker markers.

The TUI table pins the stable and rolling branches above the rolls, so `[space]`
switches to them the same way it switches to a roll. A base branch that exists
neither locally nor on `origin` is not listed.

### `list`

Lists roll branches and states, with the same `--no-tui`/`--json` options as
`status`. `--deps` adds the `deps`/`dependants` columns, which `list` leaves off
by default.

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
### `delete`

Deletes one named roll branch — locally, on `origin`, or both. Reachable from the
TUI with `[d]` on the selected row, and accepts the same flags as `prune`.

It relaxes exactly one of prune's rules: the roll need not be promoted. Prune
picks its own targets by inferring promotion from commit subjects, so it must
refuse on doubt; `delete` acts on a branch the user named, which makes an
abandoned or superseded roll a legitimate target. Everything else still holds —
an uncontained copy needs `--force`, the checked-out branch is never deleted
locally, and `main`/`rolling` are refused outright.

In the TUI the prompt follows where the branch actually lives:

- **local-only or remote-only** — a `[y]`/`[n]` confirmation defaulting to no.
  Nothing happens without a deliberate `y`; Enter is not a shortcut for it
- **both** — `[l]` local only, `[r]` origin only, `[b]` both, `[n]` neither

If the copies you chose hold commits the stable branch lacks, that keypress does
*not* delete. It swaps the modal for a second one naming how many commits would
be lost, which needs a fresh `y` — and that second `y` is the only thing that
ever forces.

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
- local-only behavior (no automatic fetch/push), except `rf prune` and
  `rf delete` (and the TUI's `[x]` and `[d]`), which fetch and delete branches
  on `origin`
- no daemon
