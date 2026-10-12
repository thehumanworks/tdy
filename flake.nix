{
  description = "Tidy: analyse, clean up, compress and organise the files on your disk";

  # A channel tarball instead of github:NixOS/nixpkgs (home-manager does the
  # same): smaller, lockable and not subject to GitHub rate limits. Flakes
  # that use Tidy usually replace it with their own:
  # `inputs.tdy.inputs.nixpkgs.follows = "nixpkgs";`
  # Tidy needs Rust 1.88 or newer from whichever nixpkgs builds it.
  inputs.nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      # nixpkgs 26.11 dropped x86_64-darwin.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        tdy = pkgs.callPackage ./nix/package.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.tdy;
      });

      overlays.default = final: _prev: { tdy = final.callPackage ./nix/package.nix { }; };

      nixosModules.default = import ./nix/nixos-module.nix self;
      # The system module only adds a package, so it works on nix-darwin too.
      darwinModules.default = import ./nix/nixos-module.nix self;
      homeModules.default = import ./nix/hm-module.nix self;
      homeManagerModules.default = self.homeModules.default;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.tdy.unwrapped ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            hyperfine
            nixfmt
          ];
          env.RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      checks = forAllSystems (
        pkgs:
        let
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./crates
              ./rustfmt.toml
              ./flake.nix
              ./nix
            ];
          };
        in
        {
          # Builds and runs the whole test suite.
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.tdy;
          fmt =
            pkgs.runCommand "tdy-fmt"
              {
                nativeBuildInputs = [
                  pkgs.rustfmt
                  pkgs.nixfmt
                ];
              }
              ''
                cd ${src}
                rustfmt --check --edition 2024 $(find crates -name '*.rs')
                nixfmt --check flake.nix nix/*.nix
                touch $out
              '';
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
