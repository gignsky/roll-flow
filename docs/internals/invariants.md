# Key invariants

Rules that must hold across the codebase. They are grouped by topic on purpose:
a new rule belongs in the section it concerns, not appended to the end of one
long list — that keeps two rolls adding rules in different areas from colliding.

## Merging and promotion

- Never fast-forward merge. Always `--no-ff` for traceability.
- `main` only receives merges from `rolling`, never directly from roll branches.
  Per-roll promotion does not weaken this: it merges a graduation *commit* that
  lives on rolling, which is why stable's history stays a prefix of rolling's
  rather than a divergent line.
- A roll is "graduated" if a merge commit exists on the rolling branch whose subject
  matches `Merge branch 'roll/N-...'` OR `Graduate roll/N-...`. Both formats must be
  checked everywhere graduation is tested.

## Rolls and branch resolution

- Roll numbers are monotonically increasing; detect from local + remote branches combined.
- Branch resolution always tries local first, then `origin/<branch>` as fallback.
  Functions that need the ref string should return `Option<String>` (null = doesn't exist).

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
  saw. Do not add a fourth.
- *Advancing* a ref on the remote happens in one place: the TUI's `[P]`, via
  `core::sync::run_push`. It is a different act from a delete and is governed by
  different rules, which is why it is a separate bullet rather than a fourth
  entry above:
  - It is always user-initiated on the branch under the cursor. Nothing pushes as
    a side effect of another op, and there is no `--all`.
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
- A version bump must be committed **before** the configured gates run, never
  after: it rewrites `Cargo.lock`, and `rolling_to_main_gates` contains
  `cargo update --workspace --locked`, which fails on a stale lockfile. This is
  why the bump is a CLI-level step in `main.rs` rather than part of `ops::promote`.

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
