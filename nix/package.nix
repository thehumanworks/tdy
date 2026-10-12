{
  lib,
  rustPlatform,
  symlinkJoin,
  makeBinaryWrapper,
  gitMinimal,
  libheif,
  # The git guard asks git whether a folder holds uncommitted or unpushed
  # work; without git, Tidy refuses to remove folders that contain repos.
  withGit ? true,
  # HEIC/HEIF thumbnails on Linux need `heif-convert`; off by default because
  # libheif pulls in several video codecs.
  withHeif ? false,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;

  unwrapped = rustPlatform.buildRustPackage {
    pname = "tdy";
    inherit (cargoToml.workspace.package) version;

    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../crates
      ];
    };
    cargoLock.lockFile = ../Cargo.lock;

    # Only the `tdy` binary; `tdy-fixture` (benchmarks) stays out.
    cargoBuildFlags = [
      "--package"
      "tdy-cli"
    ];
    cargoTestFlags = [ "--workspace" ];
    # Tests run in the dev profile: the release profile's fat LTO would make
    # every test binary slow to link for no benefit.
    checkType = "debug";
    nativeCheckInputs = [ gitMinimal ];

    meta = {
      description = "Analyse, clean up, compress and organise the files on your disk";
      longDescription = ''
        Fast parallel disk scanner and organiser. Every change is a reviewable
        plan entry, journalled and undoable; rules can be written in plain
        English and judged by a decision model.
      '';
      license = lib.licenses.mit;
      mainProgram = "tdy";
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  };

  # Appended to PATH, so your own versions still win.
  runtimePath = lib.optional withGit gitMinimal ++ lib.optional withHeif libheif;
in
# A separate wrapper derivation: changing runtime tools never rebuilds Rust.
if runtimePath == [ ] then
  unwrapped
else
  symlinkJoin {
    pname = "tdy";
    inherit (unwrapped) version meta;
    paths = [ unwrapped ];
    nativeBuildInputs = [ makeBinaryWrapper ];
    postBuild = ''
      wrapProgram $out/bin/tdy --suffix PATH : ${lib.makeBinPath runtimePath}
    '';
    passthru = { inherit unwrapped; };
  }
