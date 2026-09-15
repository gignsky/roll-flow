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

```toml
repo_root = "/home/gig/.dotfiles"
rolling_branch = "rolling"
stable_branch = "main"
roll_prefix = "roll/"
username = "gig"
hosts = ["ganoslal", "merlin", "wsl"]

[host_active]
ganoslal = true
merlin = true
wsl = false
```

`host_active` is sourced from `vars/hosts.nix` in the dotfiles repo. Inactive hosts are
excluded from verification requirements (used when a machine is offline or being rebuilt).

Auto-generation reads `flake.nix` via `nix eval .#nixosConfigurations` and
`.#homeConfigurations` to discover hosts and username.
