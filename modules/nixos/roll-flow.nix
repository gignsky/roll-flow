# NixOS module for roll-flow — installs rf system-wide.
#
# Usage in configuration.nix:
#   imports = [ inputs.roll-flow.nixosModules.roll-flow ];
#   programs.roll-flow.enable = true;
#
# Per-user config is handled by the Home Manager module.

{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.roll-flow;
in
{
  options.programs.roll-flow = {
    enable = lib.mkEnableOption "roll-flow git workflow manager";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../../package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage inputs.roll-flow.package.nix { }";
      description = "The roll-flow package to install system-wide.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
