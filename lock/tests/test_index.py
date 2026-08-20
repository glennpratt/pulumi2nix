"""Hermetic tests for the pulumi2nix-index walker (no network, no git).

These pin down the walker's language-agnostic contract: breadth-first
ordering, budget stops, append-only resume, absent-asset nulls, checksum
shortcuts, drift detection, and the lock tool's index fast path.
"""

import hashlib
import json
import tempfile
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

import pulumi2nix_lock as lock
import pulumi2nix_lock.index as idx


def fake_hex(url: str) -> str:
    """Deterministic fake artifact hash derived from the URL."""
    return hashlib.sha256(url.encode()).hexdigest()


def http_404(url: str) -> bytes:
    raise urllib.error.HTTPError(url, 404, "not found", None, None)


class WalkerTestCase(unittest.TestCase):
    PLATFORMS = ["linux-amd64", "darwin-arm64"]

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.index_dir = Path(self.tmp.name)
        self.hashed_urls: list[str] = []

    def fake_sha256_of_url(self, url: str) -> str:
        if "-windows-" in url or "absent-prov" in url:
            raise urllib.error.HTTPError(url, 404, "not found", None, None)
        self.hashed_urls.append(url)
        return fake_hex(url)

    def walk(self, versions: dict[str, list[str]], providers: list[str],
             max_artifacts: int = 1000, http_get=http_404,
             platforms: list[str] | None = None) -> int:
        args = ["walk", "--index-dir", str(self.index_dir),
                "--max-artifacts", str(max_artifacts)]
        for p in providers:
            args += ["--provider", p]
        for platform in platforms or self.PLATFORMS:
            args += ["--platform", platform]
        with mock.patch.object(idx, "list_versions", lambda p: versions[p]), \
             mock.patch.object(idx, "http_get", http_get), \
             mock.patch.object(idx, "sha256_of_url", self.fake_sha256_of_url):
            return idx.main(args)

    def shard(self, provider: str) -> dict:
        return json.loads((self.index_dir / "index" / f"{provider}.json").read_text())

    def test_breadth_first_ordering(self):
        versions = {"aaa": ["2.0.0", "1.0.0"], "bbb": ["1.5.0", "1.4.0"]}
        self.assertEqual(self.walk(versions, ["aaa", "bbb"]), 0)
        walked = [u.rsplit("/", 1)[-1].rsplit("-", 2)[0] for u in self.hashed_urls]
        # Rank 0 of every provider before any rank 1.
        self.assertEqual(walked[:4], ["pulumi-resource-aaa-v2.0.0"] * 2
                         + ["pulumi-resource-bbb-v1.5.0"] * 2)
        self.assertEqual(walked[4:], ["pulumi-resource-aaa-v1.0.0"] * 2
                         + ["pulumi-resource-bbb-v1.4.0"] * 2)

    def test_budget_stops_and_resume_is_append_only(self):
        versions = {"aaa": ["2.0.0", "1.0.0"]}
        self.walk(versions, ["aaa"], max_artifacts=3)
        self.assertEqual(len(self.hashed_urls), 3)
        first_run = list(self.hashed_urls)

        self.walk(versions, ["aaa"])
        # Second run only fetches what the first run's budget cut off.
        self.assertEqual(len(self.hashed_urls), 4)
        self.assertEqual(self.hashed_urls[:3], first_run)
        shard = self.shard("aaa")
        for version in ("2.0.0", "1.0.0"):
            self.assertEqual(
                set(shard["entries"][version]["hashes"]), set(self.PLATFORMS))

    def test_pinned_version_jumps_the_queue(self):
        versions = {"aaa": ["3.0.0", "2.0.0", "1.0.0"]}
        self.walk(versions, ["aaa@1.0.0", "aaa"], max_artifacts=2)
        self.assertTrue(all("v1.0.0" in u for u in self.hashed_urls))

    def test_absent_asset_recorded_null_and_not_retried(self):
        versions = {"absent-prov": ["1.0.0"]}
        self.walk(versions, ["absent-prov"])
        shard = self.shard("absent-prov")
        self.assertEqual(shard["entries"]["1.0.0"]["hashes"],
                         {p: None for p in self.PLATFORMS})
        self.walk(versions, ["absent-prov"])
        self.assertEqual(self.hashed_urls, [])  # nothing re-attempted

    def test_sha256_checksums_file_skips_downloads(self):
        versions = {"aaa": ["2.0.0"]}
        asset = "pulumi-resource-aaa-v2.0.0-linux-amd64.tar.gz"
        digest = fake_hex("checksummed")

        def http_get(url):
            self.assertIn("checksums.txt", url)
            return f"{digest}  ./{asset}\n".encode()

        self.walk(versions, ["aaa"], http_get=http_get, platforms=["linux-amd64"])
        self.assertEqual(self.hashed_urls, [])  # no tarball downloads
        self.assertEqual(self.shard("aaa")["entries"]["2.0.0"]["hashes"]["linux-amd64"],
                         lock.sha256_sri(digest))

    def test_verify_detects_drift_and_records_conflict(self):
        versions = {"aaa": ["2.0.0"]}
        self.walk(versions, ["aaa"])

        # Upstream artifact silently changes: hashes now derive differently.
        def drifted(url):
            return fake_hex(url + "tampered")

        with mock.patch.object(idx, "sha256_of_url", drifted):
            rc = idx.main(["verify", "--index-dir", str(self.index_dir),
                           "--sample", "1"])
        self.assertEqual(rc, 3)
        conflicts = list((self.index_dir / "conflicts").glob("*.json"))
        self.assertEqual(len(conflicts), 1)
        report = json.loads(conflicts[0].read_text())
        self.assertNotEqual(report["existing"], report["observed"])
        # The index entry itself was NOT rewritten.
        recorded = self.shard("aaa")["entries"]["2.0.0"]["hashes"]
        self.assertIn(report["existing"], recorded.values())

    def test_verify_passes_on_honest_index(self):
        versions = {"aaa": ["2.0.0"]}
        self.walk(versions, ["aaa"])
        with mock.patch.object(idx, "sha256_of_url", self.fake_sha256_of_url):
            rc = idx.main(["verify", "--index-dir", str(self.index_dir),
                           "--sample", "10"])
        self.assertEqual(rc, 0)

    def test_lock_tool_index_fast_path(self):
        versions = {"aaa": ["2.0.0"]}
        self.walk(versions, ["aaa"])
        spec = lock.PluginSpec(python_package="pulumi-aaa", python_version="2.0.0",
                               name="aaa", version="2.0.0")
        hashes = lock.index_hashes(str(self.index_dir), spec, self.PLATFORMS)
        self.assertEqual(set(hashes), set(self.PLATFORMS))
        # Unknown version and server-override providers miss the fast path.
        spec.version = "9.9.9"
        self.assertEqual(lock.index_hashes(str(self.index_dir), spec, self.PLATFORMS), {})
        spec.version = "2.0.0"
        spec.server = "github://api.github.com/someorg"
        self.assertEqual(lock.index_hashes(str(self.index_dir), spec, self.PLATFORMS), {})


if __name__ == "__main__":
    unittest.main()
