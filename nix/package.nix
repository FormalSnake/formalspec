{
  lib,
  rustPlatform,
  gitMinimal,
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

  # The installer's tests write the new system's spec repo with git.
  nativeCheckInputs = [ gitMinimal ];

  meta = {
    description = "Declare your machines in Starlark and keep them identical";
    homepage = "https://github.com/FormalStudio/formalspec";
    license = lib.licenses.mit;
    mainProgram = "fspec";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
