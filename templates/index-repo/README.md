# pulumi-nix-index (template)

Copy these files into a fresh **data-only** repository (e.g.
`glennpratt/pulumi-nix-index`) to run a Pulumi provider hash index. The repo
holds no code — `walk.yml` is a ~10-line stub calling
[pulumi2nix](https://github.com/glennpratt/pulumi2nix)'s reusable
`index-walk.yml` workflow pinned by SHA. Via `github.job_workflow_ref`, that
one pin also selects the walker binary: the workflow downloads the static
Rust walker from pulumi2nix's `walker-<sha>` release for exactly that
commit and verifies its build-provenance attestation
(`gh attestation verify`) before executing it. Pin only SHAs that have a
walker release (cut automatically on pushes to main). This repo's git
history is therefore a pure audit log: every commit is either new hash
entries or a drift alarm.

Layout:

- `index/<provider>.json` — shards: version → platform → SRI hash.
  Hash-only by design: consumers derive download URLs locally, so a
  corrupted index can at worst fail a build, never substitute code.
  Absence is never recorded as fact — a 404 stamps a write-once `misses`
  timestamp (scheduling metadata only): fresh misses retry at natural
  priority (mid-publish releases settle), stale ones retry behind all
  fresh work, and every gap heals automatically if the asset appears.
- `providers.txt` — which official providers the cron walk covers.
- `conflicts/` — written by `verify` when a re-hashed artifact no longer
  matches its recorded hash (tag/asset rewrite upstream). Never
  auto-resolved; a human investigates.
- `.github/workflows/walk.yml` — the breadth-first backfill cron.

Consumers: `pulumi2nix-lock --index https://raw.githubusercontent.com/<owner>/pulumi-nix-index/main`
(misses fall back to direct hashing automatically).

Recommended repo settings: default branch protected, pushes restricted to
the Actions bot, and the pulumi2nix pin in `walk.yml` bumped only via
reviewed PRs.
