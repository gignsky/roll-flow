{
  rustPlatform,
  lib,
  git,
  bacon,
  lazygit,
  makeBinaryWrapper,
  runtimeShell,
}:
rustPlatform.buildRustPackage {
  pname = "roll-flow";
  version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    git
    makeBinaryWrapper
  ];

  # One binary (rf), three ways to invoke it — all from this single package:
  #   rf …          the real Cargo binary
  #   roll-flow …   symlink to rf (clap dispatches on args, not argv[0])
  #   roll flow …   `roll` is a forgiving dispatcher; `flow` is optional sugar
  # `roll` forwards any subcommand straight to rf; when called as `roll flow …`
  # it drops the `flow` word and sets argv[0] to "roll flow" so clap's help and
  # usage lines read "roll flow" instead of "rf".
  postInstall = ''
    # `bb` and `gg` in the TUI launch bacon and lazygit, so both ship with rf.
    # Suffixed, not prefixed: one the user already has on PATH (or sets via
    # `bacon_command`/`lazygit_command`) still wins; this is only the fallback
    # that makes the keys work out of the box.
    wrapProgram "$out/bin/rf" --suffix PATH : ${
      lib.makeBinPath [
        bacon
        lazygit
      ]
    }
    ln -s rf "$out/bin/roll-flow"
    printf '#!%s\nif [ "$1" = flow ]; then shift; exec -a "roll flow" %s "$@"; fi\nexec %s "$@"\n' \
      "${runtimeShell}" "$out/bin/rf" "$out/bin/rf" > "$out/bin/roll"
    chmod +x "$out/bin/roll"
  '';

  meta = {
    description = "Structured NixOS dotfiles workflow manager";
    homepage = "https://github.com/gignsky/roll-flow";
    license = lib.licenses.mit;
    mainProgram = "rf";
  };
}
