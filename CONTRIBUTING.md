# Contributing to roll-flow

roll-flow (`rf`) manages its own development using the same roll-flow model it
implements. See [README.md](README.md) for command reference and
[CLAUDE.md](CLAUDE.md) for the full domain model — this document only covers
how that model applies to *this* repository.

## Branch model for this repo

```text
main      <- stable, only advanced by promotion merges
  └─ develop   <- integration branch (this repo's "rolling" branch)
       └─ roll/N-MMDD-slug  <- numbered work branches
```

This repo's `.roll-flow.toml` sets `rolling_branch = "develop"` and
`stable_branch = "main"` — `develop` plays the role that `README.md`/`CLAUDE.md`
generically call "rolling."

## Day-to-day workflow

1. Start work with `rf create <slug>` (alias `rf start`), which branches
   `roll/N-MMDD-slug` off `main` — the *stable* branch, so every roll starts
   from a clean baseline. `develop` and other rolls become dependencies only
   when you pull them in deliberately with `rf integrate`.
2. Do the work, integrating any feature branches with `rf integrate <branch>`
   if you split work further.
3. When ready, run `rf verify` to check the branch is mergeable and run the
   configured gates without merging.
4. Run `rf graduate` from the roll branch to merge it into `develop` with a
   structured `--no-ff` commit. Open your PR against `develop`.
5. Periodically, a maintainer runs `rf promote` from `develop` to merge into
   `main` once enough graduated rolls are ready for a stable release.
6. Once rolls have reached `main`, `rf prune` clears their branches out of the
   local repo and `origin`. Start with `rf prune --dry-run` — it only offers
   branches whose commits are already contained in `main`, and lists anything it
   skips along with the reason.

Adding or changing a subcommand or a flag requires a matching `README.md`
update. This is enforced, not merely requested — see
[Documentation is a gate](#documentation-is-a-gate) below.

## Two `rf` binaries, and which one you are running

`nix develop` (or direnv) puts an `rf` on `PATH`, but it is the **built flake
package** — `self.packages.<system>.default` in `flake.nix` — not your working
tree. It will happily ignore every edit you have just made.

So there are two binaries, with two jobs:

- **`rf ...`** — the packaged build. Use it to *drive the workflow*: `rf create`,
  `rf verify`, `rf graduate`. This is the bootstrap: a stable `rf` is what moves
  your changes to `rf` through the pipeline.
- **`cargo run -- ...`** — your working tree. Use it to *exercise your changes*:
  `cargo run -- status`, `cargo run -- list --no-tui`. `cargo build` also leaves
  a binary at `target/debug/rf` if you would rather invoke it directly.

The dev shell prints this distinction on entry; run `rf-dev` to reprint the
banner once it has scrolled away.

## Documentation is a gate

`tests/docs_sync.rs` asks the freshly built binary for its own CLI surface
(`rf --help`, then `rf <sub> --help`) and asserts that `README.md` documents
every subcommand and every long flag — each subcommand needs a line in the
`## Commands` block listing its flags, plus a `### ` section of its own.

It is an ordinary test on purpose. `cargo test --locked` is already both a CI
step and a configured gate in `.roll-flow.toml`, so documentation drift fails
`rf verify`, `rf graduate`, and `rf promote` locally, and the PR check remotely,
with no separate workflow to keep in sync. Two carve-outs are allowlisted in the
test: clap's generated `help` subcommand, and the universal `--help`/`--version`.

## Releases and version bumps

Every PR into `develop` or `main` must raise `version` in `Cargo.toml`.
`.github/workflows/version-bump-check.yml` compares the crate version on your
head against the target branch and fails the PR if it has not gone up — roll-flow
treats a merge into either branch as a promotion, and every promotion should
carry a version the release tooling can point at.

`Cargo.toml` is the single source of truth: `package.nix` reads the version out
of it, and `rf version` prints it. Bump it in its own commit, following the
convention already in the history:

```text
chore(release): bump version to 0.1.2 for <reason>
```

Once a version change lands on `main`, `.github/workflows/tag-on-main.yml`
creates and pushes the matching `vX.Y.Z` tag automatically (it is idempotent — an
unchanged version tags nothing). That pushed tag then triggers
`.github/workflows/release-check.yml`, which re-checks the tag against
`Cargo.toml` and confirms the lockfile is current.

## What CI checks

Every PR is gated by the `build · test · fmt · clippy` job defined in
`.github/workflows/ci.yml`:

- `cargo update --workspace --locked` (Cargo.lock is up to date)
- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo build --locked --verbose`
- `cargo test --locked --verbose`

These are the same checks configured as this repo's roll-flow gates in
`.roll-flow.toml` (`roll_to_rolling_gates` / `rolling_to_main_gates`), so
`rf verify`/`rf graduate`/`rf promote` fail locally before CI would fail
remotely. Note that `cargo test` carries `tests/docs_sync.rs`, so the README
check rides along with them.

A second required check, `.github/workflows/version-bump-check.yml`, enforces the
version bump described under [Releases and version bumps](#releases-and-version-bumps).

## Branch protection (manual maintainer follow-up — not yet configured)

The following GitHub branch-protection settings implement this model but are
**not yet applied** as of this writing; they must be set by a repo admin in
GitHub Settings → Branches (no tool in the current toolset configures this
automatically):

- **`develop`**: require pull requests before merging; require the
  `build · test · fmt · clippy` status check to pass before merging.
- **`main`**: require pull requests before merging (or restrict direct
  pushes to maintainers only); require the same status check; in practice
  `main` should only ever receive `--no-ff` promotion merges from `develop`,
  never direct feature PRs.
- Required review count and who is authorized to run `rf promote` / merge
  into `main` are maintainer calls not made here.
