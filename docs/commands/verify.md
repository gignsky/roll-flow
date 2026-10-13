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

A roll whose route has nothing to merge counts as skipped, not failed.

Each roll's version is checked the way a single `rf verify` checks it, and then
some:

- a missing `-roll<N>` dev marker is applied first (see below), committed on
  that roll before its gates run, and reported as `version marked …`;
- a roll whose version *numbers* are behind the stable branch's (`0.2.3-roll13`
  after `main` released `0.2.7`) fails without running its gates. It has missed
  a release; [`rf update`](update.md) (`[u]`) brings it up to date. The marker is
  ignored in this comparison — `-roll<N>` always sorts below the same release —
  and the check is off when `version_gate = false`.

No bump is ever offered: a bump is a commit on one branch, and this pass walks
many. The dev marker is the only commit it makes.

Routes checked:

- `roll/* -> rolling`
- `rolling -> main`

On a roll branch, `verify` also applies the roll's dev marker to `Cargo.toml` if
it is missing (`0.2.4` → `0.2.4-roll9`, see [`create`](create.md#dev-versions))
— so a roll created before the marker existed, or with `--no-dev-version`, is
brought in line by its first verify. Idempotent, skipped by `--dry-run`, and
off when `dev_versions = false`.

Validation includes:

- clean tree
- non-detached HEAD
- mergeability (common history, something new to merge; divergence is fine and
  only produces an informational note)
- merge conflicts: when the branches have diverged, the graduation/promotion
  merge is tried in git's object store (`git merge-tree`, git 2.38+) without
  touching the working tree. A conflict fails `verify` before any gate runs,
  naming each conflicted path and the roll (or direct commit) on the target that
  changed it, and printing the fix — `rf integrate <culprit>` from a roll branch,
  as [`graduate`](graduate.md#when-the-merge-conflicts) recommends, or merging the
  target into the source. Under `--dry-run` it is a warning instead.
- the version gate on the `rolling -> main` route (see
  [Versioning and release tags](../../README.md#versioning-and-release-tags)),
  checked against the *finalized* (marker-stripped) version rolling would carry
  once promoted. When a bump is needed, `verify` offers one; `--bump <level>`
  applies it without asking and `--yes` accepts the default (patch)
  non-interactively. Resolving a bump this way also finalizes rolling's
  `-dev` marker first, the same way a confirmed `rf promote` does — `verify`
  never asks "is this final?" itself, since a bump here is already an explicit,
  opt-in action
- configured gate command execution

## Example

```text
$ rf verify
merging 'roll/3-0612-second' into 'rolling' would conflict in 1 file

  shared.txt
    roll/1-0611-first  (548fd6b: Graduate roll/1-0611-first into rolling)

The conflicting change is already on 'rolling' — it came in with roll/1-0611-first.
From 'roll/3-0612-second', take the conflict onto it, resolve, commit, then re-run rf verify:
  rf integrate roll/1-0611-first
Or merge 'rolling' itself instead:
  git checkout roll/3-0612-second && git merge rolling
Error: verification failed: 'roll/3-0612-second' does not merge cleanly into 'rolling'
```
