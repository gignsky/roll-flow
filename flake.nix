{
  description = "roll-flow (rf) — structured NixOS dotfiles workflow manager";

  inputs = {
    gigpkgs = {
      url = "github:gignsky/gigpkgs";
      inputs.nixpkgs.follows = "gigpkgs/nixpkgs-unstable";
    };
    nixpkgs.follows = "gigpkgs";
    pre-commit-hooks.follows = "gigpkgs/pre-commit-hooks";
  };

  outputs =
    { self, nixpkgs, ... }@inputs:
    let
      system = "x86_64-linux";
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = sys: inputs.gigpkgs.legacyPackages.${sys};
      pkgs = pkgsFor system;
      cargoCheckWrapped = pkgs.writeShellApplication {
        name = "cargo-check-wrapper";
        runtimeInputs = [
          pkgs.cargo
          pkgs.gcc
        ];
        text = "cargo check --locked";
      };
    in
    {
      packages = forAllSystems (
        sys:
        let
          p = pkgsFor sys;
        in
        {
          default = p.callPackage ./package.nix { };
          roll-flow = p.callPackage ./package.nix { };
        }
      );

      overlays.default = final: _prev: {
        roll-flow = final.callPackage ./package.nix { };
      };

      pre-commit-check = inputs.pre-commit-hooks.lib.${system}.run {
        src = ./.;
        hooks = {
          nixfmt = {
            enable = true;
          };
          statix = {
            enable = true;
          };
          deadnix = {
            enable = true;
          };
          rustfmt = {
            enable = true;
          };
          cargo-check = {
            enable = true;
            entry = pkgs.lib.getExe cargoCheckWrapped;
            pass_filenames = false;
          };
          clippy = {
            enable = false;
          };
          end-of-file-fixer = {
            enable = true;
          };
          markdownlint = {
            enable = false;
          };
        };
      };

      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs =
          with pkgs;
          [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            pkg-config
            gcc
            pre-commit
            upignore
            locker
            gitflow
            bacon
          ]
          ++ [ self.packages.${system}.default ];

        shellHook = ''
          ${self.pre-commit-check.shellHook}
        '';
      };

      homeManagerModules.roll-flow = import ./modules/home-manager/roll-flow.nix;
      nixosModules.roll-flow = import ./modules/nixos/roll-flow.nix;
    };
}
