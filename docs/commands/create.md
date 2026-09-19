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
main @ 0.2.4  ──rf start──▶   roll/9 @ 0.2.4-roll9
                ──rf graduate──▶  marker stripped, rolling @ 0.2.4
                ──rf promote──▶   refused: unchanged from main
                ──rf promote --bump patch──▶  main @ 0.2.5
```

[`graduate`](graduate.md) strips the marker back to exactly the version the roll
branched from, so the promotion gate then reports `UNCHANGED` and demands a real
bump. A `-roll<N>` version can never reach stable: [`verify`](verify.md) and
[`promote`](promote.md) reject one outright rather than comparing it, because
`0.2.5-roll9` *is* numerically above `0.2.4` and would otherwise promote — and
be tagged `v0.2.5-roll9`.

Bumping on the roll branch keeps the marker (`0.2.4-roll9` → `0.2.5-roll9`),
raising the base that graduation strips back to. That is the other route to a
promotable version.

Turn it off per repo with `dev_versions = false` in `.roll-flow.toml`, or per
invocation with `--no-dev-version`. Repos with no `Cargo.toml` — every dotfiles
repo — are unaffected either way, and no commit is invented to say so.
