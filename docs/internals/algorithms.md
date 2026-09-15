# Core algorithms

## Scope detection (`core/scope.rs`)

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

## Verification (`core/verification.rs`)

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

## Dependency detection (`core/dependencies.rs`)

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

## Graduate/promote flow (`cli/graduate.rs`, `cli/promote.rs`)

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

## Merge commit message format

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
