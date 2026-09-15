# Releases and version bumps

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

## `rf` enforces the same policy locally

Promoting through a PR used to be the only way to get the bump verified and the
tag created. `rf` now does both itself, so a local `rf promote` and a PR merge
produce the same result:

- `rf verify` and `rf promote` refuse to proceed when the version has not been
  raised above the branch being merged into, and offer to bump it for you
  (`--bump <patch|minor|major>` skips the prompt). The bump lands as its own
  `chore(release):` commit, **before** the gates run — it rewrites `Cargo.lock`,
  and `rolling_to_main_gates` includes `cargo update --workspace --locked`.
- `rf promote` creates the annotated `vX.Y.Z` tag on the promotion merge commit
  and offers to push it. Like the workflow, it skips an existing tag rather than
  failing. A `--roll` promotion is several merges, so it gates and tags each one
  in turn; only the whole-branch route can offer a bump, since only it merges a
  branch a bump commit could land on.

The workflows stay in place as the backstop for anything that arrives by PR, and
the two implementations are kept in agreement deliberately: `src/core/version.rs`
documents which workflow each rule mirrors. `version_gate`, `tag_on_promote`, and
`push_tag` in `.roll-flow.toml` can turn each piece off.
