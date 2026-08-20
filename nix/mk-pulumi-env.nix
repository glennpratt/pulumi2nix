# The main user-facing builder: pulumi-lock.json (+ optional uv2nix python
# env) -> a wrapped `pulumi` executable with all plugins pre-materialized.
#
# PULUMI_HOME must stay writable (credentials, local backend state, workspace
# metadata live there), so instead of pointing it into the Nix store the
# wrapper symlinks each immutable plugin directory into
# $PULUMI_HOME/plugins/ at startup. Pulumi then sees natively-installed
# plugins: no "from $PATH" warnings, and with automatic plugin acquisition
# disabled it can never silently hit the network.
{ lib, loadLock, fetchPlugin, fetchCli, mkPluginStore, systemToTarget }:
{ pkgs
, lockFile               # path to pulumi-lock.json (from pulumi2nix-lock)
, pythonEnv ? null       # e.g. a uv2nix virtualenv; sets PULUMI_PYTHON_CMD
, pulumi ? null          # explicit CLI derivation. Default: the official
                         # release pinned in the lock's `cli` section (exact
                         # SDK/CLI/language-host lockstep), falling back to
                         # pkgs.pulumi for locks without one.
, languageHosts ? null   # [{ name, version, drv }] linked into the plugin
                         # store. Default: none when the bundled CLI already
                         # carries its language hosts, else nixpkgs' python
                         # host to accompany pkgs.pulumi.
, extraResourcePlugins ? [ ] # extra [{ name, version, drv }] beyond the lock
, name ? "pulumi"
}:
let
  lock = loadLock lockFile;
  target = systemToTarget.${pkgs.stdenv.hostPlatform.system}
    or (throw "pulumi2nix: unsupported system ${pkgs.stdenv.hostPlatform.system}");

  bundledCli =
    if lock ? cli then
      fetchCli
        {
          inherit pkgs target;
          inherit (lock.cli) version baseURL;
          hash = lock.cli.hashes.${target} or (throw (
            "pulumi2nix: no hash for the pulumi CLI v${lock.cli.version} on "
            + "${target}; re-run pulumi2nix-lock with --platform ${target}"
          ));
        }
    else null;

  cli =
    if pulumi != null then pulumi
    else if bundledCli != null then bundledCli
    else pkgs.pulumi;

  # The bundled release carries language hosts next to the CLI binary, where
  # Pulumi finds them natively; only external CLIs need store-linked hosts.
  effectiveLanguageHosts =
    if languageHosts != null then languageHosts
    else if pulumi == null && bundledCli != null then [ ]
    else
      let # renamed in nixpkgs from pulumi-language-python
        host = pkgs.pulumiPackages.pulumi-python or pkgs.pulumiPackages.pulumi-language-python;
      in
      [{
        name = "python";
        inherit (host) version;
        drv = host;
      }];

  resourcePlugins = lib.mapAttrsToList
    (pluginName: p: {
      name = pluginName;
      inherit (p) version;
      drv = fetchPlugin {
        inherit pkgs target;
        name = pluginName;
        inherit (p) version baseURL;
        hash = p.hashes.${target} or (throw (
          "pulumi2nix: no hash for plugin '${pluginName}' v${p.version} on "
          + "${target}; re-run pulumi2nix-lock with --platform ${target}"
        ));
      };
    })
    lock.plugins
  ++ extraResourcePlugins;

  pluginStore = mkPluginStore {
    inherit pkgs resourcePlugins;
    languageHosts = effectiveLanguageHosts;
  };
in
pkgs.writeShellApplication {
  inherit name;
  runtimeInputs = lib.optional (pythonEnv != null) pythonEnv;
  text = ''
    export PULUMI_HOME="''${PULUMI_HOME:-$HOME/.pulumi}"
    mkdir -p "$PULUMI_HOME/plugins"

    # Prune dangling store links from previous generations (and any plugin
    # dirs they leave empty).
    shopt -s nullglob
    for entry in "$PULUMI_HOME"/plugins/*; do
      if [ -L "$entry" ] && [ ! -e "$entry" ]; then
        rm -f "$entry"
      elif [ -d "$entry" ] && [ ! -L "$entry" ]; then
        for f in "$entry"/*; do
          if [ -L "$f" ] && [ ! -e "$f" ]; then rm -f "$f"; fi
        done
        rmdir "$entry" 2>/dev/null || true
      fi
    done

    # Pulumi's plugin scanner ignores symlinked plugin *directories*
    # (DirEntry.IsDir() is false for symlinks), so create real directories
    # and symlink the immutable contents from the Nix store into them.
    for dir in ${pluginStore}/plugins/*; do
      dest="$PULUMI_HOME/plugins/$(basename "$dir")"
      mkdir -p "$dest"
      ln -sfn "$dir"/* "$dest/"
    done

    # Fail fast instead of downloading anything at runtime.
    export PULUMI_DISABLE_AUTOMATIC_PLUGIN_ACQUISITION=true
    ${lib.optionalString (pythonEnv != null) ''
      export PULUMI_PYTHON_CMD="${pythonEnv}/bin/python"
    ''}
    exec ${cli}/bin/pulumi "$@"
  '';

  # Expose internals for debugging / composition.
  derivationArgs.passthru = {
    inherit pluginStore resourcePlugins cli;
  };
}
