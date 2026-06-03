# roll-flow

A git workflow manager for multi-host NixOS dotfiles — or any repo that benefits
from a structured, numbered branch flow with verification gating before
promotion to stable.

## What it does

`roll-flow` manages a three-tier branch model where work accumulates in numbered
"roll" branches, integrates to a rolling/develop branch for testing across
hosts, and only reaches `main` once fully verified.

```
main          ← stable, only verified changes
  └─ rolling  ← integration, tested on some hosts
       └─ roll/N-MMDD-theme  ← numbered work batches
```

Work flows **upward** — never directly to `main`. Each tier has gates.
