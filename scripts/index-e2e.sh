#!/usr/bin/env bash
# Language-agnostic end-to-end test of the index pipeline (needs network).
#
# Walks the exact pulumi-random version pinned in the example's committed
# pulumi-lock.json into a temp index, then asserts the walker's
# independently-computed hashes are byte-identical to the committed golden
# ones. No Python project, uv2nix, or language host involved — this
# exercises only the index's contract: (provider, version, platform) → SRI.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export PYTHONPATH="$root/lock/src"
golden="$root/examples/random/pulumi-lock.json"

version=$(python3 -c "import json; print(json.load(open('$golden'))['plugins']['random']['version'])")
echo "==> walking random@$version into $tmp"
python3 -m pulumi2nix_lock.index walk \
  --index-dir "$tmp" --provider "random@$version" --max-artifacts 8

echo "==> comparing walker hashes against committed golden lock"
python3 - "$golden" "$tmp" "$version" <<'PY'
import json, sys
golden_path, index_dir, version = sys.argv[1:]
golden = json.load(open(golden_path))["plugins"]["random"]["hashes"]
shard = json.load(open(f"{index_dir}/index/random.json"))
walked = {p: h for p, h in shard["entries"][version]["hashes"].items()
          if h is not None}
missing = {p: (golden.get(p), walked.get(p))
           for p in set(golden) | set(walked)
           if golden.get(p) != walked.get(p)}
if missing:
    sys.exit(f"MISMATCH: {json.dumps(missing, indent=2)}")
print(f"identical: {len(walked)} platform hashes for random v{version}")
PY

echo "PASS"
