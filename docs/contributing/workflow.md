# Branch model and day-to-day workflow

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
7. `rf clean` goes wider: it prunes stale remote-tracking refs from every remote
   and additionally clears merged `feature/*` branches and any branch whose
   upstream was deleted by another host. That last case is the one `rf prune`
   cannot see — if a branch you pushed was merged and deleted elsewhere, `rf
   clean` is what removes both your local copy and the stale `origin/<branch>`
   entry that tools like lazygit still list. Same containment gate, same
   `--dry-run` first.

Adding or changing a subcommand or a flag requires a matching documentation
update. This is enforced, not merely requested — see
[Documentation is a gate](docs-gate.md).
