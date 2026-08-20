"""Tests for the lock tool's index fast path (index_hashes).

The walker itself is the Rust crate under walker/ (unit + wiremock tests
there; cross-implementation contract via scripts/index-e2e.sh). These tests
cover the Python side that consumes an index: shard lookup, misses, and the
server-override opt-out.
"""

import json
import tempfile
import unittest
from pathlib import Path

import pulumi2nix_lock as lock

PLATFORMS = ["linux-amd64", "darwin-arm64"]


class IndexFastPathTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.index_dir = Path(self.tmp.name)
        shard = {
            "version": 1,
            "provider": "aaa",
            "entries": {
                "2.0.0": {
                    "observedAt": "2026-08-20T00:00:00+00:00",
                    "hashes": {
                        "linux-amd64": "sha256-AAAA",
                        "darwin-arm64": "sha256-BBBB",
                        "linux-arm64": None,  # asset absent upstream
                    },
                }
            },
        }
        (self.index_dir / "index").mkdir()
        (self.index_dir / "index" / "aaa.json").write_text(json.dumps(shard))

    def spec(self, **kw):
        return lock.PluginSpec(python_package="pulumi-aaa", python_version="2.0.0",
                               name="aaa", version="2.0.0", **kw)

    def test_hit_serves_requested_platforms(self):
        hashes = lock.index_hashes(str(self.index_dir), self.spec(), PLATFORMS)
        self.assertEqual(hashes, {"linux-amd64": "sha256-AAAA",
                                  "darwin-arm64": "sha256-BBBB"})

    def test_null_absent_assets_are_not_served(self):
        hashes = lock.index_hashes(str(self.index_dir), self.spec(),
                                   PLATFORMS + ["linux-arm64"])
        self.assertNotIn("linux-arm64", hashes)

    def test_unknown_version_misses(self):
        spec = self.spec()
        spec.version = "9.9.9"
        self.assertEqual(lock.index_hashes(str(self.index_dir), spec, PLATFORMS), {})

    def test_unknown_provider_misses(self):
        spec = self.spec()
        spec.name = "nope"
        self.assertEqual(lock.index_hashes(str(self.index_dir), spec, PLATFORMS), {})

    def test_server_override_always_takes_slow_path(self):
        # The index vouches only for the official URL convention.
        spec = self.spec(server="github://api.github.com/someorg")
        self.assertEqual(lock.index_hashes(str(self.index_dir), spec, PLATFORMS), {})


if __name__ == "__main__":
    unittest.main()
