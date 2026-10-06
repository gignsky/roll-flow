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

## Revert detection (`core/branches.rs`)

A roll's graduation (or promotion) merge is a normal commit — someone can
`git revert` it, and nothing about the merge itself prevents that. The
revert leaves the merge subject right where it was, so the plain "is there a
merge commit naming this branch" check that defines graduated/promoted still
says yes. Revert detection is the second check layered on top, deciding
whether that merge is *still in effect*.

**The signal.** `git revert` always writes `This reverts commit <hash>.` into
the new commit's body — not the subject, which survives even a conflicted
revert's hand-edited subject the same way `extract_graduated_branch`'s
leniency survives a hand-edited merge subject (the editor opens with the
boilerplate line already there). `find_active_revert_in_range` scans a commit
range for that line.

**Chained, not single-level.** A revert can itself be reverted (un-reverting
it), and that can be reverted again. `find_active_revert_in_range` walks the
range oldest-first and tracks a `(current_hash, reverted)` pair: whenever a
commit's body reverts `current_hash`, parity flips and `current_hash` becomes
*that* commit — because a later revert-of-the-revert references the revert's
own hash, not the original merge's. This is why `rf graduate`'s own remedy
(below) correctly stops being "needed" once applied, and why a roll a user
un-reverted by hand (no `rf` involved) reports `graduated` again too — both
are exercised in `tests/revert_detection.rs`.

**Graduation side — `RollState::Reverted`.** `check_reverted` /
`find_reverted_graduation` run `find_active_revert_in_range` over
`<graduation-commit>..<rolling>`. A hit means the roll is `⚠ diverged`'s
sibling: `⛔ blocked`'s cousin, actually — dependants treat a `Reverted`
dependency as still gating them (`dep_rows`/`dependent_rows`), the same as
`Active`/`Blocked`, since its content is not really on rolling.

The remedy is *not* re-running the ordinary merge. The roll branch's own tip
remains an ancestor of rolling either way — a revert adds a commit on top, it
removes nothing from history — so `classify_merge` sees
`MergeState::NothingToMerge` and a plain re-merge has nothing new to bring in
regardless of whether the roll gained commits since. The git-correct fix is
to revert the revert, which is what a reverted roll's "re-graduation" actually
is. `ops::graduate` detects this *before* `classify_merge` runs and routes to
`regraduate_reverted`, which runs the same `roll_to_rolling_gates` as an
ordinary graduation and then `git revert`s the revert commit instead of
merging. `rf graduate` / the TUI's `[g]` need no awareness of this — both
already call `ops::graduate` for every eligible state.

**Promotion side — `RollState::Demoted`.** Detect-only, deliberately. The
same remedy on stable would mean teaching the per-roll promotion pipeline
(version gate, release tags, carried-rolls disclosure — see
[Per-roll promotion](#per-roll-promotion)) a second kind of step, so
`check_promotion_reverted` only reports the state; nothing reverts the revert
automatically. It is also narrower in what it recognizes: `scan_promotion_commits`
only matches the single-roll `Promote <roll> to <stable>` shape
(`rf promote --roll`), not the multi-roll `Promote <rolling> to <stable>`
shape — a revert of a bundled promotion is not attributed to any one roll.

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
merge commit (skipping an existing tag rather than erroring); `rf graduate`
creates the equivalent `v<X.Y.Z>-dev` tag on rolling's new tip. Repos without a
`Cargo.toml` — including the dotfiles repo — get `VersionStatus::NotApplicable`
and skip all of it. Configurable via `version_gate`, `tag_on_promote`,
`tag_on_graduate`, `push_tag`, `dev_versions`.

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

## Dev markers and finalizing a release (`core/version.rs`, `core/merge_driver.rs`)

A `Semver` carries one of three `Marker`s: `Roll(n)` on a roll branch
(`0.2.4-roll9`), `Dev` on rolling (`0.2.4-dev`, its steady state once
`dev_versions` is on), or `None` on stable. `Marker`'s variants are declared in
that order on purpose — `Roll(_) < Dev < None` is a derived `Ord`, so a release
always outranks `-dev`, which always outranks any `-roll<N>` of the same
numbers; reordering the variants would silently flip the promotion gate's
sense. `Semver`'s own `Ord` is then also derived, comparing
`(major, minor, patch, marker)` in that field order, so the numbers dominate
the marker exactly as the gate needs.

The merge driver (`core::merge_driver::resolve`, wired up by `rf init` and
before every `rf` merge) states one rule for every direction the crate's own
version gets merged — `Cargo.toml`'s `version` line and the same value in
`Cargo.lock`'s own `[[package]]` entry alike: **keep `ours`'s marker; take the
higher of the two sides' numbers.** It is correct
as-is for `[i]`/`[I]`/`rf update` (a roll's own `-roll<N>` is already set by
`rf start`, so "ours" already carries the right marker going in). `ops::graduate`
does **not** use it, though — the very first graduation ever, rolling has never
worn `-dev`, so "keep ours's marker" would just carry `None` forward. Instead
`ops::graduate` computes its own answer — the higher of the two numbers,
unconditionally marked `-dev` (or bare, with `dev_versions` off) regardless of
what either side's marker was — and forces it into the staged tree itself
(`reconcile_staged_version`, the same fix that already exists for the far more
common "only the roll changed the line" trivial case the driver never even
sees). Once rolling has graduated once, its own `ours` marker *is* `-dev`, so
the driver's generic rule and `ops::graduate`'s own computation agree from then
on — the special-casing matters only for bootstrapping.

**Making a version-only conflict impossible.** A driver that only covers one
of the two files, or that is only configured in clones that happened to run
`rf init`, still lets a marker-vs-marker merge stop — which is exactly what
happened to `rf integrate` from one roll into another: the lockfile repeated
both markers, and the clone had never had the driver configured at all. Three
layers close that, each covering a gap in the one before:

1. **Both files.** `merge_driver::VersionFile` is `Manifest` or `Lockfile`;
   git passes `%P` so the driver knows which. In the lockfile only the
   `[[package]]` entry named after `Cargo.toml`'s `package.name` *with no
   `source` key* is touched (`version::replace_lock_package_version`) — a
   registry crate never has an empty source, and an ambiguous match counts as
   none. The driver doctors *all three* sides (ancestor too) to the resolved
   value before `git merge-file`, so the line is unchanged everywhere and
   cannot crowd an edit on a neighbouring line into a conflict; whatever still
   conflicts is real. Every version rewrite also syncs that lockfile entry
   directly (`ops::sync_lockfile_own_entry`, ahead of the best-effort `cargo
   update`), so the two files never drift apart when cargo cannot run.
2. **Wired whenever `rf` merges.** `ops::wire_version_merge_driver` writes the
   attribute lines to the clone's own `info/attributes` (via `git rev-parse
   --git-path`, shared by linked worktrees) and the driver command to local
   git config — both clone-local, neither in the tree. `run_merge`,
   `merge_gated` and `ops::integrate` call it quietly before merging, so every
   `rf` merge (integrate, `[i]`/`[I]`, update, graduate, promote, hotfix land)
   is covered on a clone that never ran `rf init`. `info/attributes` rather
   than a committed `.gitattributes` because the latter applies only on
   branches that carry it, and writing it lazily would leave an uncommitted
   file on whatever happened to be checked out.
3. **Settled in-process.** If git stops anyway — `rf` not on the `PATH` git
   sees, a read-only config — `ops::settle_version_only_conflicts` reads the
   index's conflict stages (`:1:`/`:2:`/`:3:`) for every unmerged path and
   runs the same `merge_driver::merge_texts`. It is all-or-nothing: any
   unmerged path that is not a version file, or that still conflicts with the
   version line agreed, and it touches nothing. `run_merge`/`ops::integrate`
   then commit (`--cleanup=strip`, to drop git's `# Conflicts:` comment);
   `merge_gated` just carries on to its gates, since a settled merge is
   indistinguishable from a clean `--no-commit` one — which is also why
   graduation's `reconcile_staged_version` still has the last word.

A dev marker of either kind must never reach stable, and is refused **before**
the numbers are compared, not by them: `0.2.5-roll9`/`0.2.5-dev` are
numerically above `0.2.4`, so a comparison alone would promote them — and then
tag them as-is. `VersionStatus::DevVersion` is this refusal. `rf verify` and a
non-final `rf promote` hit it directly; a *final* `rf promote` avoids it by
finalizing first (below), which is why `version::check_against` exists
alongside `check` — it takes an already-resolved head rather than reading the
branch fresh, so the gate can be run against the finalized value without
pretending the raw commit says something it doesn't.

**Finalizing.** `rf promote` asks "is this final?" before anything else
(`confirm_final_promotion` in `main.rs`; `--final` answers just this question,
`--yes` answers it along with everything else, an unattended run with neither
fails). Declining aborts the whole command. Confirming doesn't itself write
anything — it only permits `resolve_version_gate` to, which defers the actual
strip until it knows a promotion will proceed: the finalized value (rolling's
version with its marker dropped) is what the gate is evaluated against from the
start, but the `chore(release): finalize X.Y.Z for promotion` commit that
actually drops it is deferred until the function is past every early-return
that doesn't promote (Lower, an unresolvable bump), landing either at the
"already satisfied" return or immediately before the bump commit. A promotion
that ultimately fails (no bump resolved, no `--force`) therefore never leaves
rolling finalized with nothing to show for it, the same property `apply_version_bump`
already had for the bump itself.

A `--roll` step needs the same strip but has no branch to commit it on — it
merges a commit that already exists on rolling — so `run_promote_step`
generalizes the same staged-tree mechanism `in_merge_bump` already used for a
numeric bump: `raw_head` (the graduation commit's own `Cargo.toml`, still
`-dev`) and `finalized_head` (the same, marker dropped) are both read before
the gate runs; the gate itself runs against `finalized_head` via
`check_against`; and whatever must actually land — the bumped value if one was
computed, otherwise the finalized one if that differs from `raw_head` — is
written into the staged tree the same way a bump already was. A repo with
`dev_versions` off, or a graduation that was already bare, computes no write at
all and is byte-identical to before this existed.

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
