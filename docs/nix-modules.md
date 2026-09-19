# Nix modules

This flake exports a Home Manager module and a NixOS module:

```nix
inputs.roll-flow.homeManagerModules.roll-flow
inputs.roll-flow.nixosModules.roll-flow
```

Both live under `programs.roll-flow` and both default `package` to the
consumer's `pkgs.roll-flow` when it has one (gigpkgs' overlay provides it),
building from this flake's own source only when it does not.

## Home Manager: `programs.roll-flow`

Installs `rf` and writes `~/.config/roll-flow/config.toml` — the machine-wide
defaults every repo's `.roll-flow.toml` is laid over (see
[config.md, "Layers"](config.md#layers)). `settings` is free-form: any key in
[the key table](config.md#every-key) may be given, and one rf does not know is
written as-is and warned about at run time rather than rejected at evaluation,
so a newer rf's keys need no module change. The typed options carry defaults
and documentation for the common ones.

```nix
programs.roll-flow = {
  enable = true;
  settings = {
    username = "gig";
    hosts = [ "ganoslal" "merlin" "wsl" ];
    host_active = { ganoslal = true; merlin = true; wsl = false; };
    lazygit_command = "lazygit";
  };
};
```

A repo still needs its own `.roll-flow.toml` (that is what marks it as
roll-flow's; `rf init` writes it), but with the global file in place it can be
reduced to the branch names and the repo-specific gate arrays.

## NixOS: `programs.roll-flow`

`enable` and `package` only; puts `rf` in `environment.systemPackages`. Per-user
config is the Home Manager module's job.

## Checks

`nix flake check` evaluates both modules against a stub of the option tree they
need (`checks.<system>.hm-module`, `checks.<system>.nixos-module`), so a broken
module fails without a full Home Manager or NixOS evaluation. The HM check goes
further: the generated `config.toml` is fed to the freshly built `rf`, which
must load it without a single warning — the module and `Config::KEYS` are held
to agree.

## Consuming from gigpkgs and dotfiles (not yet done)

What the two downstream repos need, recorded here so it is not rediscovered.
Their state as surveyed on 2026-09-19:

- **gigpkgs** (`~/local_repos/gigpkgs`) packages `rf` via the input
  `roll-flow.url = "github:gignsky/roll-flow"` (unpinned; `flake.lock` holds
  the rev) and `pkgs/inputs/roll-flow.nix`, exposed as `pkgs.roll-flow` through
  `overlays/default.nix`. It exports **no** roll-flow module today. Its module
  aggregators are auto-discovered: every `modules/home/inputs/<name>.nix` and
  `modules/nixos/inputs/<name>.nix` of the shape

  ```nix
  # gigpkgs inputMan: managed homeManagerModules aggregator
  { inputs }:
  {
    roll-flow = inputs.roll-flow.homeManagerModules.roll-flow;
  }
  ```

  is merged into `homeManagerModules` / `nixosModules` (see
  `modules/home/inputs/gigvim.nix` for the existing pattern;
  `modules/nixos/inputs/` does not exist yet). `inputman update roll-flow`
  discovers module outputs and writes both files itself. Steps: run it (or add
  the two files by hand), re-lock, confirm with `nix flake show` that
  `homeManagerModules.roll-flow` and `nixosModules.roll-flow` appear, add a
  news entry.

- **dotfiles** (`~/.dotfiles`) consumes gigpkgs *as* `nixpkgs`
  (`nixpkgs.url = "github:gignsky/gigpkgs/gigos-2605"`), so the module arrives
  as `inputs.nixpkgs.homeManagerModules.roll-flow` — there is no `gigpkgs`
  input. Home Manager is standalone (`homeManagerConfiguration`, one per
  `gig@<host>`), and `home/gig/common/core/` is scanned, so a file dropped
  there is imported. `rf` is currently in the dev shell only, not in
  `home.packages`. A root-level `tmp-roll-flow.nix` is dead: it references
  `inputs.gigpkgs` (nonexistent) and a module gigpkgs does not export, and it
  is not in a scanned directory. Steps: delete it; add
  `home/gig/common/core/roll-flow.nix`:

  ```nix
  { inputs, lib, configLib, ... }:
  let
    vars = import (configLib.relativeToRoot "vars") { inherit lib; };
    hostActive = import (configLib.relativeToRoot "vars/hosts.nix");
  in
  {
    imports = [ inputs.nixpkgs.homeManagerModules.roll-flow ];
    programs.roll-flow = {
      enable = true;
      settings = {
        username = vars.username;
        hosts = builtins.attrNames hostActive;
        host_active = hostActive;
      };
    };
  }
  ```

  then trim `~/.dotfiles/.roll-flow.toml` to the branch names and its gate
  arrays (those are repo-specific and belong there). Its `config_version = 3`
  will be warned about until set to `1`. Optionally
  `hosts/common/core/roll-flow.nix` with the NixOS module for hosts that want
  `rf` system-wide.
