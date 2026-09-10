{
  description = "pulumi2nix dev flake: offline e2e checks (dev-only inputs live here, not in consumers' locks)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
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

    # Narrow relative-path inputs into the parent repo (Nix >= 2.26).
    # Deliberately NOT `path:..`: that would copy the whole worktree into
    # the store, gitignored build artifacts (walker/target, ~GBs) included.
    pulumi2nix-lib = {
      url = "path:../nix";
      flake = false;
    };
    example-random = {
      url = "path:../examples/random";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, pyproject-nix, uv2nix, pyproject-build-systems, pulumi2nix-lib, example-random }:
    let
      inherit (nixpkgs) lib;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      pulumi2nixLib = import pulumi2nix-lib { inherit lib; };

      # examples/random built with uv2nix + pulumi2nix, like a consumer would.
      exampleFor = pkgs: extraArgs:
        let
          workspace = uv2nix.lib.workspace.loadWorkspace {
            workspaceRoot = "${example-random}";
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
          lockFile = "${example-random}/pulumi-lock.json";
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
          cp -r ${example-random}/. project
          chmod -R +w project
          cd project

          mkdir -p "$TMPDIR/state"
          pulumi login "file://$TMPDIR/state"
          pulumi stack init test
          pulumi preview --non-interactive 2>&1 | tee preview.log

          grep -q 'random:index:RandomPet' preview.log
          grep -q 'command:local:Command' preview.log
          if grep -i 'warning' preview.log; then
            echo "FAIL: pulumi emitted warnings (plugin from \$PATH?)" >&2
            exit 1
          fi
          touch $out
        '';
    in
    {
      packages = forAllSystems (pkgs: {
        example-random = exampleFor pkgs { };
      });

      checks = forAllSystems (pkgs: {
        # Default mode: official pulumi release pinned by the lock's `cli`
        # section — SDK, CLI, and language hosts all at the uv.lock version.
        e2e-preview = mkPreviewCheck pkgs "e2e-preview" (exampleFor pkgs { });
        # nixpkgs-CLI mode: pkgs.pulumi + nixpkgs' python language host
        # linked through the plugin store.
        e2e-preview-nixpkgs-cli = mkPreviewCheck pkgs "e2e-preview-nixpkgs-cli"
          (exampleFor pkgs { pulumi = pkgs.pulumi; });
      });
    };
}
