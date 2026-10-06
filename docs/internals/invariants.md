# Key invariants

Rules that must hold across the codebase. They are grouped by topic on purpose:
a new rule belongs in the section it concerns, not appended to the end of one
long list — that keeps two rolls adding rules in different areas from colliding.

## Merging and promotion

- Never fast-forward merge. Always `--no-ff` for traceability.
- `main` only receives merges from `rolling`, never directly from roll branches.
  Per-roll promotion does not weaken this: it merges a graduation *commit* that
  lives on rolling, which is why stable's history stays a prefix of rolling's
  rather than a divergent line. When a per-roll step has to bump the version
  (or finalize rolling's `-dev` marker), that goes *inside* that merge commit,
  never as a commit beside it — and stable is then merged back into rolling so
  the tiers do not silently diverge (the same reintegration a landed hotfix
  performs).
- A promotion is *final* or it doesn't happen — there is no in-between state on
  stable. `rf promote` asks before touching anything (`confirm_final_promotion`),
  and declining lands nothing at all; there is no "promote but keep the `-dev`
  marker" path. See "Versioning and release tags" below for what finalizing
  actually does.
- A per-roll promotion that would land rolls the user did not name must say
  which, and get an answer, *before* it merges — `rf promote --roll` prompts
  (`--yes` accepts, unattended fails) and the TUI's `[m]` lists them in its
  confirmation. Carrying earlier graduations is inherent to advancing stable to a
  graduation commit; landing them silently is not, and any new promotion entry
  point inherits the disclosure, not just the merge.
- A roll is "graduated" if a merge commit exists on the rolling branch whose subject
  names it as the merge *source* — `Merge branch 'roll/N-...'`, `Graduate roll/N-...`,
  a GitHub `Merge pull request` subject, or a hand-written one a conflicted merge
  left behind. Every shape must be checked everywhere graduation is tested, which
  is why there is exactly one reader of merge subjects,
  `branches::extract_graduated_branch`, and why new callers must go through it
  rather than matching a prefix themselves. A single missed shape costs the roll
  its dependency, its graduated state *and* its graduation commit at once, since
  all three are read from that one function.
- **Source, never target.** A subject's ` into ` clause names where the merge
  landed, and must never be read as a branch that graduated. Misreading it marks
  an unmerged roll as graduated — strictly worse than failing to notice a real
  one, which is why the clause is cut before any shape is matched.
- **A revert cannot be undone by re-merging.** Once a graduation (or
  promotion) merge is reverted, the roll branch's own tip is still an
  ancestor of the target either way — a revert adds a commit on top, it
  removes nothing from history — so an ordinary `--no-ff` merge of the roll
  has nothing new to bring in, regardless of whether the roll has gained
  commits since. The only way to restore it is to revert the revert, which is
  what `rf graduate`'s remedy for `RollState::Reverted` actually does (see
  [Revert detection](algorithms.md#revert-detection-corebranchesrs)). Any new
  code path that tries to "fix" a reverted roll by merging the branch again
  will silently no-op, or hit `classify_merge`'s `NothingToMerge` and bail
  with a misleading "already up to date".

## Rolls and branch resolution

- Roll numbers are monotonically increasing; detect from local + remote branches combined.
- Branch resolution always tries local first, then `origin/<branch>` as fallback.
  Functions that need the ref string should return `Option<String>` (null = doesn't exist).
- A dependency that has not graduated (`RollState::Active`/`Blocked`) is a real
  `⛔ blocker` — the ordering constraint. Whether it has also *moved* since the
  dependent integrated it is a separate question, answered by an ancestry check
  (`RollInfo::stale_deps`), not by `RollState`: an `Active` dependency that
  keeps gaining commits is exactly as stale as a `Diverged` one that graduated
  and then moved, and either must be surfaced as needing reintegration
  (`⚠ reintegrate` in `tui::rolls::dep_rows`/`dependent_rows`, the same `⚠` in
  the plain table and `stale_deps` in `--json`). The two signals are not
  mutually exclusive and must not be conflated into one marker: a dependency
  can be both an active blocker and stale at once, and collapsing that to
  "blocked" alone hides the staleness, while gating the reintegration notice on
  `RollState::Diverged` alone misses every dependency that is stale while still
  active — exactly the case that matters before merging a batch of dependent
  rolls against a dependency someone keeps pushing to.

## Verification

- Active hosts only. Never require verification from inactive hosts.

## Deletion safety

- Deleting a branch requires more than promoted state. Promotion is read off commit
  subjects on stable, which does not prove a given branch tip is fully contained —
  so `rf prune` additionally requires the tip to be an ancestor of the stable branch
  or of `origin/<stable>`, and `--force` is the only override. Never delete the
  checked-out branch — that guard sits ahead of the force check and is not an
  override target.
- `rf delete <branch>` and the TUI's `[d]` are the one place a *non-promoted* branch
  may be deleted, because the user named it explicitly rather than the tool inferring
  it from commit subjects. Every other rule is unchanged: an uncontained copy still
  needs `force`, and in the TUI that means a second, separate confirmation stating how
  many commits would be lost. That confirmation is the only thing that sets `force`.
- What counts as "safe to delete" is a *policy*, `ops::Containment`, not a second
  code path. `rf prune` and `rf delete` carry `Stable`; `rf tidy` carries
  `Recoverable`, which also accepts the rolling branch and the branch's own
  `origin/<branch>` copy — because tidy deletes only the local copy, so commits
  still on the remote can be fetched back. `decide_copies` takes the *answers*,
  never the policy: which refs were consulted is `local_survives`'s business, and
  keeping that split is what lets one safety table serve all three commands. A new
  command that deletes branches adds a policy, not a path.
- `Recoverable` must always fetch before planning, even though it deletes nothing
  on the remote. It reads `origin/<branch>` as proof the commits survive, and a
  stale ref there names a branch that may already be deleted upstream — certifying
  as recoverable exactly the branches whose only remaining copy is the local one.
  `Stable` does not need this: a stale `origin/<stable>` is only ever *older*, so
  it under-reports containment and errs toward keeping branches.
- A local copy checked out in *any* worktree is never deleted — the current branch
  and another worktree are two separate refusals with two separate messages, and
  both sit ahead of the force check. git refuses either delete regardless; the
  guards exist so the user is told which worktree to clear rather than shown git's
  error once per branch.
- All *roll branch* deletion goes through `ops::decide_copies` → `plan_branch_deletion`
  → `prune_apply`. There is deliberately one implementation of those safety rules and
  one path to the remote; do not add a second way to delete a roll branch. `rf clean`
  is the deliberate exception — it answers a different question (stale branches in any
  repo, roll or not) and has its own `core::clean::plan` → `core::clean::apply` pair,
  which applies the same containment gate.

## Writing to the remote

- `rf promote` may push a release tag, but only after an explicit y/N
  confirmation (or `--yes`), and only when `push_tag` is enabled.
- Deleting a ref on the remote (`git push --delete`) happens in exactly three
  places: `rf prune`, `rf delete` (including the TUI's `[d]`), and
  `rf clean --with-remote`. All of them `git fetch --prune` before planning a
  remote delete, so containment is never judged against a stale ref — a stale one
  names an old tip, and deleting against it would destroy commits the check never
  saw. Do not add a fourth. `rf tidy` in particular is not one and must never
  become one: its whole safety argument is that a branch still on `origin` is
  recoverable, which stops being true the moment it can delete the remote copy.
  The TUI's `[t]` is the same command and inherits the same rule — it builds its
  scope with `PruneScope::tidy`, whose `remote: false` is the thing that keeps it
  out of this list.
- *Advancing* a ref on the remote happens through one function,
  `core::sync::run_push`, reached from the TUI's `[P]` and `PP`. It is a
  different act from a delete and is governed by different rules, which is why it
  is a separate bullet rather than a fourth entry above:
  - It is always user-initiated. Nothing pushes as a side effect of another op.
  - `PP` is the one bulk form, and it exists only because it cannot do anything
    `[P]` would have stopped to ask about. `tui::rolls::plan_push_all` queues a
    branch **only** when it is ahead of its upstream or has none — a
    fast-forward, or a creation. Behind/diverged branches are skipped, so the
    bulk key can never be what forces a push; a `gone` upstream is skipped too,
    so it can never resurrect a branch `rf prune` or `rf clean --with-remote`
    deleted on purpose. Both halves are reported, never silently dropped. A wider
    `PP` is not a feature request — it is the thing this rule forbids.
  - A non-fast-forward push is never forced silently. Either the tracking state
    already shows the branch is behind, or git refuses and
    `core::sync::is_rejection` recognises the refusal; either way the user answers
    an explicit y/N first, and only `y` sets `force`.
  - Forcing uses `--force-with-lease` and nothing else. If git reports `stale
    info` the push is *refused*, not retried with `--force`: a stale lease means
    we do not know what is on the remote, and the honest response is to fetch and
    look. This is a deliberate divergence from lazygit, which does fall back.
  - `rf` still never syncs on its own. Every fetch and push is a keypress.
- Sync commands run with `GIT_TERMINAL_PROMPT=0`. Their output is piped into the
  TUI's panel, so nobody is reading the terminal on git's behalf — without this a
  credential prompt blocks forever on a pipe instead of failing.

## Versioning and release tags

- The version gate and release tagging mirror the CI workflows exactly —
  `version-bump-check.yml`, `tag-on-main.yml`, `release-check.yml`. When changing
  one side, change the other. `src/core/version.rs` records which rule mirrors
  which workflow.
- Any version *rewrite* must be committed **before** the configured gates run,
  never after: it rewrites `Cargo.lock`, and `rolling_to_main_gates`/
  `roll_to_rolling_gates` contain `cargo update --workspace --locked`, which
  fails on a stale lockfile. This is why the bump is a CLI-level step in
  `main.rs` rather than part of `ops::promote` — a per-roll step has to land it
  *inside* the merge, which only `ops::promote` can do (see
  [algorithms.md](algorithms.md)'s "Graduate/promote flow" section). All
  version writes go through `ops::commit_version_change`, which is the one
  place the write, the lockfile refresh and the commit happen in that order.
- `ops::graduate` checks **before** anything else, including the merge-state
  classification and the gates, that a dev marker the roll carries is its own
  (`ops::check_dev_marker_ownership`): a roll should only ever wear its own
  `-roll<N>`, and one naming a different roll means the version history was
  mixed with another roll's somewhere upstream (an `[i]` integrate merge, a
  stray cherry-pick). Read-only and cheap, so it runs first — a data-integrity
  check, independent of the merge-conflict question below.
- `ops::graduate` checks out `roll` itself before doing anything that mutates
  state or runs a gate, rather than operating on whatever happened to be
  checked out when it was called. The TUI's `[G]` can graduate any selected
  row regardless of which branch is currently checked out (`validate_action`
  only checks the roll's state, not whether it's the current branch), and the
  gates run real shell commands against the working tree — so without this,
  they would silently validate the wrong tree. `merge_gated` then stages the
  actual merge and runs the gates against *that* result, not `roll` in
  isolation, which is the content that will actually land. The original
  checked-out branch is restored at the very end, success or failure; for the
  CLI (which already requires being on the roll) this is a no-op, so the
  behavior there is unchanged.
- A roll's `-roll<N>` marker reaching `rolling` (via `rf graduate`), rolling's
  own `-dev` marker reaching `stable` (via a *final* `rf promote`), or either
  getting stale against an advanced target (via `rf update`/`[i]`/`[I]`) is
  resolved at the git level, not by `rf` committing a strip/reapply on either
  branch first. See `src/core/merge_driver.rs` and
  `ops::ensure_version_merge_driver` (wired up by `rf init` and before every
  `rf` merge): `Cargo.toml` and `Cargo.lock` are attributed to a custom merge
  driver that resolves the crate's own version line by
  keeping whichever side is being merged *into*'s own marker and the higher of
  the two numbers — "ours" is just whichever branch git has checked out,
  decided by git itself rather than by which `rf` command is running. This
  replaced an earlier design (strip the marker in a commit before merging,
  roll it back if the gates or the merge failed afterward) that worked but
  needed careful transactionality to avoid leaving a roll half-graduated with
  its marker stripped and nothing to show for it; the merge driver needs none
  of that, because nothing is committed to either branch until the merge
  itself commits. The driver's generic rule is correct as-is for `[i]`/`[I]`/
  `rf update`, where "ours" (a roll) already carries its own right marker from
  `rf start` — but not for graduation, see below.
- **A version-only conflict never stops an `rf` merge.** The crate's own
  version lives in two files that change in lockstep — `Cargo.toml` and
  `Cargo.lock`'s own `[[package]]` entry — so a fix for one is no fix: both are
  attributed to the driver, and every version rewrite syncs the lockfile entry
  itself (`ops::sync_lockfile_own_entry`) rather than trusting a `cargo update`
  that may not run. The driver is clone-local configuration, so `rf init`
  cannot be the only thing that sets it: every `rf` merge path (`run_merge`,
  `merge_gated`, `ops::integrate`) wires it first, into the clone's
  `info/attributes` rather than the tree. And because git can still fail to
  run it, those same paths finish a merge that stopped on *nothing but* those
  lines by the same rule (`ops::settle_version_only_conflicts`). Any new merge
  path must go through one of the three, not call `git merge` itself.
- **Only the version line, and only all-or-nothing.** The driver and the
  in-process fallback touch the crate's own entry and nothing else — never a
  dependency's lockfile entry, never a `[[package]]` with a `source`, never an
  ambiguous match. If any unmerged path is something else, or a version file
  still conflicts once its version line agrees, the merge is left exactly as
  git left it: finishing the parts `rf` understands would make the real
  conflict look smaller than it is.
- `ops::graduate` does not delegate to the merge driver's generic rule, because
  "keep ours's marker" is wrong on the very first graduation ever: rolling has
  never worn `-dev` before that point, so "keep ours's marker" would just carry
  `None` forward forever. Graduation instead *forces* its own result —
  unconditionally `-dev` (or bare, with `dev_versions` off) at the higher of
  the two sides' numbers, regardless of what either side's marker was — the
  same answer the generic rule converges on anyway once rolling has graduated
  once and its own `-dev` is already in place, but correct from the very first
  graduation too.
- A git merge driver only fires when *both* sides changed the attributed line
  — the case that would otherwise conflict. The far more common graduate case
  is the opposite: rolling's own version is untouched, so only the roll
  changed the line, and git resolves that *trivially* by taking the changed
  side (the roll's marked value) without ever invoking any driver. `ops::graduate`
  corrects this itself, inside `merge_gated`'s staged tree, before the gates
  run (`reconcile_staged_version`): it reads both sides' version ahead of the
  merge and rewrites `Cargo.toml` (and syncs `Cargo.lock`'s own entry) to its
  own computed answer (above) if the merge didn't already land there. This is
  why the fix exists in `ops::graduate` and not purely in the driver — the
  driver alone covers the conflicting case but not the trivial one, and both
  have to resolve the same way for the marker to never reach rolling wrong. `rf update`'s direction doesn't need
  this: its trivial case (stable hasn't touched the version, only the roll's
  own marker has) already resolves correctly by git's own default of keeping
  the side that changed, since that side is the roll.
- A dev marker of either kind — a roll's `-roll<N>` or rolling's own `-dev` —
  must never reach the stable branch, and is refused **before** the numbers
  are compared, not by them. `0.2.5-roll9`/`0.2.5-dev` are numerically above
  `0.2.4`, so a comparison alone would promote them — and then tag them as-is.
  Both sides state the rule separately for the same reason: `sort -V` ranks
  either suffix *above* the bare version, and so would a derived `Ord` on the
  numbers alone, which is why `Marker`'s declaration order (`Roll(_)`, `Dev`,
  `None`) is what `Semver`'s derived `Ord` actually depends on. A non-final
  `rf promote`/`rf verify` hits this directly (`VersionStatus::DevVersion`); a
  *final* `rf promote` avoids it by finalizing (stripping the marker) before
  the gate runs at all — see "Finalizing" below.
- The dev marker is a `Marker` — a roll *number*, rolling's `-dev`, or none —
  not a free-form pre-release string. It keeps `Semver` `Copy`, and any other
  suffix still fails to parse rather than being silently dropped — dropping one
  could let a lower version read as higher.
- **Finalizing is a gate, not a silent side effect.** `rf promote` asks "is
  this final?" before touching anything (`confirm_final_promotion`); declining
  aborts the whole command. `--final` answers just this question — `--yes`
  also answers it, along with the bump and tag-push prompts, which is why
  `--final` exists as its own flag (the same relationship `--bump <level>`
  already has with `--yes` for the bump prompt specifically): a scenario like
  "finalize non-interactively but still let the tag-push prompt behave
  unattended" needs the two kept separate. Once confirmed, the actual
  `chore(release): finalize X.Y.Z for promotion` commit is deferred until
  `resolve_version_gate` is past every early-return that doesn't promote
  (`Lower`, an unresolvable bump) — the same reason the bump commit itself is
  deferred — so a promotion that ultimately fails never leaves rolling
  finalized with nothing landed. A `--roll` step has no branch to commit this
  on, so it lands in the **staged merge tree** instead, the same mechanism
  (and the same spot) `in_merge_bump` already used for a numeric bump — see
  [algorithms.md](algorithms.md#dev-markers-and-finalizing-a-release-coreversionrs-coremerge_driverrs).

## `rf clean`

- `rf clean` is the only command that runs without a config and across all remotes.
  It resolves the repo root itself and treats `Config` as optional — note that
  `Config::load` resolves the repo root *first*, so `.ok()`-ing it wholesale would
  swallow "not a git repository" too. Find the repo first, then soften the config.
- Detecting a gone upstream must happen *after* the pruning fetch.
  `%(upstream:track)` reports `gone` from the absence of a remote-tracking ref,
  which a stale cache still supplies — detect first and nothing ever reports gone.
- A deleted upstream is not proof a branch tip is contained. `rf clean` applies the
  same containment gate to gone branches as to promoted ones; `--force` is the only
  override, and it never overrides the checked-out/worktree/protected guards.
