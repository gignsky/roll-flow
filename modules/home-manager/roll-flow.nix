# Home Manager module for roll-flow.
#
# Usage in home.nix:
#   imports = [ inputs.roll-flow.homeManagerModules.roll-flow ];
#   programs.roll-flow = {
#     enable = true;
#     settings = {
#       repo_root = "/home/gig/.dotfiles";
#       username = "gig";
#       hosts = [ "ganoslal" "merlin" "wsl" ];
#       host_active = { ganoslal = true; merlin = true; wsl = false; };
#     };
#   };

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
      default = pkgs.callPackage ../../package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage inputs.roll-flow.package.nix { }";
      description = "The roll-flow package to install.";
    };

    settings = {
      repo_root = lib.mkOption {
        type = lib.types.str;
        example = "/home/gig/.dotfiles";
        description = "Absolute path to the dotfiles repository root.";
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
        example = "gig";
        description = "Username for homeConfigurations in the flake.";
      };

      hosts = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [
          "ganoslal"
          "merlin"
          "wsl"
        ];
        description = "List of NixOS hosts managed by this dotfiles repo.";
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
          Per-host active status. Inactive hosts are excluded from
          verification requirements (e.g. a machine that is offline or
          being rebuilt).
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."roll-flow/config.toml".source = settingsFormat.generate "roll-flow-config.toml" {
      inherit (cfg.settings)
        repo_root
        rolling_branch
        stable_branch
        roll_prefix
        username
        hosts
        host_active
        ;
    };
  };
}
