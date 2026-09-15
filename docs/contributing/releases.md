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
