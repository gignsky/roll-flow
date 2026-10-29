# Home Manager module for roll-flow.
#
# Installs `rf` and writes the machine-wide defaults file rf reads,
# `~/.config/roll-flow/config.toml`. Every repo still needs its own
# `<repo>/.roll-flow.toml` (that file is what marks a repo as roll-flow's, and
# `rf init` writes it), but it can be as small as the branch names: anything it
# leaves out comes from this file, key by key. See docs/config.md, "Layers".
#
# Usage in home.nix:
#   imports = [ inputs.roll-flow.homeManagerModules.roll-flow ];
#   programs.roll-flow = {
#     enable = true;
#     settings = {
#       username = "gig";
#       hosts = [ "ganoslal" "merlin" "wsl" ];
#       host_active = { ganoslal = true; merlin = true; wsl = false; };
#       lazygit_command = "lazygit";
#     };
#   };
#
# `settings` is free-form: any key docs/config.md lists may be given, and a
# key rf does not know is written as-is and warned about at run time rather
# than rejected here — so a newer rf's keys need no module change.

{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.roll-flow;
  settingsFormat = pkgs.formats.toml { };
in
{
  options.programs.roll-flow = {
    enable = lib.mkEnableOption "roll-flow git workflow manager";

    package = lib.mkOption {
      type = lib.types.package;
      # Prefer the package the consumer's nixpkgs already carries — gigpkgs
      # exposes `pkgs.roll-flow` through its overlay — and only build from this
      # flake's own source when it does not. Building here unconditionally made
      # a second, differently-hashed derivation of the same tool.
      default = pkgs.roll-flow or (pkgs.callPackage ../../package.nix { });
      defaultText = lib.literalExpression "pkgs.roll-flow or (pkgs.callPackage ./package.nix { })";
      description = "The roll-flow package to install.";
    };

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = settingsFormat.type;
        options = {
          config_version = lib.mkOption {
            type = lib.types.int;
            default = 1;
            description = "The config schema this file is written against. Leave at the default.";
          };

          rolling_branch = lib.mkOption {
            type = lib.types.str;
            default = "rolling";
            description = "Name of the rolling integration branch.";
          };

          stable_branch = lib.mkOption {
            type = lib.types.str;
            default = "main";
            description = "Name of the stable branch.";
          };

          roll_prefix = lib.mkOption {
            type = lib.types.str;
            default = "roll/";
            description = "Prefix for roll branch names.";
          };

          username = lib.mkOption {
            type = lib.types.str;
            default = "";
            example = "gig";
            description = "The user rolls are attributed to. Informational today.";
          };

          hosts = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            example = [
              "ganoslal"
              "merlin"
              "wsl"
            ];
            description = ''
              Order for `host_active`. May be left empty, in which case the
              table's keys are used in their own order.
            '';
          };

          host_active = lib.mkOption {
            type = lib.types.attrsOf lib.types.bool;
            default = { };
            example = {
              ganoslal = true;
              merlin = true;
              wsl = false;
            };
            description = ''
              Which hosts take part in verification. Inactive hosts are
              excluded (a machine that is offline or being rebuilt).
            '';
          };
        };
      };
      default = { };
      description = ''
        Contents of `~/.config/roll-flow/config.toml`, the machine-wide
        defaults every repo's `.roll-flow.toml` is laid over. Any key from
        docs/config.md may be given.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."roll-flow/config.toml".source =
      settingsFormat.generate "roll-flow-config.toml" cfg.settings;
  };
}
