# CLAUDE.md

Guidance for AI agents working in this repository.

## What this is

`roll-flow` (`rf`) is a Rust CLI+TUI tool that manages the roll-flow git workflow — a
structured multi-host NixOS dotfiles workflow. It is the successor to a monolithic
Nushell script (`scripts/roll-flow` in the dotfiles repo) and is packaged via gigpkgs.

The dotfiles repo is at `~/.dotfiles`. The gigpkgs repo is at `~/local_repos/gigpkgs`.

## Architecture

Pure Rust binary. No shell script intermediary layer.

- **CLI**: `clap` with subcommands
- **TUI**: `ratatui` for `rf status` and `rf list` (interactive drill-down)
- **Git operations**: `std::process::Command` shelling out to `git` binary
- **Nix operations**: `std::process::Command` shelling out to `nix` binary
- **Config**: `serde` + TOML at `~/.config/roll-flow/config.toml`

### Planned module layout

```
src/
  main.rs              entry point, CLI dispatch
  error.rs             error types (thiserror)
  cli/
    mod.rs             clap definitions, top-level dispatch
    init.rs
    start.rs
    integrate.rs
    graduate.rs
    promote.rs
    status.rs          → calls tui::status or prints table
    list.rs            → calls tui::list or prints table
    update.rs
  tui/
    mod.rs
    status.rs          ratatui full-screen status view
    list.rs            ratatui list with drill-down to roll detail
    widgets/           reusable ratatui components
  core/
    mod.rs
    config.rs          Config struct, auto-detection from flake.nix
    git.rs             low-level git subprocess wrappers
    branches.rs        branch listing, numbering, location (L/R/B/-)
    scope.rs           scope detection, file categorization
    verification.rs    verification checking (two sources)
    dependencies.rs    dependency resolution (four methods)
    quasi_rolls.rs     quasi-roll detection and analysis
```

## Domain model

### Branch tiers

| Branch | Purpose |
|--------|---------|
| `main` (stable) | Verified on ALL hosts |
| `rolling` (integration) | Tested on target hosts, may still need others |
| `roll/N-MMDD-theme` | Numbered work batches, graduated to rolling |
| `feature/*` | Individual features, integrated into rolls |

Branch names are configurable. Defaults: `rolling_branch = "rolling"`, `stable_branch = "main"`, `roll_prefix = "roll/"`.

### Roll lifecycle states

- **active** — branch exists, not merged to rolling
- **graduated** — merged to rolling (by merge commit or Graduate commit)
- **promoted** — merged to main, stable
- **diverged** — graduated but branch has commits after the merge point (needs re-graduation)
- **blocked** — has ungraduated dependencies that must graduate first

### Quasi-rolls

Direct-to-rolling commits that happen between roll merge points are grouped into virtual
"quasi-rolls" (q1, q2, ...) by `detect-quasi-rolls`. They appear in list/status views
and follow the same verification/promotion gating as real rolls. Auto-generated commits
(generation bumps, `gig@`, `Flake-Check:` commits) are filtered out.

### Config structure

```toml
repo_root = "/home/gig/.dotfiles"
rolling_branch = "rolling"
stable_branch = "main"
roll_prefix = "roll/"
username = "gig"
hosts = ["ganoslal", "merlin", "wsl"]

[host_active]
ganoslal = true
merlin = true
wsl = false
```

`host_active` is sourced from `vars/hosts.nix` in the dotfiles repo. Inactive hosts are
excluded from verification requirements (used when a machine is offline or being rebuilt).

Auto-generation reads `flake.nix` via `nix eval .#nixosConfigurations` and
`.#homeConfigurations` to discover hosts and username.

## Core algorithms

### Scope detection (`core/scope.rs`)

Diffs the roll branch against `main` and categorizes changed files:

```
hosts/**            → NixOS scope
home/**             → Home scope
flake.nix           → Flake scope
flake.lock          → Flake scope
scripts/**          → Flake scope
pkgs/**             → Flake scope
vars/**             → Flake scope
lib/**              → Flake scope
docs/**             → Docs scope
operations/**       → Docs scope
*.md                → Docs scope
```

For graduated rolls, diffs via the merge commit's two parents to get exactly what the
roll brought in (not what's accumulated since).

Docs-only rolls skip graduation requirement — they can promote directly.

### Verification (`core/verification.rs`)

Two sources, checked in order:

**Source 1 — structured test commit** on the roll branch:
```
test(roll/N-theme): flake=pass host=✓ ...

Flake-Check: pass
Host-Results:
  ganoslal: PASSED
  merlin: PASSED
Scope: NHF-
...
```
Written by `rf test-all`. If found, this is authoritative.

**Source 2 — rebuild commit detection** on the rolling branch after roll merge:
- NixOS: `^hostname[^:]*: generation \d+`
- Home: `^username@hostname[^:]*:`
- Flake: `Flake-Check: pass` in commit body

The `[^:]*` suffix pattern handles WSL variants like `wsl@merlins-windows`.

### Dependency detection (`core/dependencies.rs`)

Four methods, applied in order and deduplicated:

**Method 1 — explicit metadata**: reads `~/.config/roll-flow/rolls/N.toml` for a
`depends_on` array written at `rf start` time or manually edited.

**Method 2 — git ancestry**: if another roll's branch tip is an ancestor of this roll
(via `git merge-base --is-ancestor`), this roll's history contains those commits — a
hard git dependency. Only checks lower-numbered ungraduated rolls.

**Method 2b — merge subject parsing**: scans merge commit subjects for
`Merge branch 'roll/N-...'` patterns within this roll's history. Precise — avoids
false positives from shared ancestry.

**Method 3 — file overlap**: if rolls modify the same files and the other roll has a
lower number, it's a dependency. Uses `--first-parent --no-merges` on the other roll
to avoid false positives from cross-merges.

**Method 4 — transitive baseline** (`--full` mode, not `--basic`): lower-numbered
graduated rolls whose changes are in this roll's baseline. This is a promotion ordering
constraint (this roll can't go to main before those do), not a graduation blocker. The
`--basic` flag skips Method 4.

### Graduate/promote flow (`cli/graduate.rs`, `cli/promote.rs`)

Phases:
1. **Context** — detect current branch, determine mode (graduate vs promote)
2. **Candidates** — gather roll branches, filter to eligible (ungraduated/diverged for
   graduate; ready/verified for promote)
3. **Selection** — interactive numbered table, or `--all`, or explicit branch args
4. **Dependency resolution** — topological sort selected rolls with their deps
5. **Pre-merge checks** — uncommitted changes, dep graduation status, verification,
   divergence, flake check per roll
6. **Confirmation** — show merge plan, require y/N
7. **Merge execution** — `git merge --no-ff` with structured commit messages
8. **Post-merge** — offer branch deletion, reintegrate rolling onto main after promotion

The interactive selection table columns: `#`, `roll`, `loc` (L/R/B/-), `dev` (↑↓✓⚠=),
`blk` (🔒 if blocked), `scope` (NHFD flags), then per-host verification columns (✓⌛✗—).

### Merge commit message format

Graduation:
```
Graduate roll/N-theme into rolling

Scope: NHFD
Verified: ganoslal ✓  merlin ✓
```

Promotion:
```
Promote roll/N-theme to main

Verified-On: all-hosts
```

When a single promotion merge carries multiple graduated rolls, the subject is
`Promote <rolling> to <stable>` and the body lists the rolls it includes:
```
Promote rolling to main

Rolls:
  roll/1-0611-alpha
  roll/2-0612-beta
```

A roll counts as "promoted" if a `Promote roll/N-...` subject exists on the stable
branch, its graduation merge is reachable from stable, or it is named in a `Rolls:`
body of a Promote commit. All three sources must be checked wherever promotion is
tested.

## Build and test

```bash
cargo build
cargo test
cargo run -- status
cargo run -- list --no-tui
```

## Key invariants

- Never fast-forward merge. Always `--no-ff` for traceability.
- `main` only receives merges from `rolling`, never directly from roll branches.
- Roll numbers are monotonically increasing; detect from local + remote branches combined.
- A roll is "graduated" if a merge commit exists on the rolling branch whose subject
  matches `Merge branch 'roll/N-...'` OR `Graduate roll/N-...`. Both formats must be
  checked everywhere graduation is tested.
- Branch resolution always tries local first, then `origin/<branch>` as fallback.
  Functions that need the ref string should return `Option<String>` (null = doesn't exist).
- Active hosts only. Never require verification from inactive hosts.

## Integration with dotfiles

The dotfiles repo (`~/.dotfiles`) calls `rf` as a plain binary via the justfile:

```just
roll-start theme:
    rf start {{theme}}

roll-graduate:
    rf graduate

roll-promote:
    rf promote
```

The `rf` binary is provided by gigpkgs. The dotfiles repo does NOT contain roll-flow
source — it just consumes the package.

Config auto-detection reads the dotfiles repo's `flake.nix`, `vars/hosts.nix`, and git
branch structure. The `repo_root` in config always points to the current repo when `rf`
is invoked (detected via `git rev-parse --show-toplevel`).

## Shell environment

The developer uses Nushell. When testing manually, run commands as:

```nu
cargo run -- status
cargo run -- list
```

The binary itself has no Nushell dependency — it is a plain executable.
