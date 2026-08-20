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
      exampleFor = pkgs:
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
          # pip must be importable: pulumi's pip toolchain discovers required
          # plugins via `python -m pip list`.
          pythonEnv = pythonSet.mkVirtualEnv "pulumi2nix-example-random-env"
            (workspace.deps.default // { pip = [ ]; });
        in
        pulumi2nixLib.mkPulumiEnv {
          inherit pkgs pythonEnv;
          lockFile = ./examples/random/pulumi-lock.json;
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
        default = pulumi2nix-lock;
        example-random = exampleFor pkgs;
      });

      apps = forAllSystems (pkgs: {
        pulumi2nix-lock = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.pulumi2nix-lock;
        };
      });

      checks = forAllSystems (pkgs:
        let example = exampleFor pkgs;
        in {
          # Offline end-to-end: preview a stack against a local backend using
          # only Nix-provided plugins. Fails on any plugin download attempt
          # (acquisition is disabled) and on any "from $PATH" warning.
          e2e-preview = pkgs.runCommand "pulumi2nix-e2e-preview"
            { nativeBuildInputs = [ example ]; } ''
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
            if grep -i 'warning: using pulumi' preview.log; then
              echo "FAIL: plugin resolved from \$PATH instead of plugin store" >&2
              exit 1
            fi
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
