# `graduate`

```text
rf graduate [--dry-run] [--force --reason <text>] [--yes]
```

Merges the current roll branch into rolling with `--no-ff` and a structured
subject (`Graduate roll/N-slug into rolling`), then returns to the roll branch.
Divergence between the roll and rolling is handled by the merge; a conflicting
merge is aborted and the original branch restored, leaving the repo clean.

`--force` proceeds past failing gates and requires `--reason <text>`, which is
recorded as a `Force-Reason:` trailer in the merge commit so the bypass stays
auditable in git history.

## Dependencies graduate first

A roll that [integrated](integrate.md) another is `⛔ blocked` until that one
graduates. Rather than sending you off to do that by hand, `rf graduate` walks
the dependency chain: every ungraduated dependency, transitively and in the
order they must land, then the roll you asked for. The plan is printed and
confirmed first, because it merges more than the branch you are standing on:

```text
'roll/9-0918-better-deps' depends on 1 ungraduated roll; graduating in order:
  1. roll/8-0918-help-menu  (dependency of 9, active)
  2. roll/9-0918-better-deps  (⛔ blocked)

Graduate these in order? [y/N]
```

`--yes` takes the plan as read. Unattended (no terminal and no `--yes`) the plan
is printed, nothing is merged, and the exit is 0 — the same shape as every other
confirmation. `--dry-run` prints the plan and previews each step. A roll with
nothing ungraduated beneath it graduates without a word, exactly as before.

Each step is an ordinary graduation — its own gate run, its own `--no-ff` merge,
the same abort-and-restore on conflict — so a chain is precisely what running
`rf graduate` on each roll by hand would have been. A `diverged` dependency
re-graduates; one that has already graduated is history and is not touched. If
a step fails, the error names which rolls graduated, which one failed, and which
were not attempted, since by then the earlier merges are committed.

Refused before anything runs, naming the roll: a dependency that exists only on
`origin` (fetch it, or press `[space]` on it in the TUI), a dependency number no
known roll carries, and a dependency cycle. The TUI's `[G]` on a blocked roll
opens the same plan in its confirm modal.
