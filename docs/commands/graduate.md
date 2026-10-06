# `graduate`

```text
rf graduate [--dry-run] [--force --reason <text>] [--no-tag] [--yes]
```

Merges the current roll branch into rolling with `--no-ff` and a structured
subject (`Graduate roll/N-slug into rolling`), then returns to the roll branch.
Divergence between the roll and rolling is handled by the merge; a conflicting
merge is aborted and the original branch restored, leaving the repo clean — and
diagnosed first, see below.

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

Each step is an ordinary graduation — its own gate run, its own `--no-ff`
merge, the same abort-and-restore on conflict — so a chain is precisely what
running `rf graduate` on each roll by hand would have been. A `diverged` or
`reverted` dependency re-graduates; one that has already graduated is history
and is not touched (a `reverted` one is restored by reverting the revert, as
below). If a step fails, the error names which rolls graduated, which one
failed, and which were not attempted, since by then the earlier merges are
committed. Only the step that merges the roll you named can take the
interactive "ways forward" a conflict offers (below) — `ops::integrate` merges
into HEAD, and HEAD is that roll throughout the chain. A conflicting
dependency or carrier step instead prints the commands to resolve it by hand,
even with `--yes`, since integrating its culprit into the wrong branch would
make things worse, not better.

Refused before anything runs, naming the roll: a dependency that exists only on
`origin` (fetch it, or press `[space]` on it in the TUI), a dependency number no
known roll carries, and a dependency cycle with no carrier (below). The TUI's
`[G]` on a blocked roll opens the same plan in its confirm modal.

## Dependency cycles

Two rolls can end up depending on each other: roll 14 is built on roll 15
(`rf integrate` of 15 into 14), and later 15 folds 14's work in (`rf integrate`
of 14 into 15). Each now waits for the other, and no order of separate
graduations satisfies both.

The way out is containment. The ordering rule exists so a dependency's commits
reach rolling *before or with* the roll that integrated them — and once 15
contains 14's tip, graduating 15 lands all of 14 in the same merge. So in a
cycle, the member that contains every other member's tip is the *carrier*: it
graduates as one ordinary merge and carries the rest; afterwards they read
`✓ graduated` too, through its integrate merge. Its plan says so:

```text
roll/15-0919-show-hotfixes  (active, carries 14)

Graduated 'roll/15-0919-show-hotfixes' into 'rolling'
  carried roll/14-0919-add-hotfix-to-menu with it (dependency cycle)
```

The carrier graduates without a prompt, as any lone roll does — the merge is its
own branch, and what it carries is already inside it. Running `rf graduate` from
a carried member plans the carrier in its place (that is the only way the member
can land), and since that merges a branch you are not standing on, it is shown
and confirmed like any other chain. Dependencies *outside* the cycle — of any
member, since the carrier's merge lands them all — still graduate first.

If no member contains the others — each kept working after integrating the
other — any graduation would land a partial copy of some member, so it is
refused, with the fix spelled out:

```text
↻ rolls 14 ⇄ 15 integrated each other and none contains the others' latest work — on roll/15-0919-show-hotfixes run `rf integrate roll/14-0919-add-hotfix-to-menu`, then graduate roll/15-0919-show-hotfixes and it carries 14 to rolling
```

The member named is the one missing the fewest tips (the newest on a tie). One
`rf integrate` per missing tip makes it the carrier, and it then graduates
normally — no `--force` involved. [`status`](status.md) and `list` show the same
line under their tables, and mark the cycle's deps with `↻`.

Promoting a carried roll advances stable to the carrier's graduation merge —
where the carried roll actually landed on rolling — so `rf promote --roll` on
either member is one merge that lands both.

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

`--yes` takes option 1 without asking, when it is offered. An unattended run
without it takes option 2 — reports, prints the commands, and changes nothing,
so a graduation that lands in CI never leaves a repo mid-merge. A change made
on rolling directly rather than by a roll is reported as such, and option 1 is
not offered for it. Nor is it offered when a chain's dependency or carrier
step is what conflicted (above) rather than the roll named on the command
line — only that roll is ever checked out for `rf integrate` to merge into.

In the TUI, a conflicting `[G]` puts the same report in the output panel and
opens a modal with the same choices: `[i]` integrate (only when the roll is the
checked-out branch, since integrate merges into HEAD), `[m]` redo the merge on
rolling and leave it for `gg`, `[n]` close.

## Which branch the gates see

The TUI's `[G]` can graduate any eligible roll row regardless of which branch
is actually checked out — selecting a row doesn't check it out first. So
before the gates run, `rf graduate` checks the named roll out itself (rather
than running the configured gates against whatever happened to be checked out
already), stages the merge, and runs the gates against the staged result —
the content that will actually land on rolling, not the roll in isolation and
not whatever branch the TUI started from. Whichever branch was checked out
before the call is restored afterward, success or failure; for the CLI, which
already requires being on the roll, this is a no-op.

## Dev versions

Before anything else — including the merge-state checks, let alone the gates —
`rf graduate` confirms a dev marker the roll carries is actually its own: a
roll's `Cargo.toml` should only ever read `-roll<N>` for its own number (what
[`create`](create.md#dev-versions) and `rf verify` write). One that names a
different roll means the version history got mixed with another roll's
somewhere upstream (an `[i]` integrate merge, a stray cherry-pick), and
graduation refuses rather than silently papering over evidence of that. This
check is read-only, independent of the rest of this section, and runs first
because it is nearly free next to a full gate run.

Merging a roll that still carries its `-roll<N>` marker into rolling is
otherwise handled entirely at the git level, by a merge driver scoped to the
crate's own version — `Cargo.toml`'s `version` line and the matching entry in
`Cargo.lock` — not by `rf` stripping or committing anything on the roll branch
first. It is wired up per clone, by `rf init` and again before every merge `rf`
makes, and when git still stops on nothing but those lines `rf` finishes the
merge itself by the same rule (see [`init`](init.md) for both). The rule it
applies, whenever a merge needs to reconcile the
line: **keep whichever side is being merged *into*'s own marker — its own
value if it has one, otherwise the higher of the two numbers.** The same
driver, same rule, is what [`rf update`](update.md) relies on to keep a roll's
marker while raising its base number, and what `[i]`/`[I]` rely on when
integrating another roll or rolling into the current one — "ours" is whichever
branch is checked out, so the one driver and rule cover every direction.

Graduation is the one direction this generic rule alone cannot bootstrap,
though: the very first graduation ever, rolling has never worn `-dev` before,
so "keep ours's marker" would just carry `None` forward. `rf graduate`
therefore computes its own result rather than deferring to the driver — the
higher of the two sides' numbers, always marked `-dev` (or bare, if
`dev_versions` is off) regardless of what either side's marker was — and
forces it into the staged tree itself, the same way it already has to correct
the trivial "only the roll changed the line" case below. Once bootstrapped,
the driver's generic rule and `rf graduate`'s own computation agree, since
rolling's `ours` marker is `-dev` from then on.

A git merge driver only runs when both sides actually changed the line —
that's when a plain text merge would otherwise conflict, which is exactly the
case this exists for: rolling has moved since the roll branched (another
roll's version bump, say), so the roll's `-roll<N>` line and rolling's new one
land on the same spot and a plain merge would reject it. The far more common
case is the opposite — rolling's own version hasn't moved at all, so only the
roll changed the line — and git resolves *that* trivially by taking the
changed side, with no driver involved. `rf graduate` corrects this case itself,
inside the staged merge, before the gates run: it reads both sides' version
ahead of the merge and, after staging, rewrites `Cargo.toml` (and refreshes
`Cargo.lock`) to its own computed answer if the merge didn't already land
there on its own. Either way — driver-resolved or corrected in the staged
tree — nothing is committed to the roll branch itself, and nothing needs
rolling back: the merge either succeeds as one commit with the right version
already in it, or it's aborted entirely and the roll is untouched, exactly as
any other failed graduation.

`--dry-run` doesn't stage a merge at all, so the gates report on the roll's
branch exactly as it is — the honest answer for a run that changes nothing.

## Dev tag

Once the merge lands, `rf graduate` offers an annotated `v<X.Y.Z>-dev` tag on
rolling's new tip (`v0.2.6-dev`), the same idempotent shape `rf promote`'s own
release tag uses — an existing tag is left alone rather than failing. Skip
creating it with `--no-tag`, or disable the feature repo-wide with
`tag_on_graduate = false`. Pushing it to `origin` is then offered the same way
the release tag's push is, confirmed interactively or with `--yes`.
