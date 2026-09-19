# `verify`

```text
rf verify [--dry-run] [--bump <patch|minor|major>] [--yes]
rf verify --all [--state <all|active|blocked|graduated|diverged|local>]
```

Checks graduation/promotion readiness for the current branch. Reachable from the
TUI with `[v]`, which runs the same checks minus the bump offer — see
[`status`](status.md#verifying).

## Verifying many at once

`rf verify --all` (and the TUI's `[V]`) runs the same check for every roll in
`--state` (default `all`) in turn, printing each roll's report under its own
header and a `N passed, M failed, K skipped` line at the end. It exits non-zero
if any roll failed, naming them. Promoted rolls are never included — they have
nowhere left to go.

Because verification judges the checked-out branch and runs the gates in the
working tree, the pass **checks out each roll in turn**. Three rules make that
safe, and all three live in one place (`ops::verify_many`) so the CLI and the TUI
cannot drift:

- a dirty working tree is refused before the first switch;
- the branch you started on is restored at the end, unconditionally — after a
  failed gate, after an error, after a roll that would not check out;
- a roll with no local copy is skipped with a reason, never fetched. `--state
  local` leaves those out entirely.

A roll whose route has nothing to merge counts as skipped, not failed. No bump is
ever offered: a bump is a commit on one branch, and this pass walks many.

Routes checked:

- `roll/* -> rolling`
- `rolling -> main`

Validation includes:

- clean tree
- non-detached HEAD
- mergeability (common history, something new to merge; divergence is fine and
  only produces an informational note)
- the version gate on the `rolling -> main` route (see
  [Versioning and release tags](../../README.md#versioning-and-release-tags)).
  When a bump is needed, `verify` offers one; `--bump <level>` applies it without
  asking and `--yes` accepts the default (patch) non-interactively
- configured gate command execution
