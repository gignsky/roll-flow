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
tested — which is why `check_promoted` defers to `scan_promoted` rather than
reimplementing them, and why any new promotion path must keep producing one of
those three shapes.

## Version gate and release tags (`core/version.rs`)

When the repo has a `Cargo.toml`, `rf verify` and `rf promote` require the
`[package]` version on the source branch to be strictly greater than the
target's, and `rf promote` creates an annotated `vX.Y.Z` tag on the promotion
merge commit (skipping an existing tag rather than erroring). Repos without a
`Cargo.toml` — including the dotfiles repo — get `VersionStatus::NotApplicable`
and skip all of it. Configurable via `version_gate`, `tag_on_promote`, `push_tag`.

The gate and the tag are per promotion *step*, not per invocation: a per-roll
promotion compares each roll's graduation commit against stable as that step is
reached, and tags each merge it makes. Only the whole-rolling route offers a
bump, because only it merges a branch a bump commit could land on — a per-roll
step merges a commit that already exists on rolling, so a short version there is
reported, not fixed.

## Per-roll promotion

`rf promote --roll <branch>` (and `[p]` on a roll row in the TUI) promotes one
graduated roll by advancing stable to **that roll's graduation merge on rolling**
— never by merging the roll branch into stable. The invariant in
[invariants.md](invariants.md) therefore still holds: the merge source is always a
commit on rolling. The consequence is that promoting a roll necessarily carries
whatever graduated ahead of it, so promotion order is graduation order and
dependencies are satisfied for free.

Each `--roll` is a separate merge behind a separate gate run; promoting the whole
rolling branch is a single merge behind a single gate run.

Promotion gates run against the **staged merge result**, not the pre-merge
worktree: `merge_gated` stages `git merge --no-ff --no-commit`, runs the gates,
and commits only if they pass. This is why a gate that rewrites tracked files
aborts the promotion — `git commit` would drop those changes and record a merge
whose content the gates never saw. Graduation still uses `run_merge`, which
merges and commits in one step.
