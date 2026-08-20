"""pulumi2nix-index: breadth-first backfill walker for a pulumi-nix-index repo.

The index repo is data-only: shards under ``index/<provider>.json`` mapping
version -> platform -> SRI hash (or null for a published release that lacks
that platform's asset). This walker lives in pulumi2nix so it shares the
asset-naming and checksum logic with pulumi2nix-lock; the index repo's
workflow invokes it pinned to a pulumi2nix revision.

Design properties:

- **Stateless walk**: the frontier is recomputed every run as (all release
  tags, via ``git ls-remote`` — no API quota) minus (what the shards already
  hold). Idempotent, crash-tolerant, self-healing.
- **Breadth-first**: all providers' rank-0 (latest) versions before any
  rank-1, so coverage goes wide before deep; new releases are automatically
  rank 0 and thus front of the queue.
- **Budgeted**: stops after --max-artifacts or --max-seconds, whichever
  comes first; shards are written after every version so partial runs land.
- **Hash-only entries**: download URLs are always derived locally by the
  consumer; a corrupted index can cause build failures, never substitution.
- **Append-only walk**: an existing hash is never overwritten or re-derived
  by ``walk``. Drift detection is ``verify``'s job: it re-hashes a random
  sample of existing entries per run (continuously re-witnessing the whole
  index against tag/asset rewrites) and records disagreements under
  ``conflicts/`` with a loud non-zero exit — never a silent update.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import subprocess
import sys
import time
import urllib.error
from datetime import datetime, timezone
from pathlib import Path

from . import (
    DEFAULT_PLATFORMS,
    http_get,
    log,
    parse_checksums_txt,
    sha256_of_url,
    sha256_sri,
)

SHARD_FORMAT_VERSION = 1

TAG_RE = re.compile(r"^refs/tags/v(\d+)\.(\d+)\.(\d+)$")


def provider_repo_url(provider: str) -> str:
    return f"https://github.com/pulumi/pulumi-{provider}"


def release_base_url(provider: str, version: str) -> str:
    return f"{provider_repo_url(provider)}/releases/download/v{version}"


def asset_name(provider: str, version: str, platform: str) -> str:
    return f"pulumi-resource-{provider}-v{version}-{platform}.tar.gz"


def checksums_url(provider: str, version: str) -> str:
    return f"{release_base_url(provider, version)}/pulumi-{provider}_{version}_checksums.txt"


def list_versions(provider: str) -> list[str]:
    """All stable release versions of a provider, newest first.

    Uses `git ls-remote --tags` — a single request with no GitHub API quota.
    Prerelease/suffixed tags are skipped.
    """
    out = subprocess.run(
        ["git", "ls-remote", "--tags", provider_repo_url(provider)],
        check=True,
        capture_output=True,
        text=True,
        timeout=120,
    ).stdout
    versions: set[tuple[int, int, int]] = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) != 2:
            continue
        m = TAG_RE.match(parts[1].removesuffix("^{}"))
        if m:
            versions.add((int(m.group(1)), int(m.group(2)), int(m.group(3))))
    return [f"{a}.{b}.{c}" for a, b, c in sorted(versions, reverse=True)]


# --- shard I/O --------------------------------------------------------------


def shard_path(index_dir: Path, provider: str) -> Path:
    return index_dir / "index" / f"{provider}.json"


def load_shard(index_dir: Path, provider: str) -> dict:
    path = shard_path(index_dir, provider)
    if path.exists():
        return json.loads(path.read_text())
    return {"version": SHARD_FORMAT_VERSION, "provider": provider, "entries": {}}


def save_shard(index_dir: Path, shard: dict) -> None:
    path = shard_path(index_dir, shard["provider"])
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    shard["entries"] = dict(sorted(
        shard["entries"].items(),
        key=lambda kv: tuple(int(x) for x in kv[0].split(".")),
        reverse=True,
    ))
    tmp.write_text(json.dumps(shard, indent=2) + "\n")
    tmp.replace(path)


def record_conflict(index_dir: Path, provider: str, version: str,
                    platform: str, existing: str, observed: str) -> None:
    path = index_dir / "conflicts" / f"{provider}-{version}-{platform}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({
        "provider": provider,
        "version": version,
        "platform": platform,
        "existing": existing,
        "observed": observed,
        "observedAt": datetime.now(timezone.utc).isoformat(),
    }, indent=2) + "\n")


# --- walk -------------------------------------------------------------------


class Budget:
    def __init__(self, max_artifacts: int, max_seconds: float):
        self.max_artifacts = max_artifacts
        self.deadline = time.monotonic() + max_seconds
        self.spent = 0

    def exhausted(self) -> bool:
        return self.spent >= self.max_artifacts or time.monotonic() >= self.deadline

    def charge(self) -> None:
        self.spent += 1


def resolve_version(shard: dict, provider: str, version: str,
                    platforms: list[str], budget: Budget) -> None:
    """Fill in missing platform hashes for one version.

    Append-only: platforms already recorded (including null = asset absent)
    are never touched — re-observation of existing entries is `verify`'s job.
    """
    entry = shard["entries"].setdefault(version, {"observedAt": None, "hashes": {}})
    hashes = entry["hashes"]

    checksums: dict[str, str] = {}
    try:
        checksums = parse_checksums_txt(
            http_get(checksums_url(provider, version)).decode()
        )
    except (urllib.error.URLError, urllib.error.HTTPError):
        pass

    for platform in platforms:
        if platform in hashes or budget.exhausted():
            continue
        asset = asset_name(provider, version, platform)
        if asset in checksums:
            observed = sha256_sri(checksums[asset])
        else:
            url = f"{release_base_url(provider, version)}/{asset}"
            try:
                budget.charge()
                observed = sha256_sri(sha256_of_url(url))
            except urllib.error.HTTPError as e:
                if e.code in (403, 404):
                    log(f"  {provider} v{version} {platform}: no asset (HTTP {e.code})")
                    hashes[platform] = None
                    continue
                log(f"  {provider} v{version} {platform}: HTTP {e.code}, will retry next run")
                continue
            except urllib.error.URLError as e:
                log(f"  {provider} v{version} {platform}: {e.reason}, will retry next run")
                continue
        hashes[platform] = observed
        log(f"  {provider} v{version} {platform}: {observed}")
    if entry["observedAt"] is None and hashes:
        entry["observedAt"] = datetime.now(timezone.utc).isoformat()


def is_version_complete(shard: dict, version: str, platforms: list[str]) -> bool:
    hashes = shard["entries"].get(version, {}).get("hashes", {})
    return all(p in hashes for p in platforms)


def cmd_walk(args: argparse.Namespace) -> int:
    index_dir = args.index_dir
    platforms = args.platforms or DEFAULT_PLATFORMS

    providers: list[str] = list(args.providers or [])
    if args.providers_file:
        for line in args.providers_file.read_text().splitlines():
            line = line.split("#")[0].strip()
            if line:
                providers.append(line)
    if not providers:
        log("No providers given (use --provider and/or --providers-file)")
        return 2

    # `name@version` pins one exact version (demand lane) and jumps every
    # BFS layer; bare names get the full breadth-first enumeration.
    pinned: list[tuple[str, str]] = []
    plain: list[str] = []
    for p in providers:
        if "@" in p:
            name, _, ver = p.partition("@")
            pinned.append((name, ver.removeprefix("v")))
        else:
            plain.append(p)

    # Build the frontier: (rank, provider, version) for every incomplete
    # version, breadth-first — every provider's rank N before any rank N+1.
    frontier: list[tuple[int, str, str]] = []
    shards: dict[str, dict] = {}
    for provider, version in pinned:
        shards.setdefault(provider, load_shard(index_dir, provider))
        if not is_version_complete(shards[provider], version, platforms):
            frontier.append((-1, provider, version))
    for provider in plain:
        shards.setdefault(provider, load_shard(index_dir, provider))
        try:
            versions = list_versions(provider)
        except subprocess.SubprocessError as e:
            log(f"{provider}: failed to enumerate tags ({e}), skipping this run")
            continue
        log(f"{provider}: {len(versions)} release versions")
        for rank, version in enumerate(versions):
            if not is_version_complete(shards[provider], version, platforms):
                frontier.append((rank, provider, version))
    frontier.sort(key=lambda t: (t[0], t[1]))

    log(f"Frontier: {len(frontier)} incomplete versions; "
        f"budget {args.max_artifacts} artifacts / {args.max_seconds}s")

    budget = Budget(args.max_artifacts, args.max_seconds)
    for rank, provider, version in frontier:
        if budget.exhausted():
            break
        shard = shards[provider]
        resolve_version(shard, provider, version, platforms, budget)
        save_shard(index_dir, shard)

    log(f"Done: {budget.spent} artifacts hashed this run")
    return 0


# --- verify -----------------------------------------------------------------


def cmd_verify(args: argparse.Namespace) -> int:
    index_dir = args.index_dir
    population: list[tuple[str, str, str, str]] = []
    for path in sorted((index_dir / "index").glob("*.json")):
        shard = json.loads(path.read_text())
        for version, entry in shard["entries"].items():
            for platform, sri in entry["hashes"].items():
                if sri is not None:
                    population.append((shard["provider"], version, platform, sri))

    if not population:
        log("Index is empty; nothing to verify")
        return 0
    sample = random.sample(population, min(args.sample, len(population)))
    log(f"Re-verifying {len(sample)} of {len(population)} entries")

    mismatches = 0
    for provider, version, platform, recorded in sample:
        url = f"{release_base_url(provider, version)}/{asset_name(provider, version, platform)}"
        try:
            observed = sha256_sri(sha256_of_url(url))
        except (urllib.error.URLError, urllib.error.HTTPError) as e:
            log(f"  {provider} v{version} {platform}: unavailable ({e}) — investigate")
            mismatches += 1
            continue
        if observed != recorded:
            log(f"  DRIFT: {provider} v{version} {platform}: index has {recorded}, observed {observed}")
            record_conflict(index_dir, provider, version, platform, recorded, observed)
            mismatches += 1
        else:
            log(f"  ok: {provider} v{version} {platform}")

    if mismatches:
        log(f"{mismatches} entries FAILED re-verification — see conflicts/")
        return 3
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="pulumi2nix-index",
        description="Breadth-first backfill walker for a pulumi-nix-index repo",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    walk = sub.add_parser("walk", help="fill in missing hashes, breadth-first, within a budget")
    walk.add_argument("--index-dir", type=Path, default=Path("."),
                      help="index repo root (shards live under index/)")
    walk.add_argument("--provider", action="append", dest="providers", metavar="NAME[@VERSION]",
                      help="provider name (repeatable); name@version pins one "
                           "exact version and jumps the breadth-first queue")
    walk.add_argument("--providers-file", type=Path,
                      help="file with one provider name per line (# comments ok)")
    walk.add_argument("--platform", action="append", dest="platforms", metavar="TARGET",
                      help=f"platforms to index (default: {' '.join(DEFAULT_PLATFORMS)})")
    walk.add_argument("--max-artifacts", type=int, default=200,
                      help="stop after hashing this many tarballs (default 200)")
    walk.add_argument("--max-seconds", type=float, default=2400,
                      help="stop after this much wall clock (default 2400)")
    walk.set_defaults(func=cmd_walk)

    verify = sub.add_parser("verify", help="re-hash a random sample of existing entries")
    verify.add_argument("--index-dir", type=Path, default=Path("."))
    verify.add_argument("--sample", type=int, default=50,
                        help="number of entries to re-verify (default 50)")
    verify.set_defaults(func=cmd_verify)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
