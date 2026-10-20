{
  lib,
  rustPlatform,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "fspec";
  inherit (cargoToml.workspace.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../examples
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "Declare your machines in Starlark and keep them identical";
    homepage = "https://github.com/FormalSnake/formalspec";
    license = lib.licenses.mit;
    mainProgram = "fspec";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
