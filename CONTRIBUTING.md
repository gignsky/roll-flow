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
   `roll/N-MMDD-slug` off `develop`.
2. Do the work, integrating any feature branches with `rf integrate <branch>`
   if you split work further.
3. When ready, run `rf verify` to check the branch is mergeable and run the
   configured gates without merging.
4. Run `rf graduate` from the roll branch to merge it into `develop` with a
   structured `--no-ff` commit. Open your PR against `develop`.
5. Periodically, a maintainer runs `rf promote` from `develop` to merge into
   `main` once enough graduated rolls are ready for a stable release.

Until you have `rf` built locally, `cargo build` produces `target/debug/rf`;
there is no installed package for this repo (unlike the dotfiles repo, which
consumes `rf` via gigpkgs).

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
remotely.

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
