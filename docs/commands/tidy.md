# `tidy`

```text
rf tidy [--state <active|blocked|diverged|graduated|promoted|all>,...] [--dry-run] [--yes] [--force] [--no-fetch]
```

Deletes **local** roll branches whose commits survive somewhere else, to clear
what has piled up on one machine. It never touches `origin` — there is no
`--remote` flag here to reach for by accident.

This is the disk-cleanup counterpart to [`prune`](prune.md), and the difference
is the question each one asks. Prune asks *has this landed?*, answers it against
the stable branch, and retires the branch on both sides. Tidy asks the weaker
question *could I get this back?* Since it only ever deletes the local copy, a
branch still sitting on `origin` honestly answers yes — `git fetch` brings it
back, along with its history.

So a local branch is deleted when its tip is contained in any of:

| Where | Why it is enough |
|---|---|
| the stable branch, or `origin/<stable>` | the work was promoted |
| the rolling branch, or `origin/<rolling>` | the work graduated and lives on rolling |
| the branch's own `origin/<branch>` | every commit is pushed; refetchable |

Anything else — a branch holding commits that were never pushed and never
merged — is listed as skipped with the reason, and is deleted only with
`--force`. The local copy of the checked-out branch is never deleted, nor is one
checked out in another worktree; those are reported by name and path rather than
attempted, and `--force` does not override them.

Because the `origin/<branch>` rule is what makes this command useful, `rf tidy`
runs `git fetch --prune origin` before planning. That is not a nicety: a cached
remote-tracking ref for a branch already deleted upstream would certify as
"recoverable" exactly the branches whose only remaining copy is the local one.
`--no-fetch` opts out and plans against cached refs.

## Which rolls it looks at

`--state` selects by roll lifecycle state, and defaults to `graduated,promoted`
— the rolls whose work is merged, where the local branch is pure clutter. Values
may be repeated or comma-separated.

```text
rf tidy                                  # graduated + promoted
rf tidy --state promoted                 # the same set prune would consider
rf tidy --state active --dry-run         # what could go if it is all pushed
rf tidy --state all                      # everything the safety gate allows
```

Widening the states does not widen what may be *deleted*: an active roll is only
reached when it is fully pushed to `origin`, which is the ordinary state of an
active roll you have been working on from more than one machine.

- `--dry-run` — show the plan, delete nothing. The pruning fetch still runs, so
  the preview is not computed against stale refs
- `--yes` — skip the confirmation prompt. Run unattended without it, tidy reports
  what it would do, deletes nothing, and exits 0
- `--force` — also delete branches whose commits are nowhere else, including
  unpushed ones. This widens *what* may be deleted; it does not skip the prompt
- `--no-fetch` — skip the pruning fetch and plan against cached refs

To retire a roll on both sides once it is promoted, use [`prune`](prune.md). To
delete one branch by name regardless of state, use [`delete`](delete.md). To
clear stale branches in any repository, roll or not, see [`clean`](clean.md).
