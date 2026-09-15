# `delete`

```text
rf delete <branch> [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
```

Deletes one named roll branch — locally, on `origin`, or both. Reachable from the
TUI with `[d]` on the selected row, and accepts the same flags as
[`prune`](prune.md).

It relaxes exactly one of prune's rules: the roll need not be promoted. Prune
picks its own targets by inferring promotion from commit subjects, so it must
refuse on doubt; `delete` acts on a branch the user named, which makes an
abandoned or superseded roll a legitimate target. Everything else still holds —
an uncontained copy needs `--force`, the checked-out branch is never deleted
locally, and `main`/`rolling` are refused outright.

In the TUI the prompt follows where the branch actually lives:

- **local-only or remote-only** — a `[y]`/`[n]` confirmation defaulting to no.
  Nothing happens without a deliberate `y`; Enter is not a shortcut for it
- **both** — `[l]` local only, `[r]` origin only, `[b]` both, `[n]` neither

If the copies you chose hold commits the stable branch lacks, that keypress does
*not* delete. It swaps the modal for a second one naming how many commits would
be lost, which needs a fresh `y` — and that second `y` is the only thing that
ever forces.
