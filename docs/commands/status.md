# `status`

```text
rf status [--no-tui] [--no-deps] [--json]
```

Shows current branch, tier, cleanliness, pending rolls, and promotion readiness.
Runs as a full-screen TUI by default; `--no-tui` prints a plain table instead and
`--json` emits machine-readable output.

The table carries a `deps` column (roll numbers this roll integrated) and a
`dependants` column (roll numbers that integrated it) — `--no-deps` hides both.
They are shown whatever the roll's state, so a roll that has already graduated
still reports what it depends on and what depends on it. Press `[enter]` on a
roll for the detail overlay, which breaks the same two relationships out with
per-dependency blocker markers.

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
on the branch the merge will be made from. The header carries the current version
in its top-right corner as `v0.2.3`, so the effect is visible without opening
anything — and it sits in the border rather than on the branch line, which a long
roll name would otherwise crowd out.

```text
┌ roll-flow ──────────────────────────────────────────── v0.2.3 ┐
│Branch: roll/4-0918-x   Rolling: rolling   Stable: main        │
└───────────────────────────────────────────────────────────────┘
```

Repos with no `Cargo.toml` show nothing there, the same rule that makes `[b]`
refuse in them.

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
