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
      (workflow written; GHA on hold for now — Linux verification runs on the
      bos-lhv4l0 VM instead, same repo path under ~/Code/github.com/glennpratt)
- [x] Linux (x86_64) verification on VM: nix flake check incl. autoPatchelf path
      (bos-lhv4l0, 2026-08-20: e2e-preview passed in sandbox, exit 0)

## Phase 2 — Ergonomics

- [x] Language host handling (prioritized 2026-08-20): pin CLI + language
      hosts to the `pulumi` SDK version from uv.lock. Implemented as a `cli`
      section in pulumi-lock.json → `fetchCli` builds the official
      pulumi/pulumi release (CLI + all language hosts in one bin dir, found
      warning-free next to the executable). Default mode for `mkPulumiEnv`;
      `pulumi = pkgs.pulumi` keeps the nixpkgs CLI + store-linked nixpkgs
      language host. Both modes have e2e checks that fail on ANY warning.
      Verified on macOS + Linux VM (pulumi-watch is a Rust binary needing
      libgcc_s via buildInputs; the Go binaries are static)
- [ ] Overrides: per-provider URL/repo overrides for community providers
      (pulumiverse etc., `server: github://api.github.com/<org>` already
      handled; add manual escape hatch)
- [ ] `parameterized`/bridged plugin support (e.g. terraform-provider)
- [ ] Flake template (`nix flake init -t pulumi2nix#python`)
- [ ] Non-uv Python fallback? (requirements.txt) — probably out of scope

## Phase 3 — Central index (data-only repo `pulumi-nix-index`; tools live HERE)

Decisions (2026-08-20): the walker/tools live in pulumi2nix (shared code with
the lock tool; pinned-rev invocation is the "slowly changing artifact"); the
index repo is data-only so its git history is a pure audit log. Entries are
hash-only — consumers always derive URLs locally, so a corrupted index can
at worst fail builds, never substitute code. Backfill is a breadth-first,
budgeted, stateless walk (frontier = ls-remote tags minus shards): every
provider's rank-0 version before any rank-1, demand lane jumps the queue,
new releases are automatically rank 0. Walk is append-only; `verify`
re-hashes random samples continuously and records drift under conflicts/
(loud failure, never a silent update).

- [x] `pulumi2nix-index` CLI (walk + verify), exposed as flake app
      (2026-08-20: rewritten in async Rust under `walker/` — tokio +
      reqwest, tarballs stream-hashed concurrently per version, never
      buffered; CLI/log/shard-format compatible with the original Python
      walker, which is retired. Unit tests + wiremock behavior tests in the
      crate; scripts/index-e2e.sh doubles as the cross-implementation
      contract test and passed unchanged against the Rust binary)
- [x] Shards `index/<provider>.json` (version → platform → SRI, null=absent)
- [x] `pulumi2nix-lock --index <path|url>` fast path, direct-hash fallback
      (tested: identical output to slow path for pulumi-random)
- [x] Index repo template in `templates/index-repo/` (cron workflow with
      single-writer concurrency, drift → auto-filed issue, pinned rev)
- [x] Reusable workflow `.github/workflows/index-walk.yml` in THIS repo;
      index repo's walk.yml is a ~10-line stub pinning it by SHA — and
      the OIDC token's signed `job_workflow_ref` claim selects the walker
      at that same pin (the github context's job_workflow_sha/ref proved
      unreliably empty on workflow_dispatch), so
      one SHA governs workflow logic + binary (no Nix/uv/rust in index CI)
- [x] Attested releases over ghcr (decision): walker-release.yml builds a
      static musl binary via `nix build .#pulumi2nix-index-static`
      (pkgsStatic — no rustup/apt; x86_64-linux only, since the walker runs
      solely on index-repo GHA runners and hashes all target platforms from
      anywhere), attests provenance, publishes release `walker-<sha>`;
      index-walk.yml downloads the asset for its pinned sha, runs
      `gh attestation verify` AND asserts the provenance references the
      pinned commit before executing
- [x] Tests before repo creation:
      - hermetic walker unit tests (`lock/tests/`, in `nix flake check`):
        BFS order, budget stop, append-only resume, absent-asset nulls,
        pinned `name@version` demand lane, checksum shortcut, drift
        detection + conflict reports, lock-tool fast path
      - language-agnostic network e2e (`scripts/index-e2e.sh`): walks the
        golden version from examples/random/pulumi-lock.json into a temp
        index and asserts byte-identical hashes
- [x] Create the actual `pulumi-nix-index` repo from the template
      (github.com/glennpratt/pulumi-nix-index, pinned to 68bc7f4; stub
      exposes budget inputs on workflow_dispatch) — first walk + branch
      protection (bot-only pushes) still pending
- [x] Default `--index` URL in pulumi2nix-lock (raw.githubusercontent URL;
      --no-index opts out)
- [ ] Later: eval-time flake input consumption (no per-repo lock for indexed
      providers); shard-lazy readFile to keep eval cheap

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
- **uv toolchain tamed without forking pulumi** (2026-08-20, replaces the
  earlier `toolchain: pip` + `pip = [ ]` workarounds — users need NO
  Pulumi.yaml or venv changes): the language host auto-selects its uv
  toolchain when `uv.lock` is present. Its preview path only needs
  `uv --version` and one `uv sync --inexact` freshness check; it honors
  `UV_PROJECT_ENVIRONMENT` for venv location, runs `<venv>/bin/python`
  directly (never `uv run`), and its plugin discovery parses `uv.lock`
  itself (no pip needed). Real `uv sync` treats Nix-installed packages as
  foreign provenance and tries to reinstall into the read-only store
  (`UV_NO_SYNC` does not affect explicit `uv sync`), so the wrapper ships a
  `uv` shim that no-ops `sync` — semantically sound, Nix already guarantees
  the venv matches `uv.lock` — and delegates all other uv commands.
  Verified offline in-sandbox on macOS + Linux. Upstream idea: propose that
  pulumi-language-python honor `UV_NO_SYNC`, making the shim unnecessary.
- Plugin tarballs have no top-level dir; unpack into a clean subdir or
  stdenv's `env-vars` leaks into `$out`.
- Python package version vs plugin version can diverge; `pulumi-plugin.json`
  from the wheel is authoritative, never the PyPI version string.

## Open questions / risks

- ~~nixpkgs language host may drift from the SDK version~~ resolved: the
  lock's `cli` section pins the official release at the uv.lock SDK version
  (default mode); drift only possible in the opt-in nixpkgs-CLI mode.
- Community providers hosted off `github.com/pulumi` (`server` field in
  pulumi-plugin.json) are handled for `github://api.github.com/<org>` and
  plain https servers, but untested against a real pulumiverse provider.
