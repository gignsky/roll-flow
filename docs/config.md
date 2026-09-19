# Config

`.roll-flow.toml` (repo-local), written by [`rf init`](commands/init.md) and
edited by hand from there. `rf init` on an existing file **edits it in place**:
detected keys are refreshed, keys a newer rf knows are added with their
defaults, and everything else — gate arrays, comments, order — is left as
typed.

```toml
config_version = 1
repo_root = "/absolute/path/to/repo"
rolling_branch = "rolling"
stable_branch = "main"
roll_prefix = "roll/"
version_gate = true
tag_on_promote = true
push_tag = true
mode = "manage"
username = "gig"
hosts = ["ganoslal", "merlin", "wsl"]
roll_to_rolling_gates = []
rolling_to_main_gates = []
host_gates = []
clean_protect = []
pull_mode = "ff-only"
lazygit_command = "lazygit"

[host_active]
ganoslal = true
merlin = true
wsl = false
```

## Layers

rf reads two files and lays one over the other:

1. `$XDG_CONFIG_HOME/roll-flow/config.toml` (or `~/.config/roll-flow/config.toml`)
   — machine-wide defaults. Optional. This is the file the
   [Home Manager module](nix-modules.md) writes.
2. `<repo>/.roll-flow.toml` — the repo's own. **Required**: it is what marks a
   repo as roll-flow's, and `rf init` writes it. It may be as small as the
   branch names; anything it leaves out comes from the global file.

Merging is by top-level key: a key in the repo file replaces the global one
*whole* — an array or the `[host_active]` table included — rather than being
spliced into it, so what the repo file says is exactly what applies. `rf init`
only ever writes the repo file. Unknown keys are warned about naming the file
they are in.

Only `rolling_branch`, `stable_branch` and `roll_prefix` must be present once
the layers are merged; everything else has a default.

## Every key

| key | default | set by `rf init` | read by |
|---|---|---|---|
| `config_version` | `1` | on a fresh file only; an existing value is kept | load — warned about when it differs from what this rf writes, never refused |
| `repo_root` | `""` | always (from `git rev-parse`) | nothing: the value on disk is **ignored** and re-read from git on every run, so a checkout can move |
| `rolling_branch` | — | always (first of `rolling`, `develop`, `integration` that exists) | everything |
| `stable_branch` | — | always (`main` or `master`) | everything |
| `roll_prefix` | — | fresh file, or `--roll-prefix` | everything; a value without a trailing `/` is normalized with a warning |
| `version_gate` | `true` | fresh file | `rf verify` / `rf promote` |
| `tag_on_promote` | `true` | fresh file | `rf promote` |
| `push_tag` | `true` | fresh file | `rf promote` |
| `mode` | `"manage"` | `--mode`, else kept | **nothing yet** — round-tripped only; intended to let `assist` make mutating commands report instead of merge |
| `username` | `""` | detected: `vars/default.nix` `username`, then `$USER`, then git `user.name` | **nothing yet** — reserved for attributing `user@host` rebuild commits |
| `hosts` | `[]` | detected from `vars/hosts.nix` | ordering for `host_gates`; may be empty (see `host_active`) |
| `host_active` | `{}` | detected from `vars/hosts.nix` | the source of truth for which hosts gate; when `hosts` is empty its keys are used |
| `roll_to_rolling_gates` | `[]` | never — hand-edited | `rf verify` / `rf graduate` |
| `rolling_to_main_gates` | `[]` | never — hand-edited | `rf verify` / `rf promote` |
| `host_gates` | `[]` | never — hand-edited | per-host verification; `{host}` is substituted per active host, and an entry without it is warned about |
| `clean_protect` | `[]` | never — hand-edited | `rf clean` |
| `pull_mode` | `"ff-only"` | fresh file | the TUI's `[p]` |
| `lazygit_command` | `"lazygit"` | fresh file | the TUI's `gg` |

Gate entries are shell commands run in repo root. Any failure blocks
verify/graduate/promote.

## What loading does

`Config::load` is loud but forgiving. Anything wrong that can be worked around is
reported on stderr as `warning: … (in <path>)` and worked around, so a typo is
visible on every run without an older rf refusing a file a newer one wrote:

- an **unknown key** (`clean_protct = [...]`) is named and ignored — before this,
  serde dropped it silently and the setting simply never applied
- a `config_version` other than the one this rf writes is named and accepted
- a `roll_prefix` without a trailing `/` gets one
- a `host_gates` entry with no `{host}` placeholder is pointed out
- a host in `hosts` that `[host_active]` does not mention counts as active and is
  pointed out

Only a file that cannot be parsed at all, or a merged result lacking a key with
no default (`rolling_branch`, `stable_branch`, `roll_prefix`), is an error — and
the error says to run `rf init`.

`repo_root` is never taken from the file. It is what `rf init` wrote on whichever
machine ran it, and the checkout may since have moved; git knows where the repo
is now.

## Hosts

`host_active` is the truth about which hosts take part in verification; `hosts`
only fixes their order and may name a host the table omits (which then counts as
active). When `hosts` is empty — the shape a repo whose `vars/hosts.nix` is a
bare `{ host = bool; }` attrset produces — the table's keys are used in their
own order. Treating an empty `hosts` as "no hosts" silently switched every host
gate off for exactly the repo this tool was written for.

`rf init` reads `vars/hosts.nix` in either of two shapes, comments stripped:

```nix
# the dotfiles shape
{ merlin = true; wsl = true; ganoslal = false; }

# the explicit shape
{ hosts = [ "merlin" "wsl" ]; host_active = { merlin = true; wsl = false; }; }
```

No `nix` is run. Two small flat files are scanned as text, which also works in a
repo where `nix eval` would not.

## Release flags, cleaning, pulling, lazygit

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

## Editing by hand

TOML puts every bare key *before* the first `[table]` header. A key appended to
the end of the file, after `[host_active]`, becomes a host name instead, and the
error will complain about expecting a boolean. `rf init` never makes this
mistake — keys it adds land in the root table — so the safe way to pick up a
new key is `rf init`, then edit its value.

The key table above is enforced by `tests/config_docs_sync.rs`: a key added to
`Config` without a row here fails the build, and so does a row for a key the
struct does not have.
