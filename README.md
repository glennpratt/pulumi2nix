# pulumi2nix

Pure Nix Pulumi environments driven by `uv.lock`.

Pulumi normally downloads resource provider plugins (`pulumi-resource-*`
binaries) over the network at runtime — invisible to Nix, unpinned, and
broken inside sandboxes. `pulumi2nix` makes `uv.lock` the single source of
truth: the exact provider binaries matching your Python SDK versions are
fetched as fixed-output Nix derivations and installed into Pulumi's native
plugin cache layout, so `pulumi up` never touches the network for plugins
and never warns about `$PATH` fallbacks.

```
[ uv.lock ] ──pulumi2nix-lock──► [ pulumi-lock.json ] ──mkPulumiEnv──► wrapped `pulumi`
     │                                                                    ▲
     └──────────────────────uv2nix──────────► pythonEnv ──────────────────┘
```

## Usage

1. Generate `pulumi-lock.json` next to your `uv.lock` (re-run after `uv lock`
   changes your Pulumi SDKs):

   ```sh
   nix run github:glennpratt/pulumi2nix#pulumi2nix-lock
   ```

   It reads each Pulumi provider wheel pinned in `uv.lock` (verified against
   the lockfile's sha256), extracts the embedded `pulumi-plugin.json`
   (the authoritative plugin name/version — PyPI version strings can
   diverge), and records SRI hashes for the plugin tarball on each platform.
   No manual hash copy-pasting, no fake-hash build failures.

   It also pins the official **pulumi CLI release** at the exact `pulumi`
   SDK version from `uv.lock` (a `cli` section in the lock). That release
   bundles the CLI together with its language hosts
   (`pulumi-language-python` et al.), which Pulumi discovers warning-free
   next to its own binary — so SDK, CLI, and language host can never drift
   apart. Pass `pulumi = pkgs.pulumi;` to `mkPulumiEnv` to use the nixpkgs
   CLI instead (the nixpkgs python language host is then linked through the
   plugin store).

2. Build the wrapped CLI in your flake — no `Pulumi.yaml` changes, no extra
   venv contents; a plain `runtime: python` project works as-is:

   ```nix
   {
     inputs.pulumi2nix.url = "github:glennpratt/pulumi2nix";

     outputs = { self, nixpkgs, pulumi2nix, ... }: {
       packages.x86_64-linux.pulumi = pulumi2nix.lib.mkPulumiEnv {
         pkgs = nixpkgs.legacyPackages.x86_64-linux;
         pythonEnv = myUv2nixVenv;   # your uv2nix virtualenv
         lockFile = ./pulumi-lock.json;
       };
     };
   }
   ```

   The language host auto-selects its **uv toolchain** (it sees `uv.lock`);
   the wrapper makes that work purely: `UV_PROJECT_ENVIRONMENT` points uv at
   the Nix venv, and a `uv` shim on the wrapper's PATH turns
   `uv sync --inexact` — the pre-launch freshness check that would otherwise
   try to rewrite the read-only store venv — into the no-op it semantically
   is (Nix already guarantees the venv matches `uv.lock`). Plugin discovery
   in uv mode reads `uv.lock` directly, so the venv needs no `pip`.

The wrapper symlinks the immutable plugin directories from the Nix store
into your (writable) `PULUMI_HOME` at startup, exports
`PULUMI_PYTHON_CMD` and `PULUMI_DISABLE_AUTOMATIC_PLUGIN_ACQUISITION=true`,
then execs the real CLI. Anything missing fails fast instead of silently
downloading.

See [examples/random](examples/random) for a complete working project; the
flake's `checks.<system>.e2e-preview` runs `pulumi preview` against it fully
offline inside the Nix build sandbox.

## Library API (`pulumi2nix.lib`)

| Function | Purpose |
| --- | --- |
| `mkPulumiEnv { pkgs, lockFile, pythonEnv?, pulumi?, languageHosts?, extraResourcePlugins?, name? }` | lock file → wrapped `pulumi` executable |
| `fetchPlugin { pkgs, name, version, hash, target?, baseURL? }` | one plugin release tarball as a fixed-output derivation |
| `mkPluginStore { pkgs, resourcePlugins, languageHosts }` | synthesize the `~/.pulumi/plugins` layout as a linkFarm |
| `loadLock lockFile` | parse/validate a `pulumi-lock.json` |
| `systemToTarget` | Nix system double → Pulumi release target name |

## Notes & gotchas learned the hard way

- **Plugin dirs must be real directories.** Pulumi's plugin scanner uses
  `DirEntry.IsDir()`, which is false for symlinks — so the wrapper creates
  real dirs and symlinks their *contents* into place.
- **Official `*_checksums.txt` release assets are often SHA1**, not SHA256
  (e.g. pulumi-random). The lock tool uses them only when they're sha256 and
  otherwise downloads and hashes the tarballs itself.
- **`PULUMI_HOME` must stay writable** (credentials, local backend state), so
  it is never pointed into the Nix store.

## Status / roadmap

Phase 1 (uv.lock → pure plugins, working E2E) is done; see [PLAN.md](PLAN.md)
for the roadmap, including a central static hash index flake
(`pulumi-nix-index`) and other language frontends.
