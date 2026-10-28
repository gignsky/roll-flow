# NixOS module for roll-flow — installs rf system-wide.
#
# Usage in configuration.nix:
#   imports = [ inputs.roll-flow.nixosModules.roll-flow ];
#   programs.roll-flow.enable = true;
#
# Per-user config — the machine-wide `~/.config/roll-flow/config.toml` that
# every repo's `.roll-flow.toml` is laid over — is the Home Manager module's
# job; this one only puts `rf` on the path.

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
      # See the Home Manager module: use the consumer's `pkgs.roll-flow` when
      # there is one, build from this flake's source only when there is not.
      default = pkgs.roll-flow or (pkgs.callPackage ../../package.nix { });
      defaultText = lib.literalExpression "pkgs.roll-flow or (pkgs.callPackage ./package.nix { })";
      description = "The roll-flow package to install system-wide.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
