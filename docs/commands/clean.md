# `clean`

```text
rf clean [--dry-run] [--yes] [--force] [--with-remote] [--no-fetch]
```

The repo-wide branch janitor, and the one command that runs **without** a
`.roll-flow.toml` — it works in any git repository.

The case it exists for: one host creates and pushes `roll/101`, a *different*
host promotes it, and the branch is deleted on the remote. Back on the
originating host nothing notices. The local branch remains, and the cached
`refs/remotes/origin/roll/101` still claims the branch is live, so tools like
lazygit keep offering it in both their local and their remote views. `rf clean`
clears both halves.

It runs `git fetch --prune` against **every** configured remote (not just
`origin`, as [`prune`](prune.md) does), then deletes local branches in three
categories:

| Category | Meaning |
|---|---|
| `gone` | the upstream this branch tracked has been deleted |
| `merged` | fully contained in the base branch, with or without an upstream |
| `promoted` | a roll the config reports as promoted — needs `.roll-flow.toml` |

The base branch is resolved in order: the configured `stable_branch`; else the
remote's own `HEAD` (so a fresh clone works with no local base branch); else the
first of `main`/`master`/`develop`/`trunk` that exists; else the current branch.
With no base at all, merged detection is skipped rather than guessed at.

Every category is then gated on containment: the tip must be an ancestor of the
base branch or its remote-tracking counterpart. A deleted upstream is *not*
proof the local tip is contained, so this gate applies to `gone` branches too —
it is what stands between this command and silent data loss. Anything failing it
is listed as skipped with the reason and deleted only with `--force`, which
reports how many commits would be lost before the prompt.

Never deleted: the current branch, any branch checked out in another worktree
(named with its path), the stable and rolling branches, each remote's default
branch, and anything in `clean_protect`. `--force` does not override these —
they are not containment questions.

- `--dry-run` — show the plan, delete nothing. Remote-tracking refs are still
  refreshed, because without the prune nothing would ever *report* as gone and
  the preview would be misleading. `--no-fetch` opts out of that too
- `--yes` — skip the confirmation prompt. Run unattended without it, clean
  reports what it would do, deletes nothing, and exits 0
- `--force` — delete even when a branch holds commits the base branch lacks.
  This widens *what* may be deleted; it does not skip the prompt
- `--with-remote` — also delete the branches on their remote, using each
  branch's own upstream remote rather than assuming `origin`. Note this
  *extends* clean, whereas `rf prune --remote` *narrows* prune to the remote
  side only
- `--no-fetch` — skip the pruning fetch entirely and plan against cached refs

An unreachable remote is a warning, not a failure: clean carries on against the
last successful fetch's data, and the containment gate still guards every
deletion.
