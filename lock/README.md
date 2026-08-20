# pulumi2nix-lock

Generates `pulumi-lock.json` from a `uv.lock`: for every Pulumi provider SDK
in the lockfile, resolves the exact matching resource plugin binary release
and records Nix SRI hashes per platform. Stdlib-only Python ≥3.11.

```sh
uvx --from ./lock pulumi2nix-lock --uv-lock ./uv.lock -o ./pulumi-lock.json
# or, via the flake:
nix run github:glennpratt/pulumi2nix#pulumi2nix-lock
```

The plugin name/version comes from the `pulumi-plugin.json` embedded in each
wheel (verified against the sha256 pinned in `uv.lock`) — never from the PyPI
version string, which can diverge.
