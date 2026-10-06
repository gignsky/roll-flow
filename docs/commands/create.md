# `create`

```text
rf create <slug> [--date MMDD] [--dry-run] [--no-dev-version]  (alias: rf start)
```

- Requires a clean working tree
- Creates `roll/N-MMDD-slug` off the stable branch (so the roll starts from a clean
  baseline; rolling and other rolls become dependencies only via `rf integrate`)
- Computes `N` as next highest roll number
- Supports `--dry-run`

## Dev versions

In a repo with a `Cargo.toml`, the new branch's version is marked with the roll
it belongs to — `0.2.4` becomes `0.2.4-roll9` — in a commit of its own, so the
checked-out version says which roll you are on. The table's `version` column
shows the numbers without the marker, since the `#` column already names the
roll.

The base numbers are deliberately left alone, and that is what makes the rest
work:

```text
main @ 0.2.4  ──rf start──▶     roll/9 @ 0.2.4-roll9
                ──rf graduate──▶  rolling @ 0.2.4-dev
                ──rf promote──▶   "is this final?" [y/N]
                                  N: cancelled, nothing touched
                                  y, unchanged: refused, demands a real bump
                ──rf promote --final --bump patch──▶  main @ 0.2.5
```

[`graduate`](graduate.md) swaps the roll's marker for rolling's own steady-state
`-dev`, same numbers, so a *final* [`promote`](promote.md) — once the marker is
stripped back to the version the roll branched from — reports `UNCHANGED` and
demands a real bump. Either marker can never reach stable: `rf verify` and a
non-final `rf promote` reject one outright rather than comparing it, because
`0.2.5-roll9`/`0.2.5-dev` *are* numerically above `0.2.4` and would otherwise
promote — and be tagged as-is.

Bumping on the roll branch keeps the marker (`0.2.4-roll9` → `0.2.5-roll9`),
raising the base that graduation carries onto rolling's `-dev`. That is the
other route to a promotable version, alongside bumping on rolling itself after
graduating.

Turn it off per repo with `dev_versions = false` in `.roll-flow.toml`, or per
invocation with `--no-dev-version`. Repos with no `Cargo.toml` — every dotfiles
repo — are unaffected either way, and no commit is invented to say so.
