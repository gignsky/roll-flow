{
  rustPlatform,
  lib,
  git,
}:
rustPlatform.buildRustPackage {
  pname = "roll-flow";
  version = "0.0.6";
  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ git ];

  meta = {
    description = "Structured NixOS dotfiles workflow manager";
    homepage = "https://github.com/gignsky/roll-flow";
    license = lib.licenses.mit;
    mainProgram = "rf";
  };
}
