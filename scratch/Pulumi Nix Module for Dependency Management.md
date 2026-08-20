# **Pulumi Nix Module for Dependency Management**

[https://gemini.google.com/app/6378164c5a39b30f](https://gemini.google.com/app/6378164c5a39b30f)

*User prompt: Consider how to make a pulumi nix module that aids using pulumi like uv2nix or whatever is out there for go.mod. uv2nix is ideal because uv.lock carries critical metadata, I'm not sure pulumi has such information. Pulumi also has some behavior I'm not sure I understand. pulumi pulumi language python then pulumi providers, in our case enumerated in python and seemingly magically installed on invocation*

Response: The core reason Pulumi feels like "magic" is that it relies on a two-part execution engine: a **language host** (like pulumi-language-python) and **resource provider plugins** (gRPC server binaries like pulumi-resource-aws or pulumi-resource-docker).

When you run pulumi up, the CLI spawns your Python script, which uses SDK calls to register resources. The CLI inspects those resource calls, checks if the underlying binaries exist in \~/.pulumi/plugins (or on your system $PATH), and if missing, attempts to download them dynamically from Pulumi’s registry.

Unlike uv2nix, which parses uv.lock for exact wheel hashes, Pulumi has no native cross-language lockfile that captures **both** the language dependencies and the engine's binary plugins simultaneously.

### **Key Technical Obstacles**

> * **Dual-Layer Dependency Graph:** Python needs its PyPI packages (pulumi-aws), but the Pulumi CLI *also* needs the matching gRPC plugin binary (pulumi-resource-aws version vX.Y.Z).  
> * **Implicit Plugin Fetching:** The CLI implicitly downloads binaries at runtime, bypassing Nix’s pure/network-isolated build sandbox.  
> * **Path Resolution Arbitrage:** Pulumi checks \~/.pulumi/plugins/, but it *also* scans $PATH for binaries matching pulumi-resource-\<name\>.

### **Architectural Design for a pulumi2nix Module**

To build a declarative, pure Nix module for Pulumi, you must decoupled runtime network access from the execution step by providing both Python dependencies and binary plugins via Nix derivations.

#### **1\. Decouple and Lock the Environment**

Use standard Nix tools (like uv2nix or poetry2nix) for the language layer, but construct a parallel system for the Pulumi provider binaries.

`# Example structure for a custom nix builder function`  
`mkPulumiEnv = { pkgs, pythonEnv, providers }:`  
  `let`  
    `# Symlink all required binary plugins into a single directory`  
    `pulumiPlugins = pkgs.symlinkJoin {`  
      `name = "pulumi-plugins";`  
      `paths = providers; # e.g., [ pkgs.pulumi-packages.pulumi-aws ]`  
    `};`  
  `in`  
  `pkgs.writeShellApplication {`  
    `name = "pulumi-wrapped";`  
    `runtimeInputs = [ pkgs.pulumi pythonEnv ];`  
    `text = ''`  
      `# 1. Direct Pulumi to use Nix-provided plugins on PATH`  
      `export PATH="${pulumiPlugins}/bin:$PATH"`

      `# 2. Prevent dynamic network downloads at runtime`  
      `export PULUMI_IGNORE_AMBIENT_PLUGINS=false`  
        
      `# 3. Force Pulumi to execute using the wrapped Python environment`  
      `export PULUMI_PYTHON_CMD="${pythonEnv}/bin/python"`

      `exec pulumi "$@"`  
    `'';`  
  `};`

#### **2\. Handling Provider Binaries in Nixpkgs**

Nixpkgs already packages many core Pulumi providers (e.g., pkgs.pulumi-packages.pulumi-aws). Your module can accept a manifest mapping provider versions to Nix derivation hashes:

`# manifest.nix (or generated nix code)`  
`{`  
  `providers = [`  
    `{ name = "aws"; version = "6.10.0"; sha256 = "1a2b3c..."; }`  
    `{ name = "docker"; version = "4.5.0"; sha256 = "4d5e6f..."; }`  
  `];`  
`}`

#### **3\. Execution Flow**

> 1. **Language Layer (uv2nix):** Generates a precise pythonEnv containing the pulumi and pulumi\_aws Python SDK packages.  
> 2. **Provider Layer:** Materializes the matched pulumi-resource-\* binaries using stdenv.mkDerivation or Nixpkgs attributes.  
> 3. **Environment Wrapper:** Overrides PULUMI\_PYTHON\_CMD and exposes provider binaries on $PATH, eliminating the CLI's urge to auto-download untrusted binaries during pulumi up.

**Sources**

> 1. [Authoring a Source-Based Plugin Package | Pulumi Docs](https://www.pulumi.com/docs/iac/guides/building-extending/packages/source-based-plugin/)  
> 2. [warning: using pulumi-resource-azure-native / pulumi-language-nodejs from $PATH · Issue \#14525 \- GitHub](https://github.com/pulumi/pulumi/issues/14525)

---

*User prompt: I think the missing link is something that ensures that e.g. pulumi\_linode in uv.lock and nix match*

Response: You hit on the exact missing link: **Pulumi language SDKs hardcode their matching binary plugin version internally**.

When you install Python's pulumi\_linode package, the Python code contains embedded metadata (inside its internal \_utilities.py file) that specifically requests the binary pulumi-resource-linode at an exact version string (e.g., v5.13.0).

If uv.lock requests pulumi-linode==5.13.0, but your Nix expression pulls pkgs.pulumi-packages.pulumi-linode at version 5.12.0, Pulumi's engine detects a mismatch, decides its local PATH binary is invalid, and tries to download pulumi-resource-linode-v5.13.0 over the network.

To solve this and ensure complete parity between uv.lock and Nix, your pulumi2nix system needs to parse the lockfile and drive both sides automatically.

### **The Auto-Resolution Pipeline**

               `[ uv.lock ]`  
                    `│`  
           `┌────────┴────────┐`  
           `▼                 ▼`  
   `(Extract Python)   (Extract Providers)`  
   `pulumi-linode==5.13.0 -> "linode" v5.13.0`  
           `│                 │`  
           `▼                 ▼`  
     `uv2nix builder   Nix Provider Fetcher`  
    `(Python Wheels)    (Binary Tarballs)`  
           `│                 │`  
           `└────────┬────────┘`  
                    `▼`  
          `[ Pure Shell / DevEnv ]`

### **How to Implement the Locking Resolver**

#### **1\. Parse uv.lock for Pulumi Packages**

Create a Nix helper that inspects the JSON/TOML output of uv.lock. Filter for any package starting with pulumi- (excluding the base pulumi SDK).

`# Extract mapping of provider name -> version from uv.lock`  
`# e.g., "pulumi-linode" "5.13.0" -> { name = "linode"; version = "5.13.0"; }`  
`extractPulumiProviders = uvLock:`  
  `let`  
    `pkgs = uvLock.package or [];`  
    `pulumiPkgs = builtins.filter (p: builtins.match "pulumi-.*" p.name != null && p.name != "pulumi") pkgs;`  
  `in`  
  `builtins.map (p: {`  
    `name = builtins.replaceStrings ["pulumi-"] [""] p.name;`  
    `version = p.version;`  
  `}) pulumiPkgs;`

#### **2\. Build a Dynamic Provider Fetcher**

Instead of relying on nixpkgs having the exact matching version of pulumi-resource-linode, write a generic binary fetcher that pulls the exact release binary directly from GitHub releases based on the version extracted from uv.lock.

`fetchPulumiProvider = { name, version, hashes ? {} }:`  
  `let`  
    `# Map Nix system strings to Pulumi release target names`  
    `systemMap = {`  
      `"x86_64-linux"  = "linux-amd64";`  
      `"aarch64-linux" = "linux-arm64";`  
      `"x86_64-darwin" = "darwin-amd64";`  
      `"aarch64-darwin"= "darwin-arm64";`  
    `};`  
    `target = systemMap.${builtins.currentSystem};`  
      
    `# Standard GitHub Release URL pattern used by Pulumi`  
    `url = "https://github.com/pulumi/pulumi-${name}/releases/download/v${version}/pulumi-resource-${name}-v${version}-${target}.tar.gz";`  
  `in`  
  `pkgs.stdenv.mkDerivation {`  
    `pname = "pulumi-resource-${name}";`  
    `inherit version;`

    `src = pkgs.fetchurl {`  
      `inherit url;`  
      `# In practice, you'd feed FOD sha256s or use a fixed-output derivation lookup`  
      `hash = hashes."${name}-${version}" or pkgs.lib.fakeHash;`   
    `};`

    `sourceRoot = ".";`  
    `installPhase = ''`  
      `mkdir -p $out/bin`  
      `cp pulumi-resource-${name} $out/bin/`  
      `chmod +x $out/bin/*`  
    `'';`  
  `};`

#### **3\. Enforce Version Strictness in the Engine**

Once your Nix expression automatically fetches the exact binaries corresponding to uv.lock, wrap the binary execution to block Pulumi from falling back to dynamic downloads:

`mkStrictPulumiApp = { uvLock, pythonEnv }:`  
  `let`  
    `providerSpecs = extractPulumiProviders uvLock;`  
    `providerBins = builtins.map (spec: fetchPulumiProvider spec) providerSpecs;`  
    `pluginPath = pkgs.symlinkJoin {`  
      `name = "pulumi-plugins";`  
      `paths = providerBins;`  
    `};`  
  `in`  
  `pkgs.writeShellApplication {`  
    `name = "pulumi";`  
    `runtimeInputs = [ pkgs.pulumi pythonEnv ];`  
    `text = ''`  
      `# 1. Inject exact matched provider binaries`  
      `export PATH="${pluginPath}/bin:$PATH"`

      `# 2. Hard fail if Pulumi tries to hit the network for an unmapped plugin`  
      `export PULUMI_DISABLE_AUTOMATIC_PLUGIN_ACQUISITION=true`

      `exec pulumi "$@"`  
    `'';`  
  `};`

### **Why This Bridge Works**

> 1. **Zero Divergence:** uv.lock remains the single source of truth. When you update pulumi-linode via uv add, the Nix expression automatically changes its fetch URL for pulumi-resource-linode to match the exact same version string.  
> 2. **Nix Sandbox Compliance:** Setting PULUMI\_DISABLE\_AUTOMATIC\_PLUGIN\_ACQUISITION=true prevents hidden network requests. If the version in uv.lock doesn't match the binary available on $PATH, Pulumi aborts immediately instead of attempting dynamic downloads.

---

*User prompt: The one annoying thing I'm not sure about is making a less painful fetcher. Fetchers seem to require you to run them and fail, steal the checksum, update a file by hand, re-run. uv2nix completely fixes this because of uv.lock IIUC. Maybe something could smooth this for the the provider binaries too*

Response: The pain you are describing is the classic Nix "TOFU" (Trust On First Use) friction. Having to intentionally fail a build just to copy-paste SHA256 hashes into a file completely breaks the seamless developer flow that uv2nix provides.

uv2nix gets away with this because Python wheels on PyPI include pre-computed SHA256 hashes directly in uv.lock. Pulumi provider binaries don't have a single lockfile that exposes these binary hashes to Nix out-of-the-box.

To smooth this out for a pulumi2nix module, you have three primary architectural approaches, ranging from "zero-maintenance automatic" to "fully deterministic pre-rendered lockfiles."

### **Option 1: Immutability via Impure CLI Generator (pulumi2nix-lock)**

This is the pattern tools like flake-checker, cargo2nix, or poetry2nix use. You create a tiny helper CLI script that parses your uv.lock, looks up the exact SHA256 hashes for all required operating systems/architectures, and writes a pulumi-lock.json file to your repository.

       `[ uv.lock ]`  
            `│`  
            `▼`  
`┌──────────────────────┐`  
`│  pulumi2nix-lock     │  <-- Fetch release asset hashes`  
`└──────────────────────┘      from GitHub / Pulumi API`  
            `│`  
            `▼`  
  `[ pulumi-lock.json ]`   
            `│`  
            `▼`  
`┌──────────────────────┐`  
`│     Nix Flake        │  <-- Pure & deterministic!`  
`└──────────────────────┘`

#### **The pulumi-lock.json Structure**

Your tool outputs a locked file alongside your code:

`{`  
  `"linode-v5.13.0": {`  
    `"x86_64-linux": "sha256-abc...",`  
    `"aarch64-darwin": "sha256-xyz..."`  
  `}`  
`}`

#### **How the Helper CLI Fetches Hashes Automatically**

Pulumi publishes provider releases to GitHub. You can parse uv.lock, grab the requested versions, and fetch the official release hashes without triggering a Nix build failure:

`# Minimal generator script concept in Python`  
`import httpx, json, tomllib`

`with open("uv.lock", "rb") as f:`  
    `uv_data = tomllib.load(f)`

`# Extract packages named pulumi-<provider>`  
`providers = [`  
    `pkg for pkg in uv_data["package"]`   
    `if pkg["name"].startswith("pulumi-") and pkg["name"] != "pulumi"`  
`]`

`lock_data = {}`  
`for p in providers:`  
    `name = p["name"].replace("pulumi-", "")`  
    `version = p["version"]`  
      
    `# Pulumi releases checksum files on GitHub Releases`  
    `checksum_url = f"https://github.com/pulumi/pulumi-{name}/releases/download/v{version}/SHA256SUMS"`  
    `res = httpx.get(checksum_url)`  
      
    `# Parse SHA256SUMS text directly into a Nix-compatible hash mapping`  
    `lock_data[f"{name}-{version}"] = parse_sha256sums(res.text)`

`with open("pulumi-lock.json", "w") as f:`  
    `json.dump(lock_data, f, indent=2)`

**Why this wins:** You run pulumi2nix-lock once when upgrading dependencies (or in a pre-commit hook). Nix then consumes pulumi-lock.json via standard builtins.fromJSON with zero manual hash copy-pasting.

### **Option 2: pkgs.fetchurl with Impure Fallback (Nix Flake \--impure)**

If you hate adding extra JSON lockfiles to your repo and prioritize developer convenience during local iteration, you can build a derivation that uses **Nix's \--impure capability** or **Nix CA (Content-Addressed) derivations** to derive the hash dynamically via a fixed-output derivation shim.

Alternatively, you can write a light wrapper around nix-prefetch-url inside your Nix expression:

`# Impure hash lookup script run at evaluation time if hash is missing`  
`getProviderHash = name: version: system:`  
  `let`  
    `lockFile = ./pulumi-hashes.json;`  
    `cachedHash = (builtins.fromJSON (builtins.readFile lockFile))."${name}-${version}"."${system}" or null;`  
  `in`  
  `if cachedHash != null then`  
    `cachedHash`  
  `else`  
    `# Fails gracefully or instructs the user to run the prefetcher auto-gen`  
    `throw "Missing hash for ${name} ${version}. Run 'nix run .#update-pulumi-hashes' to resolve.";`

### **Option 3: Pre-indexed Global Provider Registry**

If your team manages many Pulumi repositories, you can publish a central Flake/Nix registry (similar to how nixpkgs itself works) that automatically indexes all official Pulumi provider releases nightly.

`# flake.nix`  
`inputs = {`  
  `pulumi-index.url = "github:your-org/pulumi-nix-index";`  
`};`

`outputs = { nixpkgs, pulumi-index, ... }: {`  
  `# The index maps (provider + version + system) directly to SRI hashes`  
  `hash = pulumi-index.hashes."linode" downloadVersion targetSystem;`  
`}`

Since provider releases are immutable (e.g., pulumi-resource-linode v5.13.0 for x86\_64-linux will never change its hash once published), a shared background scraper can build this index once for everyone in your organization.

### **Summary Recommendation**

| Strategy | Maintenance | Hash Copy-Pasting? | CI Pureness |
| :---- | :---- | :---- | :---- |
| **Manual Error Loop** | Terrible | **Yes** | Fully Pure |
| **pulumi2nix-lock CLI (Option 1\)** | Low (Automated via script) | **No** | Fully Pure |
| **Shared Flake Index (Option 3\)** | Zero (Centralized) | **No** | Fully Pure |

The **pulumi2nix-lock CLI script (Option 1\)** is by far the most pragmatic pattern. It gives you the exact same developer workflow as uv: whenever you update uv.lock, you run your updater, pulumi-lock.json syncs instantly, and Nix operates in pure mode without human intervention.

---

*User prompt: Shared Flake Index works because you trust it and it's sha, but how can it offer a matrix of versions?*

Response: The Shared Flake Index handles an arbitrary matrix of provider versions by structuring its dataset as a **version-indexed tree** rather than a single static mapping.

Because Pulumi provider binaries are immutable once released to GitHub, the index only ever needs to append new versions—it never invalidates old ones.

### **1\. Data Structure of the Flake Index**

Inside the index repository, a background runner (like a GitHub Action) monitors Pulumi releases and outputs a nested JSON/Nix data structure:

`# index.nix inside the shared flake`  
`{`  
  `linode = {`  
    `"5.13.0" = {`  
      `"x86_64-linux"   = "sha256-abc...";`  
      `"aarch64-linux"  = "sha256-def...";`  
      `"x86_64-darwin"  = "sha256-ghi...";`  
      `"aarch64-darwin" = "sha256-jkl...";`  
    `};`  
    `"5.12.0" = { ... };`  
  `};`  
  `aws = {`  
    `"6.10.0" = { ... };`  
    `"6.9.1"  = { ... };`  
  `};`  
`}`

### **2\. Consuming the Matrix Dynamically**

When your project evaluates uv.lock, your Nix code extracts the exact provider and version string (e.g., linode at 5.13.0), checks the current system platform, and performs a dynamic attribute lookup against the index flake input:

`# Your project's flake.nix`  
`{`  
  `inputs = {`  
    `nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";`  
    `pulumi-index.url = "github:your-org/pulumi-nix-index"; # Central index input`  
  `};`

  `outputs = { self, nixpkgs, pulumi-index, ... }:`  
    `let`  
      `pkgs = nixpkgs.legacyPackages.x86_64-linux;`  
        
      `# Function to resolve a hash for ANY provider/version matrix combination`  
      `getProviderHash = { name, version, system }:`  
        `let`  
          `hash = pulumi-index.lib.lookupHash { inherit name version system; };`  
        `in`  
        `if hash != null then hash`  
        `else throw "Pulumi provider '${name}' version '${version}' is missing from pulumi-index. Please update your flake input.";`  
    `in {`  
      `# Pass getProviderHash into your derivation builder`  
    `};`  
`}`

### **3\. How the Index Maintained (Automated Ingestion)**

To prevent the index from becoming stale, the central repo uses a simple Scheduled GitHub Action pipeline:

> 1. **Scrape Release Registry:** Query the GitHub API for releases across all official pulumi/pulumi-\* repositories.  
> 2. **Fetch SHA256SUMS:** Download the official SHA256SUMS file published alongside each release asset.  
> 3. **Format to SRI:** Convert GitHub's standard hex SHA256 hashes into Nix SRI format (sha256-\<base64\>).  
> 4. **Commit & Push:** Append new versions to index.nix and publish a new flake revision.

### **The Trade-off**

> * **Advantage:** Complete elimination of manual hash management across all developer repositories in your team/org. Updating a dependency is as simple as running nix flake update pulumi-index.  
> * **Edge Case Handling:** If a developer locks to a *brand new* or *custom/forked* Pulumi provider version that isn't in the index yet, the lookup fails fast. You can design your module to allow a local fallback hash block (hashes \= { "custom-provider-1.0.0" \= "sha256-..."; };) to bypass the index when testing unreleased versions.

---

*User prompt: that is neat \- can this work in a magical way and redirect actual downloads to github or would it be better to recompile?*

Response: It is vastly better—and practically necessary—to **redirect downloads to the pre-built binaries on GitHub** rather than recompiling them from source.

Recompiling Pulumi providers inside Nix is a notorious nightmare. Pulumi providers aren't standard Go binaries; they are complex code generators that depend on upstream provider schemas, bridged Terraform providers, dynamic plugin interfaces, and custom C cross-compilation toolchains. Compiling them from scratch often takes 20+ minutes per provider and requires complex dependency orchestration.

Downloading official GitHub binaries gives you identical results in seconds with zero build overhead.

### **How the "Magical" GitHub Redirect Works**

Because the Shared Flake Index already knows the provider name, version, architecture, and exact SRI hash, your Nix module can construct the GitHub release download URL on the fly:

`# Minimal logic inside your nix module`  
`fetchFromGitHubRelease = { name, version, system, hash }:`  
  `let`  
    `# Map Nix system target strings to Pulumi release artifact naming`  
    `targetMap = {`  
      `"x86_64-linux"   = "linux-amd64";`  
      `"aarch64-linux"  = "linux-arm64";`  
      `"x86_64-darwin"  = "darwin-amd64";`  
      `"aarch64-darwin" = "darwin-arm64";`  
    `};`  
    `target = targetMap.${system};`  
      
    `# Standardized URL structure used across all Pulumi official repos`  
    `url = "https://github.com/pulumi/pulumi-${name}/releases/download/v${version}/pulumi-resource-${name}-v${version}-${target}.tar.gz";`  
  `in`  
  `pkgs.stdenv.mkDerivation {`  
    `pname = "pulumi-resource-${name}";`  
    `inherit version;`

    `# Nix directly downloads the binary tarball from GitHub`  
    `src = pkgs.fetchurl {`  
      `inherit url hash;`  
    `};`

    `sourceRoot = ".";`

    `# ELF patching for Linux systems (handles glibc/dynamic linking)`  
    `installPhase = ''`  
      `mkdir -p $out/bin`  
      `cp pulumi-resource-${name} $out/bin/`  
      `chmod +x $out/bin/*`  
        
      `# Patches binaries on Linux to work seamlessly with NixOS/Nix`  
      `${pkgs.lib.optionalString pkgs.stdenv.isLinux ''`  
        `autoPatchelf $out/bin`  
      `''}`  
    `'';`

    `nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.autoPatchelfHook ];`  
  `};`

### **The One Catch on Linux: Dynamic Linking**

Pre-compiled binaries built on standard OS distributions (like Ubuntu) assume dynamic libraries like glibc live in hardcoded locations like /lib64.

On NixOS or non-standard Nix environments, running raw downloaded binaries directly will fail with No such file or directory.

By adding autoPatchelfHook (shown above), Nix intercepts the downloaded GitHub binary during derivation creation, fixes its dynamic linker instructions (RPATH), and makes it run natively on Nix—giving you the instant download speed of official binaries with the total purity of Nix.

---

*User prompt: So, if I build this index, does it require hosting even or is it static / git only?*

Response: It is **100% static and Git-only**. You do not need to host a server, run an API, manage a database, or pay for infrastructure.

Nix Flakes are designed to treat Git repositories—and specifically files like .nix or .json inside them—as raw, queryable data structures.

### **How a Git-Only Flake Index Works**

When a user's project imports your index as a flake input, Nix fetches the repository state via Git (or a tarball cached by GitHub) and evaluates the JSON or Nix file lazily in memory.

`┌───────────────────────────────────────┐`  
`│     github:your-org/pulumi-index       │`  
`│                                       │`  
`│   ├── flake.nix                       │`  
`│   └── index.json  <-- Pure static text│`  
`└───────────────────────────────────────┘`  
                    `│`  
                    `│ Imported as Flake Input`  
                    `▼`  
`┌───────────────────────────────────────┐`  
`│          Developer Machine            │`  
`│  1. Nix reads index.json statically   │`  
`│  2. Looks up SRI Hash for version     │`  
`│  3. Downloads binary directly from    │`  
`│     GitHub Releases                   │`  
`└───────────────────────────────────────┘`

### **What the Index Repo Actually Looks Like**

#### **1\. index.json (Static Data)**

A background GitHub Action updates this single text file periodically:

`{`  
  `"linode": {`  
    `"5.13.0": {`  
      `"x86_64-linux": "sha256-R2Z3...=",`  
      `"aarch64-darwin": "sha256-K9P0...="`  
    `}`  
  `},`  
  `"aws": {`  
    `"6.10.0": {`  
      `"x86_64-linux": "sha256-L1M2...="`  
    `}`  
  `}`  
`}`

#### **2\. flake.nix (Static Lookup API)**

The index exposes helper functions directly in Nix code so consumers don't have to parse raw JSON themselves:

`{`  
  `description = "Static SRI Hash Index for Pulumi Provider Binaries";`

  `outputs = { self }:`   
    `let`  
      `rawIndex = builtins.fromJSON (builtins.readFile ./index.json);`  
    `in {`  
      `# Public library helper for consumers`  
      `lib.lookupHash = { name, version, system }:`  
        `rawIndex.${name}.${version}.${system} or null;`  
    `};`  
`}`

### **Why Git-Only is Ideal for Nix**

> * **Zero Hosting Costs:** GitHub hosts the Git repository and the static tarballs for free.  
> * **Flake Evaluation Caching:** Nix caches the Git commit hash of inputs in flake.lock. Once resolved, evaluation is instant and offline-capable for cached versions.  
> * **Atomic Versioning:** Every commit to the index is an immutable snapshot. If a project locks to commit a1b2c3d, its provider lookup logic will never break or change unexpectedly.  
> * **Decentralized Upgrades:** Updating the registry across all developer machines is as simple as running nix flake update pulumi-index in their local repositories.

---

*User prompt: has anyone else tried to build this?*

Response: No one has built a comprehensive, off-the-shelf pulumi2nix system using this exact Flake Index architecture yet. The current landscape for using Pulumi with Nix consists mostly of partial workarounds rather than an end-to-end lockfile resolver.

### **What Exists Right Now**

> * **Nixpkgs Provider Packaging (pkgs.pulumi-packages.\*):** Nixpkgs manually packages a limited subset of popular Pulumi provider binaries (e.g., pulumi-aws, pulumi-gcp). However, they are maintained as standard Nixpkgs derivations, meaning they lag behind upstream releases and don't dynamically sync with your uv.lock.  
> * **Standard buildGoModule Wrapping:** Developers using Go with Pulumi often write custom buildGoModule expressions that manually fetch individual binary tarballs via pkgs.fetchurl when a build breaks, suffering through the exact "TOFU" failure loop discussed earlier.  
> * **terranix / tf2nix Ecosystem:** Similar index-driven generators exist for the Terraform ecosystem, but because Pulumi relies on a two-tier execution model (Language SDK \+ gRPC Provider Plugin Binary), no tool currently bridges language-specific lockfiles (uv.lock, package-lock.json, go.sum) directly to Pulumi’s binary releases.

### **Why the Community Hasn't Built It Yet**

> 1. **Multi-Language Fragmented Scope:** Pulumi supports Python, TypeScript, Go, .NET, and Java. A tool parsing uv.lock only solves the Python ecosystem, while another maintainer would need to build a parser for package-lock.json or cargo.  
> 2. **Dynamic Runtime Behavior:** The default behavior of the Pulumi CLI is to download plugins quietly at runtime. Many Nix users simply give up on pure builds for Pulumi and allow imperative runtime downloads in non-sandboxed dev environments.  
> 3. **Terraform Overlap:** Many Nix infrastructure enthusiasts leaned heavily toward terranix (transpiling Nix to HCL) to avoid language-host binary mismatches entirely.

### **The Opportunity**

By combining uv2nix with a static Git-only **SRI Index Flake**, you would effectively be building the first true **pulumi2nix** tool.

It solves a massive friction point for teams running infrastructure-as-code in strict CI/CD sandboxes or NixOS environments, where imperatively downloading untrusted binaries from the internet during pulumi up is prohibited.

**Sources**

> 1. [Resource Providers | Pulumi Docs](https://www.pulumi.com/docs/iac/concepts/providers/)

---

*User prompt: So, help me layout the moving parts, here's what I have in mind: The main pulumi2nix program / library / whatever git index with cron style automation language plugin bridges Better flakes for pulumi language plugins that don't generate warnings \- this is a file location problem that hardlinks can likely solve 2026-08-20T04:38:11.9233180Z pulumi:pulumi:Stack app-platform-kpp-ord-1-apple running warning: using pulumi-resource-pulumi-python from $PATH at /bin/pulumi-resource-pulumi-python*

Response: The architecture you are envisioning solves every layer of Pulumi's runtime magic.

The warning you highlighted (using pulumi-resource-pulumi-python from $PATH...) happens because the Pulumi CLI expects language host executables and plugin binaries to live in a specific directory layout inside \~/.pulumi/plugins/ (e.g., plugins/language-python-vX.Y.Z/pulumi-language-python). When it falls back to finding them on $PATH, it prints a warning.

Creating a synthetic, hardlinked/symlinked plugin store directly within the Nix store fixes this warning while preserving pure, offline execution.

### **Core Architecture & Moving Parts**

`┌─────────────────────────────────────────────────────────────────────────┐`  
`│                      Central Automated Index                            │`  
`│                     (github:org/pulumi-index)                           │`  
`│  Cron Job: Scrapes Pulumi Releases -> Updates static index.json Flake   │`  
`└─────────────────────────────────────────────────────────────────────────┘`  
                                    `│`  
                                    `▼`  
`┌─────────────────────────────────────────────────────────────────────────┐`  
`│                          pulumi2nix Library                             │`  
`│                                                                         │`  
`│  1. Parse uv.lock (via uv2nix) -> Extract Python dependencies           │`  
``│  2. Identify `pulumi-*` SDK versions                                   │``  
``│  3. Lookup SRI Hashes in `pulumi-index`                                 │``  
``│  4. Fetch & `autoPatchelf` provider binaries                           │``  
`│  5. Materialize Nix Plugin Store Structure (Fixes $PATH warning)        │`  
`└─────────────────────────────────────────────────────────────────────────┘`  
                                    `│`  
                                    `▼`  
`┌─────────────────────────────────────────────────────────────────────────┐`  
`│                           Wrapped Runner                                │`  
`│                                                                         │`  
``│  Executes `pulumi` configured to read pure store without warnings      │``  
`└─────────────────────────────────────────────────────────────────────────┘`

### **Moving Part 1: Central Hash Index (pulumi-index)**

> * **Type:** Static Git repository / Flake input.  
> * **Component:** index.json containing mappings of name \-\> version \-\> system \-\> SRI Hash.  
> * **Automation:** A daily GitHub Action that fetches release assets from all official pulumi/pulumi-\* repositories, parses SHA256SUMS, converts hashes to SRI format, and commits the updated index.json.

### **Moving Part 2: Plugin Store Synthesizer (Eliminates the $PATH Warning)**

Instead of forcing binaries onto $PATH, construct a directory layout mirroring Pulumi's internal plugin cache inside a single Nix derivation.

`# Synthesizes the exact directory hierarchy Pulumi expects`  
`mkPulumiPluginStore = { pkgs, providers, languageHosts }:`  
  `pkgs.linkFarm "pulumi-plugin-store" (`  
    `# 1. Map provider binaries: plugins/resource-aws-v6.10.0/pulumi-resource-aws`  
    `(builtins.map (p: {`  
      `name = "plugins/resource-${p.name}-v${p.version}/pulumi-resource-${p.name}";`  
      `path = "${p.pkg}/bin/pulumi-resource-${p.name}";`  
    `}) providers)`  
      
    `++`

    `# 2. Map language hosts: plugins/language-python-v3.0.0/pulumi-language-python`  
    `(builtins.map (l: {`  
      `name = "plugins/language-${l.name}-v${l.version}/pulumi-language-${l.name}";`  
      `path = "${l.pkg}/bin/pulumi-language-${l.name}";`  
    `}) languageHosts)`  
  `);`

### **Moving Part 3: Language Plugin Bridges (uv2nix Integration)**

> * **Python (uv2nix):** Inspects uv.lock, isolates pulumi-\* packages, and routes them:  
  * Python SDK wheels → Processed natively by uv2nix into the Python environment.  
  * Engine plugin demands → Passed to pulumi-index to fetch matching pulumi-resource-\* binaries.  
> * **Extensibility:** The library abstracts this into an engine mapper so you can swap uv2nix for go.mod (Go) or package-lock.json (Node.js) down the line without changing the binary resolution logic.

### **Moving Part 4: The CLI Wrapper (pulumi2nix Output)**

The wrapper sets Pulumi's environment variables to point directly to the synthetic Nix plugin store, silencing engine warnings completely:

`mkPulumiApp = { pkgs, pythonEnv, pluginStore }:`  
  `pkgs.writeShellApplication {`  
    `name = "pulumi";`  
    `runtimeInputs = [ pkgs.pulumi pythonEnv ];`  
    `text = ''`  
      `# Direct Pulumi to use our pure Nix plugin store directory`  
      `export PULUMI_HOME="${pluginStore}"`  
        
      `# Point to wrapped Python environment`  
      `export PULUMI_PYTHON_CMD="${pythonEnv}/bin/python"`

      `# Strictly disable dynamic, un-sandboxed plugin downloads over network`  
      `export PULUMI_DISABLE_AUTOMATIC_PLUGIN_ACQUISITION=true`

      `exec pulumi "$@"`  
    `'';`  
  `};`

### **Why This Design Works**

> 1. **No Warnings:** By setting PULUMI\_HOME to a synthesized linkFarm instead of appending raw paths to $PATH, the CLI thinks the plugins were installed natively by its own plugin manager.  
> 2. **Deterministic Parity:** uv.lock drives both the Python SDK version and the engine binary version via the index, making drift impossible.  
> 3. **Pure Execution:** Setting PULUMI\_DISABLE\_AUTOMATIC\_PLUGIN\_ACQUISITION=true guarantees builds fail instantly if a binary is missing rather than making silent, impure network calls.

---

