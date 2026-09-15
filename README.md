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
rf promote [--dry-run] [--force --reason <text>] [--bump <patch|minor|major>] [--no-tag] [--yes]
rf status [--no-tui] [--json]
rf list [--no-tui] [--deps] [--json]
rf update [--dry-run]
rf prune [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
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
- the version gate on the `rolling -> main` route (see
  [Versioning and release tags](#versioning-and-release-tags)). When a bump is
  needed, `verify` offers one; `--bump <level>` applies it without asking and
  `--yes` accepts the default (patch) non-interactively
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

Promotion also owns the release mechanics that previously only happened when you
opened a PR — see [Versioning and release tags](#versioning-and-release-tags):

- refuses to promote unless `Cargo.toml`'s version is above the stable branch's,
  offering to bump it (`--bump <patch|minor|major>` to skip the prompt, `--yes`
  to take the patch default non-interactively)
- creates an annotated `vX.Y.Z` tag on the promotion merge commit, then offers to
  push it to `origin`. `--no-tag` skips tagging entirely
- `--force --reason "<why>"` overrides the version gate, recording it in the
  merge commit alongside any other bypassed gate

### `status`

Shows current branch, tier, cleanliness, pending rolls, and promotion readiness.
Runs as a full-screen TUI by default; `--no-tui` prints a plain table instead and
`--json` emits machine-readable output.

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
version_gate = true
tag_on_promote = true
push_tag = true
roll_to_rolling_gates = []
rolling_to_main_gates = []
```

Gate entries are shell commands run in repo root. Any failure blocks verify/promote.

`version_gate`, `tag_on_promote`, and `push_tag` control the release behavior
described in [Versioning and release tags](#versioning-and-release-tags). All
three default to `true` and are inert in repos without a `Cargo.toml`.

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
  `git push origin <tag>`. This is the only write to the remote outside
  `rf prune`; it never happens without confirmation or `--yes`.

Repos without a `Cargo.toml` — including the dotfiles repo roll-flow was built
for — skip all of this silently.

See [CONTRIBUTING.md](CONTRIBUTING.md) for how this repo uses roll-flow on itself.

## Testing

```bash
cargo test
```

## Caveats

- local-only behavior (no automatic fetch/push), except `rf prune`, which
  fetches and deletes branches on `origin`, and the confirmed release-tag push
  at the end of `rf promote`
- no daemon; `rf` only ever runs when invoked. The `status`/`list` TUI does drive
  the workflow (`g` graduate, `p` promote, `u` update, `x` prune), but forced
  operations stay CLI-only by design
