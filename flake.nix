{
  description = "pulumi2nix — pure Nix Pulumi environments driven by uv.lock";

  # Deliberately the only input: `lib` is pkgs-agnostic and the tools build
  # with plain nixpkgs. The uv2nix stack used by the examples and offline
  # e2e checks lives in the dev/ subflake so consumers never lock it.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      pulumi2nixLib = import ./nix { inherit lib; };

      # The index walker (async Rust). GHA index repos consume it as an
      # attested release binary (walker-release.yml); the plain package
      # serves local use and `nix flake check` (cargo tests in checkPhase).
      walkerFor = pkgs': pkgs'.rustPlatform.buildRustPackage {
        pname = "pulumi2nix-index";
        version = "0.1.0";
        src = ./walker;
        cargoLock.lockFile = ./walker/Cargo.lock;
        meta.mainProgram = "pulumi2nix-index";
      };
    in
    {
      lib = pulumi2nixLib;

      packages = forAllSystems (pkgs: rec {
        pulumi2nix-lock = pkgs.python3Packages.buildPythonApplication {
          pname = "pulumi2nix-lock";
          version = "0.1.0";
          pyproject = true;
          src = ./lock;
          build-system = [ pkgs.python3Packages.hatchling ];
          meta.mainProgram = "pulumi2nix-lock";
        };
        pulumi2nix-index = walkerFor pkgs;
        default = pulumi2nix-lock;
      } // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (rec {
        # Fully static (musl) walker for the attested GHA release — built
        # with Nix, no rustup/apt toolchains (see walker-release.yml).
        pulumi2nix-index-static = walkerFor pkgs.pkgsStatic;
        # The release artifact itself: a deterministic tarball (mtime,
        # ownership, ordering, gzip timestamp all pinned) so the executable
        # bit survives GitHub Releases and anyone can `nix build` this
        # commit and compare sha256 against the published asset —
        # reproducibility on top of the attestation.
        pulumi2nix-index-tarball = pkgs.runCommand
          "pulumi2nix-index-${pkgs.stdenv.hostPlatform.system}.tar.gz"
          { } ''
          install -m 0755 ${lib.getExe pulumi2nix-index-static} pulumi2nix-index
          tar --sort=name --owner=0 --group=0 --numeric-owner \
              --mtime='1970-01-01 00:00:00 UTC' \
              -cf - pulumi2nix-index | gzip -n > $out
        '';
      }));

      apps = forAllSystems (pkgs: {
        pulumi2nix-lock = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.pulumi2nix-lock;
        };
        pulumi2nix-index = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.pulumi2nix-index;
        };
      });

      # Nixpkgs-only checks. The offline e2e preview checks (which need the
      # uv2nix stack) live in dev/ — run `nix flake check ./dev` as well.
      checks = forAllSystems (pkgs: {
        # Rust walker: building the package runs its unit + wiremock tests.
        walker = self.packages.${pkgs.stdenv.hostPlatform.system}.pulumi2nix-index;
        # Python lock tool: index fast-path + --check mode tests.
        lock-tests = pkgs.runCommand "pulumi2nix-lock-tests"
          { nativeBuildInputs = [ pkgs.python3 ]; } ''
          cd ${./lock}
          PYTHONPATH=src python3 -m unittest discover -s tests -v
          touch $out
        '';
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.uv
            pkgs.pulumi
            pkgs.python3
            pkgs.nixpkgs-fmt
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
