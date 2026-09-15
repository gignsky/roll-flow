# Branch protection (manual maintainer follow-up — not yet configured)

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
