# `verify`

```text
rf verify [--dry-run]
```

Checks graduation/promotion readiness for the current branch:

- `roll/* -> rolling`
- `rolling -> main`

Validation includes:

- clean tree
- non-detached HEAD
- mergeability (common history, something new to merge; divergence is fine and
  only produces an informational note)
- configured gate command execution
