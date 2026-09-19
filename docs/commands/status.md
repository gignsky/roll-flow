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

```text
[q] quit   [j/k ↑/↓] nav   [space] switch   [enter] detail   [r]efresh
[p] pull   [P] push   [f] fetch   [gg] lazygit   [esc] close output
[c]reate   [i]ntegrate   [G]raduate   [m] promote   [u]pdate   [b]ump
[d]elete   [x] prune   [t]idy   [PgUp/PgDn/End] scroll output
```

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
