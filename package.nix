{
  rustPlatform,
  lib,
  git,
  runtimeShell,
}:
rustPlatform.buildRustPackage {
  pname = "roll-flow";
  version = "0.0.7";
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ git ];

  # One binary (rf), three ways to invoke it — all from this single package:
  #   rf …          the real Cargo binary
  #   roll-flow …   symlink to rf (clap dispatches on args, not argv[0])
  #   roll flow …   `roll` is a forgiving dispatcher; `flow` is optional sugar
  # `roll` forwards any subcommand straight to rf; when called as `roll flow …`
  # it drops the `flow` word and sets argv[0] to "roll flow" so clap's help and
  # usage lines read "roll flow" instead of "rf".
  postInstall = ''
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
