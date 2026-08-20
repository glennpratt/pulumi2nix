# pulumi2nix — Implementation Plan

Goal: `uv.lock` remains the single source of truth for a Pulumi Python project;
Nix materializes both the Python environment (via uv2nix) and the exact matching
Pulumi provider plugin binaries, with no runtime network downloads and no
"using pulumi-resource-* from $PATH" warnings.

Architecture (from `scratch/Pulumi Nix Module for Dependency Management.md`):

- **pulumi2nix-lock** (Python CLI): parses `uv.lock`, downloads the wheels it
  already pins (verified against the hashes in `uv.lock`), reads each wheel's
  embedded `pulumi-plugin.json` (the authoritative plugin name/version/server),
  fetches the official `*_checksums.txt` from GitHub releases (falls back to
  hashing the tarballs directly), and writes `pulumi-lock.json` with SRI hashes
  per platform. Zero TOFU / hash copy-pasting.
- **Nix library** (`nix/`): consumes `pulumi-lock.json` → fetches provider
  tarballs as fixed-output derivations (autoPatchelfHook on Linux) → synthesizes
  a Pulumi-native plugin store layout (`plugins/resource-<name>-v<ver>/…`) →
  wraps the `pulumi` CLI so plugins resolve from the store, not the network.
- **Wrapper detail**: `PULUMI_HOME` must stay writable (credentials, local
  backend state live there), so the wrapper symlinks each immutable plugin dir
  from the Nix store into `$PULUMI_HOME/plugins/` at startup instead of
  pointing `PULUMI_HOME` into the store.

## Phase 1 — Core (this repo)

- [x] Verify release asset naming (`pulumi-resource-<name>-v<ver>-<target>.tar.gz`),
      checksum file naming (`pulumi-<name>_<ver>_checksums.txt`), and wheel
      `pulumi-plugin.json` metadata
- [x] `PLAN.md` (this file)
- [x] `pulumi2nix-lock` CLI (uv project under `lock/`)
  - [x] Parse `uv.lock`, find `pulumi-*` packages (skip the base `pulumi` SDK)
  - [x] Download + hash-verify wheels, extract `pulumi-plugin.json`
  - [x] Resolve download URLs (default GitHub pattern; honor `server` field)
  - [x] Fetch `*_checksums.txt`; fallback to downloading + hashing tarballs
  - [x] Emit `pulumi-lock.json` (SRI hashes keyed by plugin → platform)
- [x] Nix library
  - [x] `nix/fetch-plugin.nix` — fixed-output fetch + unpack (+ autoPatchelf on Linux)
  - [x] `nix/plugin-store.nix` — linkFarm in Pulumi's native layout
  - [x] `nix/mk-pulumi-env.nix` — wrapped `pulumi` (plugin symlink sync,
        `PULUMI_DISABLE_AUTOMATIC_PLUGIN_ACQUISITION`, `PULUMI_PYTHON_CMD`)
  - [x] `flake.nix` exposing `lib`, `packages.pulumi2nix-lock`, devShell
- [x] Example: `examples/random/` (pulumi + pulumi-random via uv2nix)
  - [x] `nix run` the lock tool → `pulumi-lock.json` checked in
  - [x] E2E: offline `pulumi preview` with local backend succeeds, no plugin
        downloads, no $PATH warnings
- [x] README.md (usage, architecture sketch)
- [x] CI: GitHub Actions — build + E2E preview on Linux (nix flake check)
      (workflow written; first run pending push to GitHub)

## Phase 2 — Ergonomics

- [ ] Language host handling: silence/pin `pulumi-language-python` (link
      nixpkgs' language hosts into the plugin store) — partially handled by
      nixpkgs `pulumi` wrapper already; verify no warnings in fresh env
- [ ] Overrides: per-provider URL/repo overrides for community providers
      (pulumiverse etc., `server: github://api.github.com/<org>` already
      handled; add manual escape hatch)
- [ ] `parameterized`/bridged plugin support (e.g. terraform-provider)
- [ ] Flake template (`nix flake init -t pulumi2nix#python`)
- [ ] Non-uv Python fallback? (requirements.txt) — probably out of scope

## Phase 3 — Central index (separate repo, `pulumi-nix-index`)

- [ ] Static `index.json`: name → version → platform → SRI hash
- [ ] Nightly GitHub Action scraping official `pulumi/pulumi-*` releases
- [ ] `lib.lookupHash` flake API; pulumi2nix consults index before/instead of
      per-repo `pulumi-lock.json`
- [ ] Per-repo lock remains the fallback for community/unindexed providers

## Phase 4 — Other language bridges (future)

- [ ] go.sum / package-lock.json extractors feeding the same plugin resolution

## Findings (verified during Phase 1)

- **Plugin dirs must be real directories**: Pulumi's plugin scanner uses
  `DirEntry.IsDir()`, false for symlinks. Symlinking the whole plugin dir
  into `$PULUMI_HOME/plugins` silently fails ("no language plugin found");
  the wrapper instead creates real dirs and symlinks their contents.
- **`*_checksums.txt` release assets are often SHA1**, not SHA256 (e.g.
  pulumi-random) — unusable for Nix SRI. Lock tool uses them only when
  sha256, otherwise downloads + hashes tarballs itself.
- **Toolchain auto-detection fights us**: the python language host walks up
  from the program dir looking for `uv.lock` and, on finding it, demands the
  `uv` binary at runtime (ignoring `PULUMI_PYTHON_CMD`). Projects must pin
  `runtime.options.toolchain: pip` in Pulumi.yaml; the pip toolchain honors
  `PULUMI_PYTHON_CMD` when no virtualenv is configured.
- **The venv needs `pip` importable**: plugin discovery runs
  `python -m pip list --format json` (`GetRequiredPlugins`). uv2nix venvs
  don't include pip by default → add `pip = [ ]` to the mkVirtualEnv spec.
- Plugin tarballs have no top-level dir; unpack into a clean subdir or
  stdenv's `env-vars` leaks into `$out`.
- Python package version vs plugin version can diverge; `pulumi-plugin.json`
  from the wheel is authoritative, never the PyPI version string.

## Open questions / risks

- `pulumiPackages.pulumi-python` in nixpkgs (3.255.0) may drift from the
  `pulumi` Python SDK in uv.lock (3.259.0 currently) — seems tolerant, but a
  language-host version pin from uv.lock would be tighter (Phase 2).
- Community providers hosted off `github.com/pulumi` (`server` field in
  pulumi-plugin.json) are handled for `github://api.github.com/<org>` and
  plain https servers, but untested against a real pulumiverse provider.
