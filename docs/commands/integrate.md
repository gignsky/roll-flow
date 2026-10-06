# `integrate`

```text
rf integrate <branch>
```

Merges `<branch>` into the current roll branch with `--no-ff`, for work split
finer than one roll. Refuses unless a roll branch is checked out.

`<branch>` is usually a `feature/*` branch, but another **roll** is equally valid
and is what the TUI's `[i]` key does — see below. Either way the merge leaves a
`Merge branch '<branch>' into <roll>` subject in the roll's history, which is how
dependency detection picks it up (method 2b in
[algorithms](../internals/algorithms.md#dependency-detection-coredependenciesrs)).

## From the TUI

In `rf status` / `rf list`, `[i]` integrates the roll **under the cursor** into the
roll that is **checked out**. It is the only key whose source and destination are
different rows: the `›` chevron marks the destination and the `▶` cursor picks the
source.

```text
│ ▶   1  roll/1-0101-alpha    ← cursor: the roll being merged in
│   › 2  roll/2-0102-beta     ← chevron: the roll it merges into
└─────────────────────────────
     ┌ confirm ───────────────────────────────────────────┐
     │ Integrate roll/1-0101-alpha into roll/2-0102-beta? │
     │              [y] confirm    [n] cancel             │
     └────────────────────────────────────────────────────┘
```

It is refused, with the reason on the status line, when the checked-out branch is
not a roll, when the cursor is on the destination itself, when the cursor is on a
base branch (`[u]pdate` is the key for bringing stable into your rolls), or when
the selected roll exists only on `origin` — fetch it first with `[space]` or
`[p]`.

### `[I]` — integrate rolling

`[I]` needs no selection: it merges the rolling branch itself into the
checked-out roll, equivalent to `rf integrate <rolling_branch>`. Rolling already
contains every graduated roll, so the one merge picks all of them up as
dependencies too (method 2 in
[algorithms](../internals/algorithms.md#dependency-detection-coredependenciesrs)).

This is the recovery path when a roll fails to merge into rolling at graduation
time: rather than let `graduate` hit the conflict, press `[I]` to bring
rolling into the roll first, resolve the conflict here — same as any
other `[i]`/`[I]` conflict, in the panel or in lazygit (`gg`) — commit it, and
then graduate normally.

It is refused when the checked-out branch is not a roll, or when it already
*is* the rolling branch.

## After a conflicting graduation

This is also what [`graduate`](graduate.md#when-the-merge-conflicts) recommends
when its merge conflicts with a roll already on rolling: integrating that roll
reproduces the conflict on your branch, where you resolve and commit it, and
records the dependency the conflict revealed. `rf graduate --yes` does it for
you.

## Consequences

Integrating roll N into roll M makes **M depend on N**, which the dashboard shows
immediately: M's `deps` column gains N, N's `dependants` column gains M, and M
becomes `⛔ blocked` until N graduates. That is the intended ordering constraint —
M's history now contains N's commits, so N has to reach rolling first.

Unlike `graduate` and `promote`, this does not require a clean working tree: git
already refuses a merge that would clobber local changes and carries harmless ones
through. A conflict is a normal outcome — the merge stops, the panel shows git's
own `CONFLICT` lines, and you resolve it and commit, press `gg` for lazygit, or
run `git merge --abort`.

## Dev versions

Two rolls each wear their own `-roll<N>` marker (see
[`create`](create.md#dev-versions)), in `Cargo.toml` and in `Cargo.lock`'s
entry for the crate itself, so integrating one into the other touches the same
version line on both sides of both files. That is never a conflict: the
[version merge driver](init.md#the-version-merge-driver) keeps the checked-out
roll's own marker and takes the higher of the two numbers — integrating
`0.2.8-roll11` into `0.2.7-roll35` leaves `0.2.8-roll35`. `rf integrate` wires
the driver itself if this clone never ran `rf init`, and if git stops anyway
with only those lines in conflict it resolves them by the same rule and
commits the merge. Any other conflict — including a dependency that moved
differently in each roll's lockfile — stops the merge as described above.
