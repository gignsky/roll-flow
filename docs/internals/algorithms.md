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
false positives from shared ancestry. This is the method that gives
[`rf integrate`](../commands/integrate.md) (and the TUI's `[i]`) its meaning:
merging roll N into roll M leaves exactly that subject in M's history, so M gains
a dependency on N and stays `⛔ blocked` until N graduates.

Subjects are read by `branches::extract_graduated_branch`, and it is deliberately
lenient, because the subject is not always the one git wrote. A merge that
conflicts opens an editor, and what comes back is whatever the user typed —
`merge branch 'roll/8-0918-help-menu'`, lowercase and shorn of its `into` clause,
is in this repo's own history. So matching ignores case, and a subject that still
begins with `merge` but fits none of the known shapes falls back to "the first
token containing a `/`".

Two rules keep that leniency from doing damage:

- **The ` into ` clause is cut before anything else looks at the subject.** It
  names the merge *target*, never the source. Without the cut,
  `Merge branch 'roll/8-x' into roll/7-y` could yield roll/7 — and on the rolling
  branch that reads as "roll/7 graduated", which is far worse than missing a
  dependency.
- **The fallback only fires on a subject that announces itself as a merge**, so
  `Revert "Merge branch 'roll/8-x'"` is not mistaken for a graduation.

The token it returns is not validated against the roll prefix, and does not need
to be: every caller either compares it to a real roll branch name or runs it
through `parse_roll_number`, so a candidate that is not a roll matches nothing.

One consequence worth knowing, since `[i]` makes roll-into-roll merges cheap: the
graduated scan below has a second pass *without* `--first-parent`, so once M
graduates, N's integrate merge is reachable from rolling and N reports as
graduated too. That is accurate — N's commits really are on rolling, carried in by
M — and the `⛔ blocked` gate is what keeps it from happening out of order. It is
only reachable at all via `rf graduate --force`.

Integrating the rolling branch itself (`[I]`, see
[integrate](../commands/integrate.md#i--integrate-rolling)) is the one case Method
2b cannot read off the subject: `git merge --no-ff <rolling>` leaves a subject
naming rolling, not a roll, so the scan above finds nothing even though the merge
just brought in every roll already graduated onto it. `branches::integration_deps`
handles it as a narrow, deliberate exception to "file overlap and broad ancestry
are not used": when one of the merges in range names the rolling branch, it falls
back to ancestry — checking each known graduated roll's graduation commit (from
the same `scan_graduated` pass `list_rolls` already did) against the roll's new
tip.

A plain ancestor check is not enough, and was the actual shape of a real bug: a
graduation from months ago is an ancestor of nearly every branch created after
it, because `rf promote` folds it into stable and every roll forks from stable.
Checking only "is this commit now an ancestor of the roll" therefore reported a
dependency on the repo's *entire* graduation history on every roll that had ever
done an `[I]` merge — exactly the explosion the opening paragraph says this
function avoids. The fix is to also require the commit be *absent* from
`base`'s ancestry (the same `base` the subject scan above is ranged over): a
graduation only counts if it is newly reachable in `base..roll`, i.e. actually
introduced by this merge, not merely inherited from stable before the roll
branched. A roll that shares history with another roll only through stable
gains no dependency from it.

**Staleness after integration.** `⛔ blocked` only covers a dependency that
has not graduated at all (`RollState::Active`/`Blocked`) — the ordering
constraint. That is a *state* question and answers nothing about whether N has
kept moving since M integrated it: N can gain commits on its own branch at any
point, blocked or not, graduated or not, and none of that is blocking M's
graduation (the ordering constraint only cares about N reaching rolling once).
So staleness is tracked as its own signal, `RollInfo::stale_deps`
(`branches::dep_tip_missing`): a direct `git merge-base --is-ancestor` check of
N's *current* tip against M, independent of `RollState` entirely. A dependency
that is still `Active` and simply kept gaining commits after `[i]` is exactly
as stale as one that graduated and then diverged — both fail the ancestry
check the same way, and `dep_tip_missing` does not care which.

This is why `is_blocker` and `needs_reintegration` (`tui::rolls::DepRow`) are
not mutually exclusive: a dependency can be both still-ungraduated *and*
stale, which is precisely the case that matters before merging a batch of
dependent rolls against a dependency someone keeps pushing to — each dependent
needs to say whether it has that dependency's latest work, not just whether
the dependency has graduated. The plain table surfaces the same signal with a
`⚠` suffix on the dep number (`branches::format_deps_with_staleness`), and
`rf list/status --json` carries it as `stale_deps`, so the check does not
require opening the detail view.

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

## Conflict diagnosis (`core/ops.rs`)

When a `--no-ff` merge in `run_merge` or `merge_gated` fails, the conflicted
paths are read (`git diff --name-only --diff-filter=U`) **before** `git merge
--abort` — afterwards there is nothing to read. Each path is then attributed by
walking the target's first-parent history since the merge base for that path
(`git log --first-parent <base>..<target> -- <path>`): every commit is a
culprit, and a merge whose subject names a branch (via
`branches::extract_graduated_branch`, the single reader of merge subjects) is a
culprit *roll*. A merge naming the source itself — its own earlier graduation,
on re-graduation — is dropped. The result is a typed `ops::MergeConflict`
error carrying a `ConflictReport`, raised only after the unwind so the repo is
clean by the time anyone acts on it; a merge that failed with no conflicted
paths keeps the plain error. Diagnosis never errors: it runs on a failing path
and must not hide the failure it explains.

### Predicting it in `rf verify`

`rf verify` finds the same conflict *before* any merge, with a trial merge in the
object store: `git merge-tree --write-tree --name-only --no-messages -z <target>
<source>` (`git::preview_merge`). Exit 0 is clean, 1 is conflicted, and anything
else — git older than 2.38, which lacks `--write-tree` — is reported as a
skipped check, never a failure, since the real merge still stops on a conflict.
Nothing is checked out, staged, or left behind but unreachable objects for `git
gc`, so it is safe on the tree verify is about to gate.

The paths go through the same attribution as above (`attribute_conflicts`, which
`diagnose_conflicts` also calls), so verify and graduate describe one conflict
identically. It runs only for a diverged pair — a fast-forwardable target cannot
conflict — and before the version gate and the configured gates, since a merge
that will stop makes their result moot. A real run fails with a typed
`ops::PredictedConflict`; `--dry-run` reports it as a warning. Verify prints the
fix (`rf integrate <culprit>` on a roll, else merging the target into the
source) but never runs it: it vouches for a merge, it does not start one.

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
reached, and tags each merge it makes. The two routes land a needed bump in
different places, because only one of them has a branch to put it on. The
whole-rolling route commits the bump on rolling *before* merging, so it rides in
with everything else. A per-roll step merges a commit that already exists on
rolling, so its bump is written into the **staged merge tree** ahead of the
gates and committed as part of the promotion merge (`run_promote_step`'s
`in_merge_bump`) — stable still only receives merge commits. Because that leaves
stable one commit ahead of rolling with a higher version, `ops::promote` then
merges stable back into rolling, exactly as a landed hotfix does; otherwise the
next whole-rolling promotion would read as LOWER.

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

Carrying earlier graduations is therefore structural, not a bug — but it is not
what "promote this roll" sounds like, so it is *disclosed*: `plan_roll_steps`
fills each step's `carried` list (via `fill_carried_rolls`), and
`ops::preview_roll_promotion` hands that plan to the CLI and the TUI so the
confirmation can state it before any merge happens. The baseline for a step is
the **previous step's merge source**, not stable's tip when the command started
— otherwise `--roll a --roll b` would report `a` as something `b` dragged along,
when `a` had its own step. `carried` is disclosure only: nothing decides what to
merge from it, so a git call that cannot answer omits a line rather than
changing the promotion.

Promotion gates run against the **staged merge result**, not the pre-merge
worktree: `merge_gated` stages `git merge --no-ff --no-commit`, runs the gates,
and commits only if they pass. This is why a gate that rewrites tracked files
aborts the promotion — `git commit` would drop those changes and record a merge
whose content the gates never saw. Graduation still uses `run_merge`, which
merges and commits in one step.
