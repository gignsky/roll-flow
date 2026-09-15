# Config

`.roll-flow.toml` (repo-local), written by [`rf init`](commands/init.md):

```toml
config_version = 1
repo_root = "/absolute/path/to/repo"
rolling_branch = "rolling"
stable_branch = "main"
roll_prefix = "roll/"
username = "gig"
hosts = []
version_gate = true
tag_on_promote = true
push_tag = true
roll_to_rolling_gates = []
rolling_to_main_gates = []
clean_protect = []
```

Gate entries are shell commands run in repo root. Any failure blocks verify/promote.

`version_gate`, `tag_on_promote`, and `push_tag` control the release behavior
described in
[Versioning and release tags](../README.md#versioning-and-release-tags). All three
default to `true` and are inert in repos without a `Cargo.toml`.

`clean_protect` names branches [`rf clean`](commands/clean.md) must never delete,
on top of the stable and rolling branches and each remote's default branch, which
it already protects. Empty by default.
