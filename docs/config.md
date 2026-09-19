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
pull_mode = "ff-only"
lazygit_command = "lazygit"
```

Gate entries are shell commands run in repo root. Any failure blocks verify/promote.

`version_gate`, `tag_on_promote`, and `push_tag` control the release behavior
described in
[Versioning and release tags](../README.md#versioning-and-release-tags). All three
default to `true` and are inert in repos without a `Cargo.toml`.

`clean_protect` names branches [`rf clean`](commands/clean.md) must never delete,
on top of the stable and rolling branches and each remote's default branch, which
it already protects. Empty by default.

`pull_mode` is what the TUI's `[p]` runs on the checked-out branch: `"ff-only"`
(the default) fast-forwards or refuses, `"merge"` is git's own default, and
`"rebase"` replays local commits onto the upstream. It only affects a branch that
is checked out — pulling any other branch advances its ref through a fetch
refspec, which can only ever fast-forward. See [`status`](commands/status.md).

`lazygit_command` is the binary the TUI's `gg` launches, run as
`<command> -p <repo_root>`. Defaults to `lazygit` on `PATH`; point it at a
wrapper, a flake app, or an absolute path if a bare `lazygit` is not what you
want.

Keys added after a config was written default as above, so an older
`.roll-flow.toml` keeps loading. Note that TOML puts every bare key *before* the
first `[table]` header — appending `pull_mode` to the end of the file, after
`[host_active]`, makes it a host name instead, and the error will complain about
expecting a boolean.
