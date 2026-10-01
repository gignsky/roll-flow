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

On a roll branch, `verify` also applies the roll's dev marker to `Cargo.toml` if
it is missing (`0.2.4` → `0.2.4-roll9`, see [`create`](create.md#dev-versions))
— so a roll created before the marker existed, or with `--no-dev-version`, is
brought in line by its first verify. Idempotent, skipped by `--dry-run`, and
off when `dev_versions = false`.

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
