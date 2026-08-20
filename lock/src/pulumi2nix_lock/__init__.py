"""pulumi2nix-lock: derive Nix-consumable Pulumi plugin hashes from uv.lock.

Reads uv.lock, finds Pulumi provider SDK packages, downloads their wheels
(verified against the sha256 already pinned in uv.lock), reads the embedded
pulumi-plugin.json to learn the authoritative plugin name/version/server,
then resolves per-platform SRI hashes for the plugin binary tarballs —
preferring the official ``*_checksums.txt`` release asset, falling back to
downloading and hashing the tarballs directly.

Output: pulumi-lock.json, consumed by the pulumi2nix Nix library.

Stdlib only, on purpose.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import sys
import tempfile
import tomllib
import urllib.error
import urllib.request
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

LOCK_FORMAT_VERSION = 1

DEFAULT_PLATFORMS = [
    "linux-amd64",
    "linux-arm64",
    "darwin-amd64",
    "darwin-arm64",
]

USER_AGENT = "pulumi2nix-lock/0.1 (+https://github.com/glennpratt/pulumi2nix)"


def log(msg: str) -> None:
    print(msg, file=sys.stderr)


def http_get(url: str) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req) as resp:
        return resp.read()


def sha256_sri(digest_hex: str) -> str:
    return "sha256-" + base64.b64encode(bytes.fromhex(digest_hex)).decode()


@dataclass
class PluginSpec:
    """A resource plugin required by a Python package in uv.lock."""

    python_package: str
    python_version: str
    name: str  # plugin name from pulumi-plugin.json
    version: str  # plugin version from pulumi-plugin.json
    server: str | None = None  # optional download server override
    hashes: dict[str, str] = field(default_factory=dict)  # platform -> SRI

    @property
    def base_url(self) -> str:
        """Base URL under which pulumi-resource-<name>-v<ver>-<target>.tar.gz lives."""
        if self.server:
            server = self.server
            # Pulumi convention: github://api.github.com/<org>[/<repo>]
            m = re.match(r"github://api\.github\.com/([^/]+)(?:/([^/]+))?$", server)
            if m:
                org = m.group(1)
                repo = m.group(2) or f"pulumi-{self.name}"
                return (
                    f"https://github.com/{org}/{repo}"
                    f"/releases/download/v{self.version}"
                )
            return server.rstrip("/")
        return (
            f"https://github.com/pulumi/pulumi-{self.name}"
            f"/releases/download/v{self.version}"
        )

    def asset_name(self, platform: str) -> str:
        return f"pulumi-resource-{self.name}-v{self.version}-{platform}.tar.gz"

    @property
    def checksums_url(self) -> str:
        return f"{self.base_url}/pulumi-{self.name}_{self.version}_checksums.txt"


def parse_uv_lock(path: Path) -> list[dict]:
    with path.open("rb") as f:
        data = tomllib.load(f)
    return data.get("package", [])


def candidate_packages(packages: list[dict]) -> list[dict]:
    """Packages that might ship a Pulumi resource plugin requirement."""
    out = []
    for pkg in packages:
        name = pkg.get("name", "")
        if name == "pulumi" or not name.startswith("pulumi-"):
            continue
        out.append(pkg)
    return out


def pick_wheel(pkg: dict) -> dict | None:
    """Pulumi provider SDKs publish a single py3-none-any wheel."""
    wheels = pkg.get("wheels", [])
    for wheel in wheels:
        if "py3-none-any" in wheel.get("url", ""):
            return wheel
    return wheels[0] if wheels else None


def read_plugin_json_from_wheel(wheel_path: Path) -> dict | None:
    with zipfile.ZipFile(wheel_path) as zf:
        for info in zf.namelist():
            if info.endswith("pulumi-plugin.json"):
                return json.loads(zf.read(info))
    return None


def fetch_wheel(wheel: dict, dest_dir: Path) -> Path:
    url = wheel["url"]
    expected = wheel.get("hash", "")
    filename = url.rsplit("/", 1)[-1]
    dest = dest_dir / filename
    data = http_get(url)
    digest = hashlib.sha256(data).hexdigest()
    if expected:
        algo, _, hexpart = expected.partition(":")
        if algo != "sha256" or digest != hexpart:
            raise RuntimeError(
                f"hash mismatch for {url}: uv.lock says {expected}, got sha256:{digest}"
            )
    dest.write_bytes(data)
    return dest


def resolve_plugin_specs(
    packages: list[dict], workdir: Path
) -> list[PluginSpec]:
    specs = []
    for pkg in candidate_packages(packages):
        name = pkg["name"]
        wheel = pick_wheel(pkg)
        if wheel is None:
            log(f"  {name}: no wheel in uv.lock, skipping")
            continue
        log(f"  {name}=={pkg['version']}: inspecting wheel")
        wheel_path = fetch_wheel(wheel, workdir)
        meta = read_plugin_json_from_wheel(wheel_path)
        if meta is None:
            log(f"  {name}: no pulumi-plugin.json (not a provider SDK), skipping")
            continue
        if not meta.get("resource", False):
            log(f"  {name}: pulumi-plugin.json has resource=false, skipping")
            continue
        specs.append(
            PluginSpec(
                python_package=name,
                python_version=pkg["version"],
                name=meta["name"],
                version=meta["version"],
                server=meta.get("server"),
            )
        )
    return specs


def parse_checksums_txt(text: str) -> dict[str, str]:
    """Parse 'sha256hex  filename' lines into filename -> hex.

    Only sha256 (64 hex chars) entries are usable: many providers publish
    SHA1 checksums (e.g. pulumi-random), which Nix fetchurl cannot consume —
    those parse to an empty dict and we fall back to hashing tarballs.
    """
    out = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and re.fullmatch(r"[0-9a-fA-F]{64}", parts[0]):
            filename = parts[1].lstrip("*")
            filename = filename.removeprefix("./")
            out[filename] = parts[0].lower()
    return out


def resolve_hashes(spec: PluginSpec, platforms: list[str]) -> None:
    checksums: dict[str, str] = {}
    try:
        checksums = parse_checksums_txt(
            http_get(spec.checksums_url).decode()
        )
    except (urllib.error.URLError, urllib.error.HTTPError):
        pass
    if checksums:
        log(f"  {spec.name}: using published sha256 checksums file")
    else:
        log(f"  {spec.name}: no usable sha256 checksums, hashing tarballs directly")

    for platform in platforms:
        asset = spec.asset_name(platform)
        if asset in checksums:
            spec.hashes[platform] = sha256_sri(checksums[asset])
            continue
        url = f"{spec.base_url}/{asset}"
        log(f"  {spec.name}: downloading {asset}")
        try:
            data = http_get(url)
        except urllib.error.HTTPError as e:
            log(f"  {spec.name}: WARNING: {url} -> HTTP {e.code}, skipping platform")
            continue
        spec.hashes[platform] = sha256_sri(hashlib.sha256(data).hexdigest())


def build_lock(specs: list[PluginSpec]) -> dict:
    plugins = {}
    for spec in sorted(specs, key=lambda s: s.name):
        plugins[spec.name] = {
            "version": spec.version,
            "pythonPackage": spec.python_package,
            "baseURL": spec.base_url,
            "hashes": dict(sorted(spec.hashes.items())),
        }
    return {"version": LOCK_FORMAT_VERSION, "plugins": plugins}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="pulumi2nix-lock", description=__doc__.splitlines()[0]
    )
    parser.add_argument(
        "--uv-lock",
        type=Path,
        default=Path("uv.lock"),
        help="path to uv.lock (default: ./uv.lock)",
    )
    parser.add_argument(
        "--output",
        "-o",
        type=Path,
        default=Path("pulumi-lock.json"),
        help="output path (default: ./pulumi-lock.json)",
    )
    parser.add_argument(
        "--platform",
        action="append",
        dest="platforms",
        metavar="TARGET",
        help=f"platform target(s) to lock (default: {' '.join(DEFAULT_PLATFORMS)})",
    )
    args = parser.parse_args(argv)

    if not args.uv_lock.exists():
        parser.error(f"{args.uv_lock} not found")

    platforms = args.platforms or DEFAULT_PLATFORMS
    packages = parse_uv_lock(args.uv_lock)
    log(f"Scanning {args.uv_lock} ({len(packages)} packages)")

    with tempfile.TemporaryDirectory(prefix="pulumi2nix-lock-") as tmp:
        specs = resolve_plugin_specs(packages, Path(tmp))

    if not specs:
        log("No Pulumi resource plugins found in uv.lock")

    for spec in specs:
        log(f"Resolving hashes for {spec.name} v{spec.version}")
        resolve_hashes(spec, platforms)

    lock = build_lock(specs)
    args.output.write_text(json.dumps(lock, indent=2) + "\n")
    log(f"Wrote {args.output} ({len(specs)} plugins)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
