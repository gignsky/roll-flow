{ rustPlatform, lib }:
rustPlatform.buildRustPackage {
  pname = "roll-flow";
  version = "0.1.0";
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Structured NixOS dotfiles workflow manager";
    homepage = "https://github.com/gignsky/roll-flow";
    license = lib.licenses.mit;
    mainProgram = "rf";
  };
}
