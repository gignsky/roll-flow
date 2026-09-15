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
