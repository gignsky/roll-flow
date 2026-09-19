# `graduate`

```text
rf graduate [--dry-run] [--force --reason <text>] [--yes]
```

Merges the current roll branch into rolling with `--no-ff` and a structured
subject (`Graduate roll/N-slug into rolling`), then returns to the roll branch.
Divergence between the roll and rolling is handled by the merge; a conflicting
merge is aborted and the original branch restored, leaving the repo clean — and
diagnosed first, see below.

`--force` proceeds past failing gates and requires `--reason <text>`, which is
recorded as a `Force-Reason:` trailer in the merge commit so the bypass stays
auditable in git history.

## When the merge conflicts

A conflict is diagnosed *before* the merge is aborted, while the conflicted
paths still exist. For each one, `rf` walks the rolling branch's first-parent
history since the merge base and reads which merges touched it — through
`branches::extract_graduated_branch`, the one reader of merge subjects — so the
report names the **roll** whose graduation brought the other side in, not just
the file:

```text
merge of 'roll/3-0918-push-all' into 'develop' conflicted in 2 files; the merge was aborted and you are back on 'roll/3-0918-push-all'

  src/tui/rolls.rs
    roll/8-0918-help-menu  (73a2c46: Graduate roll/8-0918-help-menu into develop)
  docs/commands/status.md
    roll/8-0918-help-menu  (73a2c46: Graduate roll/8-0918-help-menu into develop)

The conflicting change is already on 'develop' — it came in with roll/8-0918-help-menu.

Ways forward:
  1) integrate roll/8-0918-help-menu into 'roll/3-0918-push-all' now, resolve there, then re-run rf graduate   [recommended]
  2) nothing now; print the commands to do it by hand
  3) re-run the merge and leave the conflict in the working tree on 'develop' for lazygit
Choose [1-3, default 1]:
```

Option 1 is the roll-flow way of expressing what has happened: this roll now
depends on that one. [`rf integrate`](integrate.md) merges the culprit into the
roll branch, which reproduces the same conflict *there* — on a branch that is
yours to resolve and commit on — and from then on `rf status` shows the
dependency. If every culprit merges cleanly (the roll never touched what the
culprit changed), the graduation is retried on the spot. Option 3 is the only
path that leaves `MERGE_HEAD` behind, and says so.

`--yes` takes option 1 without asking. An unattended run without it takes
option 2 — reports, prints the commands, and changes nothing, so a graduation
that lands in CI never leaves a repo mid-merge. A change made on rolling
directly rather than by a roll is reported as such, and option 1 is not
offered for it.

In the TUI, a conflicting `[G]` puts the same report in the output panel and
opens a modal with the same choices: `[i]` integrate (only when the roll is the
checked-out branch, since integrate merges into HEAD), `[m]` redo the merge on
rolling and leave it for `gg`, `[n]` close.
