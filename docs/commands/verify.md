# `verify`

```text
rf verify [--dry-run] [--bump <patch|minor|major>] [--yes]
```

Checks graduation/promotion readiness for the current branch. Reachable from the
TUI with `[v]`, which runs the same checks minus the bump offer — see
[`status`](status.md#verifying).

Routes checked:

- `roll/* -> rolling`
- `rolling -> main`

Validation includes:

- clean tree
- non-detached HEAD
- mergeability (common history, something new to merge; divergence is fine and
  only produces an informational note)
- the version gate on the `rolling -> main` route (see
  [Versioning and release tags](../../README.md#versioning-and-release-tags)).
  When a bump is needed, `verify` offers one; `--bump <level>` applies it without
  asking and `--yes` accepts the default (patch) non-interactively
- configured gate command execution
