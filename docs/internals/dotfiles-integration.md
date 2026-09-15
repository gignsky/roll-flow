# Integration with dotfiles

The dotfiles repo (`~/.dotfiles`) calls `rf` as a plain binary via the justfile:

```just
roll-start theme:
    rf start {{theme}}

roll-graduate:
    rf graduate

roll-promote:
    rf promote
```

The `rf` binary is provided by gigpkgs. The dotfiles repo does NOT contain roll-flow
source — it just consumes the package.

Config auto-detection reads the dotfiles repo's `flake.nix`, `vars/hosts.nix`, and git
branch structure. The `repo_root` in config always points to the current repo when `rf`
is invoked (detected via `git rev-parse --show-toplevel`).
