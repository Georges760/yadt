{
  description = "yadt — a terminal side-by-side diff tool for files and directory trees";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Only what the build actually reads. Keeps README edits and a dirty
      # ./target out of the derivation hash.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
        ];
      };
    in
    {
      packages = forAllSystems (pkgs: rec {
        default = yadt;

        yadt = pkgs.rustPlatform.buildRustPackage {
          pname = "yadt";
          version = (lib.importTOML ./Cargo.toml).package.version;
          inherit src;

          # Vendored straight from the checked-in lock file, so there is no
          # cargoHash to bump when a dependency changes.
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "Terminal diff tool for two files or two directory trees";
            mainProgram = "yadt";
            platforms = lib.platforms.unix;
          };
        };
      });

      apps = forAllSystems (pkgs: rec {
        default = yadt;
        yadt = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.yadt;
        };
      });

      # `nix flake check` builds the crate (which runs `cargo test`), then
      # lints and checks formatting against the same toolchain.
      checks = forAllSystems (pkgs: {
        build = self.packages.${pkgs.stdenv.hostPlatform.system}.yadt;

        clippy = self.packages.${pkgs.stdenv.hostPlatform.system}.yadt.overrideAttrs (old: {
          pname = "${old.pname}-clippy";
          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
          buildPhase = "cargo clippy --all-targets -- --deny warnings";
          installPhase = "touch $out";
          doCheck = false;
          dontFixup = true;
        });

        fmt =
          pkgs.runCommand "yadt-fmt"
            {
              nativeBuildInputs = [
                pkgs.cargo
                pkgs.rustfmt
              ];
            }
            ''
              cd ${src}
              cargo fmt --check --manifest-path Cargo.toml
              touch $out
            '';
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
          # rust-analyzer needs the standard library sources by path.
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      overlays.default = final: _prev: {
        yadt = self.packages.${final.stdenv.hostPlatform.system}.yadt;
      };

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
