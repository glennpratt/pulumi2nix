#!/usr/bin/env bash
# Language-agnostic end-to-end test of the index pipeline (needs network).
# No Python project, uv2nix, or language host involved — this exercises only
# the index's contract: (provider, version, platform) → SRI hash.
#
# Phase 1 — fidelity: walk the exact pulumi-random version pinned in the
#   example's committed pulumi-lock.json and assert byte-identical hashes.
# Phase 2 — breadth-first: walk two providers with a small artifact budget
#   and a time limit, then assert breadth before depth against the real
#   release history (both covered, newest-first, balanced depth, budget
#   respected).
# Phase 3 — time limit: --max-seconds 0 must hash exactly nothing.
#
# WALKER_CMD overrides the walker under test (default: the release build of
# the Rust walker, built on demand) — the same contract once validated the
# original Python implementation.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
golden="$root/examples/random/pulumi-lock.json"

if [ -z "${WALKER_CMD:-}" ]; then
  bin="$root/walker/target/release/pulumi2nix-index"
  if [ ! -x "$bin" ]; then
    echo "==> building walker (release)"
    cargo build --release --manifest-path "$root/walker/Cargo.toml"
  fi
  WALKER_CMD="$bin"
fi
walker() { $WALKER_CMD "$@"; }

# --- Phase 1: fidelity against the committed golden lock --------------------
version=$(python3 -c "import json; print(json.load(open('$golden'))['plugins']['random']['version'])")
echo "==> phase 1: walking random@$version (golden fidelity)"
walker walk --index-dir "$tmp/golden" --provider "random@$version" --max-artifacts 8

python3 - "$golden" "$tmp/golden" "$version" <<'PY'
import json, sys
golden_path, index_dir, version = sys.argv[1:]
golden = json.load(open(golden_path))["plugins"]["random"]["hashes"]
shard = json.load(open(f"{index_dir}/index/random.json"))
walked = {p: h for p, h in shard["entries"][version]["hashes"].items()
          if h is not None}
diff = {p: (golden.get(p), walked.get(p))
        for p in set(golden) | set(walked) if golden.get(p) != walked.get(p)}
if diff:
    sys.exit(f"MISMATCH: {json.dumps(diff, indent=2)}")
print(f"identical: {len(walked)} platform hashes for random v{version}")
PY

# --- Phase 2: breadth-first over two providers, budgeted + time-limited -----
BUDGET=5
echo "==> phase 2: BFS walk of random+tls (budget $BUDGET artifacts, 300s, linux-amd64 only)"
walker walk \
  --index-dir "$tmp/bfs" --provider random --provider tls \
  --platform linux-amd64 --max-artifacts "$BUDGET" --max-seconds 300 \
  2> >(tee "$tmp/bfs.log" >&2)

python3 - "$tmp/bfs" "$BUDGET" "$tmp/bfs.log" <<'PY'
import json, re, subprocess, sys
index_dir, budget, log_path = sys.argv[1], int(sys.argv[2]), sys.argv[3]

def vkey(v): return tuple(int(x) for x in v.split("."))

def latest_release(name):
    out = subprocess.run(
        ["git", "ls-remote", "--tags", f"https://github.com/pulumi/pulumi-{name}"],
        check=True, capture_output=True, text=True, timeout=120).stdout
    tags = set()
    for line in out.splitlines():
        ref = line.split()[-1].removesuffix("^{}")
        m = re.fullmatch(r"refs/tags/v(\d+)\.(\d+)\.(\d+)", ref)
        if m:
            tags.add(tuple(int(x) for x in m.groups()))
    return ".".join(map(str, max(tags)))

covered = {}
for name in ("random", "tls"):
    shard = json.load(open(f"{index_dir}/index/{name}.json"))
    covered[name] = sorted(
        (v for v, e in shard["entries"].items()
         if e["hashes"].get("linux-amd64") is not None),
        key=vkey, reverse=True)
    assert covered[name], f"breadth violated: no {name} versions covered"
    latest = latest_release(name)
    assert covered[name][0] == latest, \
        f"rank-0 violated: {name} newest covered {covered[name][0]} != latest release {latest}"

depths = {n: len(v) for n, v in covered.items()}
assert abs(depths["random"] - depths["tls"]) <= 1, \
    f"depth imbalance (BFS should alternate ranks): {depths}"

m = re.search(r"Done: (\d+) artifacts", open(log_path).read())
assert m and int(m.group(1)) <= budget, f"budget exceeded: {m and m.group(1)} > {budget}"
print(f"BFS ok: depths {depths}, {m.group(1)}/{budget} artifacts, "
      f"newest-first coverage {covered}")
PY

# --- Phase 3: a zero time budget hashes exactly nothing ---------------------
echo "==> phase 3: --max-seconds 0 walks nothing"
walker walk \
  --index-dir "$tmp/bfs" --provider random --provider tls \
  --platform linux-amd64 --max-artifacts 100 --max-seconds 0 \
  2> >(tee "$tmp/zero.log" >&2)
grep -q "Done: 0 artifacts" "$tmp/zero.log" || {
  echo "FAIL: time limit not honored" >&2; exit 1; }

echo "PASS"
