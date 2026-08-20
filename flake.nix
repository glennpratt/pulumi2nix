{
  description = "pulumi2nix — pure Nix Pulumi environments driven by uv.lock";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # Used by the example / e2e check; consumers bring their own.
    pyproject-nix = {
      url = "github:pyproject-nix/pyproject.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    uv2nix = {
      url = "github:pyproject-nix/uv2nix";
      inputs.pyproject-nix.follows = "pyproject-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    pyproject-build-systems = {
      url = "github:pyproject-nix/build-system-pkgs";
      inputs.pyproject-nix.follows = "pyproject-nix";
      inputs.uv2nix.follows = "uv2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, pyproject-nix, uv2nix, pyproject-build-systems }:
    let
      inherit (nixpkgs) lib;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      pulumi2nixLib = import ./nix { inherit lib; };

      # --- example: examples/random built with uv2nix + pulumi2nix ---------
      exampleFor = pkgs: extraArgs:
        let
          workspace = uv2nix.lib.workspace.loadWorkspace {
            workspaceRoot = ./examples/random;
          };
          overlay = workspace.mkPyprojectOverlay { sourcePreference = "wheel"; };
          pythonSet =
            (pkgs.callPackage pyproject-nix.build.packages {
              python = pkgs.python313;
            }).overrideScope (lib.composeManyExtensions [
              pyproject-build-systems.overlays.default
              overlay
            ]);
          pythonEnv = pythonSet.mkVirtualEnv "pulumi2nix-example-random-env"
            workspace.deps.default;
        in
        pulumi2nixLib.mkPulumiEnv ({
          inherit pkgs pythonEnv;
          lockFile = ./examples/random/pulumi-lock.json;
        } // extraArgs);

      # Offline end-to-end: preview a stack against a local backend using
      # only Nix-provided plugins. Fails on any plugin download attempt
      # (acquisition is disabled) and on any warning (e.g. "from $PATH").
      mkPreviewCheck = pkgs: checkName: env:
        pkgs.runCommand "pulumi2nix-${checkName}"
          { nativeBuildInputs = [ env ]; } ''
          export HOME="$TMPDIR"
          export USER=nixbld
          export PULUMI_SKIP_UPDATE_CHECK=true
          export PULUMI_CONFIG_PASSPHRASE=test
          cp -r ${./examples/random}/. project
          chmod -R +w project
          cd project

          mkdir -p "$TMPDIR/state"
          pulumi login "file://$TMPDIR/state"
          pulumi stack init test
          pulumi preview --non-interactive 2>&1 | tee preview.log

          grep -q 'random:index:RandomPet' preview.log
          if grep -i 'warning' preview.log; then
            echo "FAIL: pulumi emitted warnings (plugin from \$PATH?)" >&2
            exit 1
          fi
          touch $out
        '';
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
        # The index walker (async Rust). GHA index repos consume it as an
        # attested release binary (walker-release.yml); this package serves
        # local use and `nix flake check` (cargo tests run in checkPhase).
        pulumi2nix-index = pkgs.rustPlatform.buildRustPackage {
          pname = "pulumi2nix-index";
          version = "0.1.0";
          src = ./walker;
          cargoLock.lockFile = ./walker/Cargo.lock;
          meta.mainProgram = "pulumi2nix-index";
        };
        default = pulumi2nix-lock;
        example-random = exampleFor pkgs { };
      });

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

      checks = forAllSystems (pkgs: {
        # Rust walker: building the package runs its unit + wiremock tests.
        walker = self.packages.${pkgs.stdenv.hostPlatform.system}.pulumi2nix-index;
        # Python lock tool: index fast-path consumption tests.
        lock-tests = pkgs.runCommand "pulumi2nix-lock-tests"
          { nativeBuildInputs = [ pkgs.python3 ]; } ''
          cd ${./lock}
          PYTHONPATH=src python3 -m unittest discover -s tests -v
          touch $out
        '';
        # Default mode: official pulumi release pinned by the lock's `cli`
        # section — SDK, CLI, and language hosts all at the uv.lock version.
        e2e-preview = mkPreviewCheck pkgs "e2e-preview" (exampleFor pkgs { });
        # nixpkgs-CLI mode: pkgs.pulumi + nixpkgs' python language host
        # linked through the plugin store.
        e2e-preview-nixpkgs-cli = mkPreviewCheck pkgs "e2e-preview-nixpkgs-cli"
          (exampleFor pkgs { pulumi = pkgs.pulumi; });
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
