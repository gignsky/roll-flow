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
## CLI commands

```
rf init                         Initialize roll-flow in a repo
rf start <theme>                Create a new numbered roll branch
rf integrate <branch>           Merge a feature into the current roll
rf graduate [rolls...]          Merge rolls into rolling (interactive)
rf promote [rolls...]           Merge rolling into main (interactive)
rf status                       Show fleet/roll status (TUI or table)
rf list                         List all rolls with state (TUI or table)
rf update                       Merge main into all local ungraduated rolls
rf test-all                     Run builds on all configured hosts
```

## TUI mode

`rf status` and `rf list` open a full-screen TUI when running in an interactive terminal.
Use arrow keys to navigate, Enter to drill into a roll's detail view (scope, verification
per host, dependencies, changed files). `q` or `Esc` to exit.

Pass `--no-tui` to force table output (useful for scripting or piping).

## Roll lifecycle

```
active      branch exists, not merged to rolling
graduated   merged to rolling, awaiting verification on all hosts
promoted    merged to main, stable
diverged    graduated but branch has new commits since merge (needs re-graduate)
```

Quasi-rolls are direct-to-rolling commits between roll merge points — they appear in
`rf list` and `rf status` and follow the same verification/promotion gating.

## Scope detection

roll-flow automatically detects what a roll affects by diffing against `main`:

| Scope | Triggers on |
|-------|-------------|
| NixOS | `hosts/**` |
| Home  | `home/**` |
| Flake | `flake.nix`, `flake.lock`, `scripts/`, `pkgs/`, `vars/`, `lib/` |
| Docs  | `docs/`, `operations/`, `*.md` |

Scope determines what verification is required before promotion.

## Verification

A roll is considered verified on a host when evidence exists in the git log:

1. **Structured test commit** — `test(roll/N-theme): ...` commit with body sections
   `Host-Results:`, `Flake-Check:`, etc. (written by `rf test-all`)
2. **Rebuild commits** — generation commits (`hostname: generation N`) and home-manager
   switch commits (`user@hostname: ...`) found on the rolling branch after the roll merged

## Configuration

Config is stored at `~/.config/roll-flow/config.toml` and auto-generated on first run
by inspecting the repo (reading `flake.nix` for hosts/username, detecting branch names).

Re-run `rf init` with flags to override auto-detection:

```
rf init --rolling-branch develop --stable-branch main --username gig --hosts ganoslal,merlin,wsl
```

## Installation

Packaged in [gigpkgs](https://github.com/maxwellrupp/gigpkgs). Add to your home config:

```nix
home.packages = [ pkgs.roll-flow ];
```

Or run directly:

```
nix run github:maxwellrupp/gigpkgs#roll-flow
```

## Architecture

Pure Rust binary. TUI via `ratatui`, CLI via `clap`. Git operations via subprocess calls
to `git` (no libgit2 binding required). No shell script intermediary — the binary calls
`git` and `nix` directly the same way a shell script would, just with typed output
parsing.

See `CLAUDE.md` for full architecture notes, module layout, and domain model documentation.
