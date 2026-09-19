# Domain model

## Branch tiers

| Branch | Purpose |
|--------|---------|
| `main` (stable) | Verified on ALL hosts |
| `rolling` (integration) | Tested on target hosts, may still need others |
| `roll/N-MMDD-theme` | Numbered work batches, graduated to rolling |
| `feature/*` | Individual features, integrated into rolls |

Branch names are configurable. Defaults: `rolling_branch = "rolling"`, `stable_branch = "main"`, `roll_prefix = "roll/"`.

## Roll lifecycle states

- **active** — branch exists, not merged to rolling
- **graduated** — merged to rolling (by merge commit or Graduate commit)
- **promoted** — merged to main, stable
- **diverged** — graduated but branch has commits after the merge point (needs re-graduation)
- **blocked** — has ungraduated dependencies that must graduate first

## Quasi-rolls

Direct-to-rolling commits that happen between roll merge points are grouped into virtual
"quasi-rolls" (q1, q2, ...) by `detect-quasi-rolls`. They appear in list/status views
and follow the same verification/promotion gating as real rolls. Auto-generated commits
(generation bumps, `gig@`, `Flake-Check:` commits) are filtered out.

## Config structure

`<repo>/.roll-flow.toml`, every key of it, is documented in
[`docs/config.md`](../config.md) and that table is enforced by
`tests/config_docs_sync.rs`. The shape that matters here:

```toml
rolling_branch = "rolling"
stable_branch = "main"
roll_prefix = "roll/"
hosts = ["ganoslal", "merlin", "wsl"]   # order only; may be empty

[host_active]                            # the truth about who verifies
ganoslal = true
merlin = true
wsl = false
```

`host_active` is detected from `vars/hosts.nix` in the dotfiles repo, which is a
bare `{ host = bool; }` attrset; inactive hosts are excluded from verification
requirements (a machine offline or being rebuilt). The username comes from
`vars/default.nix`, then `$USER`, then git. Detection is a text scan of those
two files — `rf` never runs `nix eval`.
