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
