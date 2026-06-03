{
  description = "roll-flow (rf) — structured NixOS dotfiles workflow manager";

  inputs = {
    gigpkgs.url = "github:gignsky/gigpkgs";
    nixpkgs.follows = "gigpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.callPackage ./package.nix { };
          roll-flow = pkgs.callPackage ./package.nix { };
        }
      );

      overlays.default = final: _prev: {
        roll-flow = final.callPackage ./package.nix { };
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              rustc
              cargo
              rustfmt
              clippy
              rust-analyzer
              pkg-config
            ];
          };
        }
      );

      homeManagerModules.roll-flow = import ./modules/home-manager/roll-flow.nix;
      nixosModules.roll-flow = import ./modules/nixos/roll-flow.nix;
    };
}
