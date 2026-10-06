# `status`

```text
rf status [--no-tui] [--no-deps] [--json]
```

Shows current branch, tier, cleanliness, pending rolls, and promotion readiness.
Runs as a full-screen TUI by default; `--no-tui` prints a plain table instead and
`--json` emits machine-readable output.

In a repo that has a `Cargo.toml`, the table carries a `version` column: the
`[package]` version as it stands at each branch's tip — its own tip for a branch
that exists locally, `origin/<branch>` for one that only exists on the remote.
Repos without a manifest, which is every dotfiles repo, get no column at all,
the same rule the header version follows. There is no flag for it.

The cell shows the full version, dev marker included: a roll carrying one reads
`0.2.4-roll9`, not `0.2.4`. The `#` column says which roll a row is, but not
whether that roll's dev marker has actually been applied yet — a roll created
before the marker existed, or with `--no-dev-version`, reads as a plain release
version until its first `rf verify` (see [`verify`](verify.md)) — so the version
cell is the only place that distinction is visible.

All versions are read in a single `git cat-file --batch`, so the column costs one
subprocess per reload rather than one per branch.

The table carries a `deps` column (roll numbers this roll integrated) and a
`dependants` column (roll numbers that integrated it) — `--no-deps` hides both.
They are shown whatever the roll's state, so a roll that has already graduated
still reports what it depends on and what depends on it. A dep number in the
table (TUI and plain alike) gets a trailing `⚠` when that dependency's branch
has moved since this roll integrated it — `26⚠` — so a stale copy is visible
without opening the detail view, which matters before reintegrating or merging
a batch of dependent rolls against a dependency that is still gaining commits.
`--json` carries the same signal as `stale_deps`, a subset of `deps`.

A trailing `↻` marks a dependency that integrated this roll back — a
**dependency cycle**, which is what two rows each `⛔ blocked` on the other
means. No order of separate graduations can satisfy a cycle, so it is resolved
by containment instead: the member whose tip contains every other member's tip
is the cycle's *carrier*, reads `active` (unless something outside the cycle
still blocks it), and graduating it lands the rest; the others stay `⛔ blocked`
waiting on it. Every table prints one line per cycle beneath it saying exactly
what to do, `--no-deps` or not:

```text
    2  roll/2-0919-add-hotfix-to-menu  L    ⛔ blocked    3⚠↻   3
>   3  roll/3-0919-show-hotfixes       L    active       2↻    2

  ↻ rolls 2 ⇄ 3 integrated each other: roll/3-0919-show-hotfixes contains the others' latest work — graduate roll/3-0919-show-hotfixes and it carries 2 to rolling
```

When no member contains the others yet — each moved on after integrating the
other — the line instead names the member to integrate the others into, and the
exact `rf integrate` to run on it; after that it is the carrier. `--json` gives
each member's row the same `cycle` object (`null` outside a cycle): `members`,
`carrier` (`null` when there is none yet), `suggested`, `lacks` (the tips
`suggested` is missing) and the `advice` line. See
[graduate](graduate.md#dependency-cycles).

Press `[enter]` on a roll for the detail overlay, which breaks the same two
relationships out with per-dependency markers, and the two are independent —
a dep can be both at once: `⛔ blocker` for a dep that has not graduated yet
(it gates this roll's graduation), and `⚠ reintegrate` for one whose branch
has moved since this roll integrated it (the same `⚠` as the plain table),
whatever its own state — a dependency does not need to have graduated and
diverged to be stale; it only needs to have kept moving after it was
integrated. A cycle carrier's fellow members read `↻ carried` rather than
`⛔ blocker`, and a roll in a cycle gets the same advice line as the tables at
the top of its dependencies. See
[divergence after integration](../internals/algorithms.md#dependency-detection-coredependenciesrs).

The TUI table pins the stable and rolling branches above the rolls, so `[space]`
switches to them the same way it switches to a roll. A base branch that exists
neither locally nor on `origin` is not listed.

## Keys

The status bar carries the handful you need before you know the rest exist:

```text
 [j/k ↑/↓] nav   [space] switch   [enter] detail   [?] keys   [q] quit
```

Everything else lives behind `?`, which opens a searchable list of every
binding. Type to filter, `↑`/`↓` to move, `enter` to *run* the key under the
cursor, `esc` to close:

```text
┌ keys ─────────────────────────────────────────────────────────────┐
│> br_                                                              │
│                                                                   │
│▶ t      tidy local roll branches, leaving origin alone  branches  │
│  p      pull the selected branch                        sync      │
│  P      push the selected branch                        sync      │
│  d      delete the selected branch                      branches  │
│  x      prune promoted roll branches, local and origin  branches  │
│                                                                   │
│type to filter   [↑/↓] move   [enter] run   [esc] close            │
└───────────────────────────────────────────────────────────────────┘
```

The search is fuzzy rather than a substring test, because the useful query is a
half-remembered word against a label you never read: matched characters score
higher when they run together (`prune`), when they land at the start of a word
(so `pb` finds "push branch"), and when they turn up early. Keys, label and
group are all searched, so `gg` finds lazygit by the key and `sync` lists the
whole group.

`enter` runs the binding by replaying its keystrokes through the ordinary key
handler — `gg` really does hand the terminal to lazygit — so there is one
dispatch path, not a second copy of the keymap that can drift.

The full list:

| key | does |
|---|---|
| `j`/`k`, `↑`/`↓` | move |
| `space` | switch to the selected branch |
| `enter` | roll detail: dependencies and divergence |
| `r` | reload |
| `?` | search every key |
| `q` | quit |
| `p` / `P` / `f` | pull / push / fetch the selected branch |
| `PP` | [push every branch that needs it](#pp--push-everything-that-needs-it) |
| `gg` | lazygit |
| `c` / `i` | create a roll / integrate one into the checked-out roll |
| `G` / `m` / `u` | graduate / promote / update from stable |
| `b` | bump the version |
| `d` / `x` / `t` | delete / prune / tidy branches |
| `esc` | close the output panel |
| `PgUp` / `PgDn` / `End` | scroll the panel, or follow new output |

The sync keys follow lazygit, which is why graduate is `[G]` and promote is `[m]`
rather than the `[g]`/`[p]` they used to be — `[p]` and `[P]` are pull and push
everywhere else, and matching that mattered more than keeping two letters.

A `›` marks the checked-out branch; the separate `▶` cursor marks the selection,
which is usually somewhere else. Both matter for `[i]`, the one key that reads two
rows: it merges the roll under the cursor into the roll wearing the chevron — see
[`integrate`](integrate.md#from-the-tui). The `sync` column reports each branch against
its upstream: `✓` in sync, `↑2` ahead, `↓1` behind, `↑2↓1` diverged, `gone` once
the upstream is deleted, and `—` when there is nothing to compare (no upstream,
or no local copy).

## Running commands

An action does not take the screen away. Its output streams into a panel in the
bottom-right corner while the table stays drawn and navigable — `j`/`k` and
`[enter]` keep working mid-push — and `[esc]` dismisses the panel once the
command has finished. `PageUp`/`PageDown` scroll its scrollback and `End` returns
to following new output. Only one command runs at a time; a second action while
one is in flight is refused rather than queued.

`gg` is the exception: lazygit is full-screen and owns the terminal, so `rf`
suspends, hands it over, and redraws when lazygit exits. Set `lazygit_command` in
`.roll-flow.toml` to point at something other than a bare `lazygit` on `PATH`.

## Pulling and pushing

`[p]` means different things depending on the row, because `git pull` only works
on the checked-out branch:

| selected branch | what runs |
|---|---|
| the checked-out one | `git pull`, plus the flag for `pull_mode` |
| local, not checked out | a fetch refspec that fast-forwards its ref without touching the worktree |
| remote-only | a fetch that updates `origin/<branch>` |
| no upstream | nothing — it says to press `[P]` first |

`pull_mode` defaults to `ff-only`, so a pull that would merge is refused rather
than quietly writing a merge commit into a roll branch. Set it to `merge` or
`rebase` in `.roll-flow.toml` to change that.

`[P]` pushes the selected branch, setting an upstream if it has none. If the
branch has diverged — either because the tracking ref already says so, or because
git refuses the push — a red confirmation states how many commits would be
overwritten and asks. Only `y` proceeds, and it uses `--force-with-lease`, so a
push someone else landed in the meantime is refused rather than clobbered. If git
reports a stale lease, fetch with `[f]` and try again.

### `PP` — push everything that needs it

Double-tapping `P` pushes every branch in the table that is ahead of its
upstream or has no upstream yet, in the order they are listed. A modal names
them first, and only `y` proceeds.

What it will **not** push is the point of the key:

| sync state | `PP` |
|---|---|
| `↑2` ahead | pushed — a fast-forward |
| `—` no upstream | pushed with `--set-upstream`; creates the branch on the remote |
| `✓` in sync | not mentioned; there is nothing to push |
| `↓1` behind, `↑2↓1` diverged | skipped, with the reason — pushing needs a force |
| `gone` | skipped — the upstream was deleted on purpose |

Every push `PP` performs is one `[P]` would have performed without asking
anything. Forcing stays a per-branch decision behind its own red confirmation,
so a bulk key can never be the thing that overwrites a remote ref; and a `gone`
upstream is left alone so `PP` cannot resurrect a branch that
[`prune`](prune.md) or [`clean`](clean.md) retired. Branches it skips are listed
in the modal *and* in the output panel, so they are not silently dropped.

Because `[P]` alone already pushes, the chord needs a timeout where `gg` does
not: a lone `P` is held about 400 ms to see whether a second one follows. Any
other key inside that window resolves it as the single push straight away — that
key is consumed rather than also acted on, so press it again once the push has
started.

## Verifying

`[v]` runs [`verify`](verify.md) on the **checked-out** branch — not the row
under the cursor, because the gates run in the working tree, so the branch they
judge is whichever one is checked out. The route comes from that branch's tier
and is named in the panel title before the gates start:

| checked out | `[v]` checks |
|---|---|
| a `roll/*` branch | `roll/N-… → rolling` |
| the rolling branch | `rolling → main` |
| anything else | nothing — it says to check out a roll or rolling first |

It is the one action key with no confirmation modal, because it is the one that
changes nothing: the modal is for the ops that write to the repo. It reports the
same checks as `rf verify`, in the same words — divergence note, version
comparison, gate notices, per-host results, verdict.

The one thing it will not do is bump the version. `rf verify` offers one; here a
failed version gate points at `[b]` instead, which is the key that already writes
that commit. A failed host or an unsatisfied gate marks the panel as failed
rather than passing quietly.

## Bumping the version

`[b]` raises the `[package]` version in `Cargo.toml` on the **checked-out**
branch — not the row under the cursor, because a bump is a commit and has to land
on the branch the merge will be made from. The bump modal shows the version it
will raise, so the effect is visible before confirming.

The header's top-right corner names the **binary that is running** — `rf v0.2.4`
— not the checked-out branch's manifest. The two used to be conflated, and the
corner changed on every `[space]`: in this repo it read as a roll's dev version,
in any other repo as whatever that repo ships, and neither answers "which rf is
this". Per-branch versions have their own table column.

```text
┌ roll-flow ───────────────────────────────────────── rf v0.2.4 ┐
│Branch: roll/4-0918-x   Rolling: rolling   Stable: main        │
└───────────────────────────────────────────────────────────────┘
```

```text
┌ bump version ────────────────────────┐
│Current: 0.2.0  on roll/1-0101-alpha  │
│                                      │
│[1] patch → 0.2.1                     │
│[2] minor → 0.3.0                     │
│[3] major → 1.0.0                     │
│                                      │
│[n] cancel                            │
└──────────────────────────────────────┘
```

The keys are digits rather than initials, unlike the delete modal's `l`/`r`/`b`.
"minor" and "major" both start with `m`, and separating two choices an order of
magnitude apart by the shift key alone is a mistake waiting to happen; the digits
also carry the ordering. Every row shows the version it would produce, so the
choice needs no semver arithmetic in your head.

It writes `Cargo.toml`, refreshes `Cargo.lock`, and commits both as
`chore(release): bump version to <X> for <branch>` — identical to what
`rf promote --bump` does, and the reason a clean working tree is required: the
commit stages its two files and then commits the index, so anything else staged
would be swept in.

In a repo with no `Cargo.toml` — the dotfiles repo, for instance — the header
shows no version and `[b]` says so rather than opening an empty picker. See
[Versioning and release tags](../../README.md#versioning-and-release-tags) for
what the version is actually gating.
