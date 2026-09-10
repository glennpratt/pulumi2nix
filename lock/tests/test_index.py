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


class CheckModeTests(unittest.TestCase):
    """--check compares a regenerated lock against the existing file.

    Uses a uv.lock with no pulumi packages so no network is touched: the
    regenerated lock is the empty {"version": 1, "plugins": {}} document.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        (self.dir / "uv.lock").write_text(
            'version = 1\n\n[[package]]\nname = "requests"\nversion = "2.0.0"\n')
        self.args = ["--uv-lock", str(self.dir / "uv.lock"),
                     "-o", str(self.dir / "pulumi-lock.json")]

    def test_check_missing_file_fails(self):
        self.assertEqual(lock.main(self.args + ["--check"]), 1)

    def test_check_passes_after_generate_and_check_writes_nothing(self):
        self.assertEqual(lock.main(self.args), 0)
        written = (self.dir / "pulumi-lock.json").read_text()
        self.assertEqual(lock.main(self.args + ["--check"]), 0)
        self.assertEqual((self.dir / "pulumi-lock.json").read_text(), written)

    def test_check_detects_stale_file(self):
        self.assertEqual(lock.main(self.args), 0)
        stale = json.loads((self.dir / "pulumi-lock.json").read_text())
        stale["plugins"]["ghost"] = {"version": "0.0.1", "hashes": {}}
        (self.dir / "pulumi-lock.json").write_text(json.dumps(stale, indent=2) + "\n")
        self.assertEqual(lock.main(self.args + ["--check"]), 1)
        # --check never rewrites the file, even when stale.
        self.assertIn("ghost", (self.dir / "pulumi-lock.json").read_text())


if __name__ == "__main__":
    unittest.main()
