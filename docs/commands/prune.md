# `prune`

```text
rf prune [--dry-run] [--local | --remote] [--yes] [--force] [--no-fetch]
```

Deletes roll branches that have already been promoted to the stable branch,
removing both the local branch and its copy on `origin`. Reachable from the TUI
with `[x]` — next to `[t]`, which is [`tidy`](tidy.md) and deletes the local copy
only.

Being promoted is not by itself treated as permission to delete. Promotion is
inferred from commit subjects on stable, which establishes that the roll landed
but not that its branch tip has nothing left on it — a roll can take commits
after its graduation merge. So each copy is additionally checked for containment:
the tip must be an ancestor of the stable branch or of `origin/<stable>`.
Anything failing that check is listed as skipped, with the reason, and is only
deleted with `--force`. The checked-out branch is never deleted locally.

Because `rf` is otherwise local-only, prune runs `git fetch --prune origin` first
so it never acts on stale remote-tracking refs (`--no-fetch` opts out).

- `--dry-run` — show the plan, delete nothing
- `--local` / `--remote` — narrow to one side (default: both)
- `--yes` — skip the confirmation prompt. Run unattended without it, prune
  reports what it would do, deletes nothing, and exits 0
- `--force` — also delete branches whose commits are not contained in stable.
  This widens *what* may be deleted; it does not skip the prompt

To delete a branch that has *not* been promoted, name it with
[`delete`](delete.md). To clear stale branches repo-wide, see [`clean`](clean.md).
To clear local branches without touching `origin` — including graduated rolls
prune will not consider — see [`tidy`](tidy.md), which judges safety by whether
the commits could be fetched back rather than by whether they reached stable.
