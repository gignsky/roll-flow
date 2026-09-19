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

      # Both modules evaluated against a stub of the option tree they need, so
      # `nix flake check` catches a broken module without needing a full Home
      # Manager or NixOS evaluation. The HM check goes further: the config file
      # the module generates is fed to the freshly built `rf`, which must load
      # it without a single warning — the module's keys and rf's `Config::KEYS`
      # are thereby held to agree.
      checks = forAllSystems (
        sys:
        let
          p = pkgsFor sys;
          inherit (p) lib;
          rf = self.packages.${sys}.default;
          hm = lib.evalModules {
            modules = [
              ./modules/home-manager/roll-flow.nix
              {
                options.home.packages = lib.mkOption { type = lib.types.listOf lib.types.package; };
                options.xdg.configFile = lib.mkOption {
                  type = lib.types.attrsOf (
                    lib.types.submodule { options.source = lib.mkOption { type = lib.types.path; }; }
                  );
                };
                config = {
                  _module.args.pkgs = p;
                  programs.roll-flow = {
                    enable = true;
                    settings = {
                      username = "check";
                      host_active = {
                        alpha = true;
                        beta = false;
                      };
                      lazygit_command = "lazygit";
                    };
                  };
                };
              }
            ];
          };
          nixos = lib.evalModules {
            modules = [
              ./modules/nixos/roll-flow.nix
              {
                options.environment.systemPackages = lib.mkOption { type = lib.types.listOf lib.types.package; };
                config = {
                  _module.args.pkgs = p;
                  programs.roll-flow.enable = true;
                };
              }
            ];
          };
        in
        {
          hm-module =
            p.runCommand "roll-flow-hm-module-check"
              {
                nativeBuildInputs = [
                  rf
                  p.git
                ];
              }
              ''
                cp ${hm.config.xdg.configFile."roll-flow/config.toml".source} config.toml
                echo "generated config:"; cat config.toml
                # rf reads the global file from XDG_CONFIG_HOME and needs a repo
                # with its own marker file to run at all.
                export HOME=$PWD XDG_CONFIG_HOME=$PWD/xdg
                mkdir -p xdg/roll-flow repo && cp config.toml xdg/roll-flow/config.toml
                cd repo && git init -q -b main && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
                git branch rolling
                printf 'stable_branch = "main"\n' > .roll-flow.toml
                rf list --no-tui 2>stderr.txt || { cat stderr.txt; exit 1; }
                if grep -q warning stderr.txt; then echo "rf warned about the generated config:"; cat stderr.txt; exit 1; fi
                touch $out
              '';
          nixos-module = p.writeText "roll-flow-nixos-module-check" (
            lib.concatStringsSep "\n" (map (pkg: pkg.name) nixos.config.environment.systemPackages)
          );
        }
      );
    };
}
