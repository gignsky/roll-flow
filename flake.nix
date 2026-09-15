{
  description = "roll-flow (rf) — structured NixOS dotfiles workflow manager";

  inputs = {
    nixpkgs.url = "github:gignsky/gigpkgs/gigpkgs-unstable";
    pre-commit-hooks.follows = "nixpkgs/pre-commit-hooks";
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
      pkgsFor = sys: inputs.nixpkgs.legacyPackages.${sys};
      pkgs = pkgsFor system;
      cargoCheckWrapped = pkgs.writeShellApplication {
        name = "cargo-check-wrapper";
        runtimeInputs = [
          pkgs.cargo
          pkgs.gcc
        ];
        text = "cargo check --locked";
      };
      # Dev-shell onboarding banner. Kept as a package rather than inlined in
      # the shellHook so it is also callable as `rf-dev` to reprint on demand,
      # and so the script never has to escape nix string interpolation.
      rfDev = pkgs.writeShellApplication {
        name = "rf-dev";
        runtimeInputs = [
          pkgs.git
          pkgs.gnused
          pkgs.coreutils
        ];
        text = builtins.readFile ./scripts/rf-dev.sh;
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
          ++ [
            self.packages.${system}.default
            rfDev
          ];

        shellHook = ''
          ${self.pre-commit-check.shellHook}
          rf-dev
        '';
      };

      homeManagerModules.roll-flow = import ./modules/home-manager/roll-flow.nix;
      nixosModules.roll-flow = import ./modules/nixos/roll-flow.nix;
    };
}
