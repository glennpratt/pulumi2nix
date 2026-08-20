//! Breadth-first backfill walker for a pulumi-nix-index repo (async Rust).
//!
//! Contract-compatible with the original Python walker: same CLI shape, same
//! shard JSON (`index/<provider>.json`: version -> platform -> SRI hash,
//! null = release exists but that platform's asset doesn't), same log line
//! formats (the language-agnostic e2e parses them), same exit codes
//! (3 = drift detected by `verify`).
//!
//! Design properties (see PLAN.md):
//! - Stateless walk: frontier = ls-remote tags minus existing shards.
//! - Breadth-first: all providers' rank-0 versions before any rank-1;
//!   `name@version` pins jump the queue (rank -1).
//! - Budgeted by artifact count (successful hashes only) and wall clock;
//!   shards saved per version.
//! - No permanent conclusions from status codes: recorded hashes are never
//!   rewritten (drift detection lives in `verify`: conflicts/ + exit 3,
//!   never a silent update), and absence is never recorded as fact — a 404
//!   stamps a write-once `misses` timestamp used only for scheduling.
//!   Fresh misses retry at natural rank (a release may still be mid-
//!   publish); misses older than MISS_GRACE are demoted behind all fresh
//!   work, so permanent gaps cost one cheap request per deep-backfill pass
//!   instead of clogging rank 0. Transport/throttle errors (403/429/5xx)
//!   record nothing at all.
//! - Async where it pays: platform tarballs of a version are streamed and
//!   hashed concurrently, never buffered to disk or memory.

use anyhow::{bail, Context, Result};
use base64::Engine;
use futures::StreamExt;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SHARD_FORMAT_VERSION: u32 = 1;

/// How long after a first 404 a version keeps retrying at natural rank
/// (covers releases whose assets are still uploading when the tag appears).
pub const MISS_GRACE_SECS: i64 = 24 * 3600;

/// Rank offset that pushes stale-missing versions behind all fresh work.
pub const MISS_DEMOTION: i64 = 1_000_000;

pub const DEFAULT_PLATFORMS: [&str; 4] = [
    "linux-amd64",
    "linux-arm64",
    "darwin-amd64",
    "darwin-arm64",
];

pub const USER_AGENT: &str =
    "pulumi2nix-index/0.1 (+https://github.com/glennpratt/pulumi2nix)";

pub fn log(msg: &str) {
    eprintln!("{msg}");
}

// --- URL conventions (hash-only index: consumers re-derive these) ----------

pub fn default_base() -> String {
    std::env::var("PULUMI2NIX_RELEASE_BASE")
        .unwrap_or_else(|_| "https://github.com".to_string())
}

pub fn provider_repo_url(base: &str, provider: &str) -> String {
    format!("{base}/pulumi/pulumi-{provider}")
}

pub fn release_base_url(base: &str, provider: &str, version: &str) -> String {
    format!(
        "{}/releases/download/v{version}",
        provider_repo_url(base, provider)
    )
}

pub fn asset_name(provider: &str, version: &str, platform: &str) -> String {
    format!("pulumi-resource-{provider}-v{version}-{platform}.tar.gz")
}

pub fn checksums_url(base: &str, provider: &str, version: &str) -> String {
    format!(
        "{}/pulumi-{provider}_{version}_checksums.txt",
        release_base_url(base, provider, version)
    )
}

// --- hashing ----------------------------------------------------------------

pub fn sha256_sri(hex_digest: &str) -> Result<String> {
    let bytes = hex::decode(hex_digest).context("invalid hex digest")?;
    Ok(format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Parse `sha256hex  filename` lines into filename -> hex. Only sha256
/// (64 hex chars) entries are usable — many providers publish SHA1
/// checksums, which parse to nothing and force the streaming fallback.
pub fn parse_checksums_txt(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let [digest, filename] = parts[..] {
            if digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
                let name = filename.trim_start_matches('*').trim_start_matches("./");
                out.insert(name.to_string(), digest.to_ascii_lowercase());
            }
        }
    }
    out
}

pub enum FetchError {
    /// HTTP 404 — the asset looks unpublished. Never treated as fact: it
    /// stamps a miss timestamp used only to deprioritize retries.
    Missing,
    /// Anything else (403 throttles, 429, 5xx, transport) — says nothing
    /// about the asset; record nothing, retry next run.
    Other(String),
}

/// Stream a URL through sha256 without buffering it in memory or on disk.
pub async fn sha256_of_url(client: &reqwest::Client, url: &str) -> Result<String, FetchError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| FetchError::Other(e.to_string()))?;
    let status = resp.status();
    if status == 404 {
        return Err(FetchError::Missing);
    }
    if !status.is_success() {
        return Err(FetchError::Other(format!("HTTP {status}")));
    }
    let mut hasher = sha2::Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        hasher.update(&chunk.map_err(|e| FetchError::Other(e.to_string()))?);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn http_get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status());
    }
    Ok(resp.text().await?)
}

// --- version enumeration ----------------------------------------------------

pub fn version_key(version: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = version.split('.').collect();
    if let [a, b, c] = parts[..] {
        return Some((a.parse().ok()?, b.parse().ok()?, c.parse().ok()?));
    }
    None
}

/// All stable release versions of a provider, newest first, via
/// `git ls-remote --tags` — one request, no GitHub API quota. Prerelease and
/// otherwise-suffixed tags are skipped.
pub async fn list_versions(base: &str, provider: &str) -> Result<Vec<String>> {
    let out = tokio::process::Command::new("git")
        .args(["ls-remote", "--tags", &provider_repo_url(base, provider)])
        .output()
        .await
        .context("failed to run git ls-remote")?;
    if !out.status.success() {
        bail!(
            "git ls-remote failed for {provider}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut versions: Vec<(u64, u64, u64)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter_map(|r| r.strip_suffix("^{}").or(Some(r)))
        .filter_map(|r| r.strip_prefix("refs/tags/v"))
        .filter_map(version_key)
        .collect();
    versions.sort_unstable();
    versions.dedup();
    versions.reverse();
    Ok(versions
        .into_iter()
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .collect())
}

// --- shard I/O --------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Entry {
    #[serde(rename = "observedAt")]
    pub observed_at: Option<String>,
    /// platform -> SRI. Option only to tolerate legacy null entries (an
    /// earlier format recorded 404s as null); the walker never writes None
    /// and treats a legacy null as "missing, retryable".
    #[serde(default)]
    pub hashes: BTreeMap<String, Option<String>>,
    /// platform -> RFC3339 timestamp of the first observed 404. Written
    /// once, removed when the asset appears; scheduling metadata only —
    /// never a statement that the asset doesn't exist.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub misses: BTreeMap<String, String>,
}

impl Entry {
    pub fn has_hash(&self, platform: &str) -> bool {
        matches!(self.hashes.get(platform), Some(Some(_)))
    }
}

#[derive(Serialize, Deserialize)]
pub struct Shard {
    pub version: u32,
    pub provider: String,
    pub entries: IndexMap<String, Entry>,
}

impl Shard {
    pub fn path(index_dir: &Path, provider: &str) -> PathBuf {
        index_dir.join("index").join(format!("{provider}.json"))
    }

    pub fn load(index_dir: &Path, provider: &str) -> Result<Shard> {
        let path = Self::path(index_dir, provider);
        if path.exists() {
            let shard: Shard = serde_json::from_str(&std::fs::read_to_string(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            return Ok(shard);
        }
        Ok(Shard {
            version: SHARD_FORMAT_VERSION,
            provider: provider.to_string(),
            entries: IndexMap::new(),
        })
    }

    pub fn save(&mut self, index_dir: &Path) -> Result<()> {
        self.entries
            .sort_by(|va, _, vb, _| version_key(vb).cmp(&version_key(va)));
        let path = Self::path(index_dir, &self.provider);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn is_complete(&self, version: &str, platforms: &[String]) -> bool {
        match self.entries.get(version) {
            Some(e) => platforms.iter().all(|p| e.has_hash(p)),
            None => false,
        }
    }

    /// A version is demoted (retried only behind all fresh work) when every
    /// platform still lacking a hash 404'd longer ago than MISS_GRACE — by
    /// then a mid-publish release would have settled, so what's left is
    /// almost certainly a platform that was never shipped. Any never-tried
    /// or freshly-missed platform keeps the version at its natural rank.
    pub fn is_demoted(&self, version: &str, platforms: &[String],
                      now: &chrono::DateTime<chrono::Utc>) -> bool {
        let Some(entry) = self.entries.get(version) else { return false };
        let mut unhashed = platforms.iter().filter(|p| !entry.has_hash(p));
        unhashed.all(|p| match entry.misses.get(p) {
            Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(t) => (*now - t.with_timezone(&chrono::Utc)).num_seconds() > MISS_GRACE_SECS,
                Err(_) => false, // unparseable: retry at natural rank
            },
            None => false,
        })
    }
}

pub fn record_conflict(
    index_dir: &Path,
    provider: &str,
    version: &str,
    platform: &str,
    existing: &str,
    observed: &str,
) -> Result<()> {
    let dir = index_dir.join("conflicts");
    std::fs::create_dir_all(&dir)?;
    let report = serde_json::json!({
        "provider": provider,
        "version": version,
        "platform": platform,
        "existing": existing,
        "observed": observed,
        "observedAt": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(
        dir.join(format!("{provider}-{version}-{platform}.json")),
        serde_json::to_string_pretty(&report)? + "\n",
    )?;
    Ok(())
}

// --- walk -------------------------------------------------------------------

pub struct Budget {
    pub max_artifacts: usize,
    pub deadline: Instant,
    pub spent: usize,
}

impl Budget {
    pub fn new(max_artifacts: usize, max_seconds: f64) -> Budget {
        Budget {
            max_artifacts,
            deadline: Instant::now() + Duration::from_secs_f64(max_seconds),
            spent: 0,
        }
    }
    pub fn exhausted(&self) -> bool {
        self.spent >= self.max_artifacts || Instant::now() >= self.deadline
    }
    pub fn charge(&mut self) {
        self.spent += 1;
    }
}

/// Frontier of incomplete (rank, provider, version), breadth-first: every
/// provider's rank N before any rank N+1. Pinned versions get rank -1 and
/// are never demoted (an explicit request retries immediately); versions
/// whose remaining gaps are all stale misses sink behind all fresh work.
pub fn build_frontier(
    pinned: &[(String, String)],
    enumerated: &[(String, Vec<String>)],
    shards: &IndexMap<String, Shard>,
    platforms: &[String],
    now: &chrono::DateTime<chrono::Utc>,
) -> Vec<(i64, String, String)> {
    let mut frontier: Vec<(i64, String, String)> = Vec::new();
    for (provider, version) in pinned {
        if !shards[provider].is_complete(version, platforms) {
            frontier.push((-1, provider.clone(), version.clone()));
        }
    }
    for (provider, versions) in enumerated {
        for (rank, version) in versions.iter().enumerate() {
            let shard = &shards[provider];
            if !shard.is_complete(version, platforms) {
                let demotion = if shard.is_demoted(version, platforms, now) {
                    MISS_DEMOTION
                } else {
                    0
                };
                frontier.push((rank as i64 + demotion, provider.clone(), version.clone()));
            }
        }
    }
    frontier.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    frontier
}

/// Fill in missing platform hashes for one version. Recorded hashes are
/// never touched; every gap is retryable. A 404 stamps a write-once miss
/// timestamp (scheduling metadata for demotion) and charges nothing —
/// only successfully hashed tarballs consume the artifact budget.
pub async fn resolve_version(
    client: &reqwest::Client,
    base: &str,
    shard: &mut Shard,
    version: &str,
    platforms: &[String],
    budget: &mut Budget,
    concurrency: usize,
    now: &chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let provider = shard.provider.clone();
    let entry = shard.entries.entry(version.to_string()).or_default();

    let checksums = match http_get_text(client, &checksums_url(base, &provider, version)).await {
        Ok(text) => parse_checksums_txt(&text),
        Err(_) => BTreeMap::new(),
    };

    let mut fetches: Vec<(String, String)> = Vec::new(); // (platform, url)
    for platform in platforms {
        if entry.has_hash(platform) || budget.exhausted() {
            continue;
        }
        let asset = asset_name(&provider, version, platform);
        if let Some(hexdigest) = checksums.get(&asset) {
            let sri = sha256_sri(hexdigest)?;
            log(&format!("  {provider} v{version} {platform}: {sri}"));
            entry.hashes.insert(platform.clone(), Some(sri));
            entry.misses.remove(platform);
            continue;
        }
        let url = format!("{}/{asset}", release_base_url(base, &provider, version));
        fetches.push((platform.clone(), url));
    }

    let results: Vec<(String, Result<String, FetchError>)> =
        futures::stream::iter(fetches.into_iter().map(|(platform, url)| async move {
            let res = sha256_of_url(client, &url).await;
            (platform, res)
        }))
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    for (platform, res) in results {
        match res {
            Ok(hexdigest) => {
                let sri = sha256_sri(&hexdigest)?;
                log(&format!("  {provider} v{version} {platform}: {sri}"));
                entry.hashes.insert(platform.clone(), Some(sri));
                entry.misses.remove(&platform);
                budget.charge();
            }
            Err(FetchError::Missing) => {
                log(&format!("  {provider} v{version} {platform}: no asset (HTTP 404), will retry later"));
                entry
                    .misses
                    .entry(platform)
                    .or_insert_with(|| now.to_rfc3339());
            }
            Err(FetchError::Other(e)) => {
                log(&format!("  {provider} v{version} {platform}: {e}, will retry next run"));
            }
        }
    }

    if entry.observed_at.is_none() && !entry.hashes.is_empty() {
        entry.observed_at = Some(now.to_rfc3339());
    }
    Ok(())
}

pub struct WalkOpts {
    pub index_dir: PathBuf,
    pub providers: Vec<String>,
    pub providers_file: Option<PathBuf>,
    pub platforms: Vec<String>,
    pub max_artifacts: usize,
    pub max_seconds: f64,
    pub concurrency: usize,
    pub base: String,
}

pub async fn cmd_walk(opts: WalkOpts) -> Result<i32> {
    let platforms: Vec<String> = if opts.platforms.is_empty() {
        DEFAULT_PLATFORMS.iter().map(|s| s.to_string()).collect()
    } else {
        opts.platforms.clone()
    };

    let mut providers: Vec<String> = opts.providers.clone();
    if let Some(file) = &opts.providers_file {
        for line in std::fs::read_to_string(file)?.lines() {
            let name = line.split('#').next().unwrap_or("").trim();
            if !name.is_empty() {
                providers.push(name.to_string());
            }
        }
    }
    if providers.is_empty() {
        log("No providers given (use --provider and/or --providers-file)");
        return Ok(2);
    }

    // `name@version` pins one exact version (demand lane) and jumps every
    // BFS layer; bare names get the full breadth-first enumeration.
    let mut pinned: Vec<(String, String)> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    for p in providers {
        match p.split_once('@') {
            Some((name, ver)) => pinned.push((
                name.to_string(),
                ver.trim_start_matches('v').to_string(),
            )),
            None => plain.push(p),
        }
    }

    let mut shards: IndexMap<String, Shard> = IndexMap::new();
    for (name, _) in &pinned {
        if !shards.contains_key(name) {
            shards.insert(name.clone(), Shard::load(&opts.index_dir, name)?);
        }
    }
    let mut enumerated: Vec<(String, Vec<String>)> = Vec::new();
    for provider in &plain {
        if !shards.contains_key(provider) {
            shards.insert(provider.clone(), Shard::load(&opts.index_dir, provider)?);
        }
        match list_versions(&opts.base, provider).await {
            Ok(versions) => {
                log(&format!("{provider}: {} release versions", versions.len()));
                enumerated.push((provider.clone(), versions));
            }
            Err(e) => log(&format!("{provider}: failed to enumerate tags ({e}), skipping this run")),
        }
    }

    let now = chrono::Utc::now();
    let frontier = build_frontier(&pinned, &enumerated, &shards, &platforms, &now);
    log(&format!(
        "Frontier: {} incomplete versions; budget {} artifacts / {}s",
        frontier.len(),
        opts.max_artifacts,
        opts.max_seconds
    ));

    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
    let mut budget = Budget::new(opts.max_artifacts, opts.max_seconds);
    for (_rank, provider, version) in &frontier {
        if budget.exhausted() {
            break;
        }
        let shard = shards.get_mut(provider).unwrap();
        resolve_version(
            &client,
            &opts.base,
            shard,
            version,
            &platforms,
            &mut budget,
            opts.concurrency,
            &now,
        )
        .await?;
        shard.save(&opts.index_dir)?;
    }

    log(&format!("Done: {} artifacts hashed this run", budget.spent));
    Ok(0)
}

// --- verify -----------------------------------------------------------------

pub struct VerifyOpts {
    pub index_dir: PathBuf,
    pub sample: usize,
    pub base: String,
}

pub async fn cmd_verify(opts: VerifyOpts) -> Result<i32> {
    let mut population: Vec<(String, String, String, String)> = Vec::new();
    let index = opts.index_dir.join("index");
    if index.is_dir() {
        for path in std::fs::read_dir(&index)? {
            let path = path?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let shard: Shard = serde_json::from_str(&std::fs::read_to_string(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            for (version, entry) in &shard.entries {
                for (platform, sri) in &entry.hashes {
                    if let Some(sri) = sri {
                        population.push((
                            shard.provider.clone(),
                            version.clone(),
                            platform.clone(),
                            sri.clone(),
                        ));
                    }
                }
            }
        }
    }
    if population.is_empty() {
        log("Index is empty; nothing to verify");
        return Ok(0);
    }

    use rand::seq::SliceRandom;
    population.shuffle(&mut rand::thread_rng());
    let sample = &population[..opts.sample.min(population.len())];
    log(&format!(
        "Re-verifying {} of {} entries",
        sample.len(),
        population.len()
    ));

    let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;
    let mut mismatches = 0;
    for (provider, version, platform, recorded) in sample {
        let url = format!(
            "{}/{}",
            release_base_url(&opts.base, provider, version),
            asset_name(provider, version, platform)
        );
        match sha256_of_url(&client, &url).await {
            Ok(hexdigest) => {
                let observed = sha256_sri(&hexdigest)?;
                if &observed != recorded {
                    log(&format!(
                        "  DRIFT: {provider} v{version} {platform}: index has {recorded}, observed {observed}"
                    ));
                    record_conflict(&opts.index_dir, provider, version, platform, recorded, &observed)?;
                    mismatches += 1;
                } else {
                    log(&format!("  ok: {provider} v{version} {platform}"));
                }
            }
            Err(FetchError::Missing) => {
                log(&format!(
                    "  {provider} v{version} {platform}: unavailable (HTTP 404) — investigate"
                ));
                mismatches += 1;
            }
            Err(FetchError::Other(e)) => {
                log(&format!(
                    "  {provider} v{version} {platform}: unavailable ({e}) — investigate"
                ));
                mismatches += 1;
            }
        }
    }

    if mismatches > 0 {
        log(&format!(
            "{mismatches} entries FAILED re-verification — see conflicts/"
        ));
        return Ok(3);
    }
    Ok(0)
}

// --- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn shard_with(provider: &str) -> Shard {
        Shard {
            version: SHARD_FORMAT_VERSION,
            provider: provider.to_string(),
            entries: IndexMap::new(),
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    #[test]
    fn frontier_is_breadth_first_with_pins_up_front() {
        let platforms = vec!["linux-amd64".to_string()];
        let mut shards = IndexMap::new();
        shards.insert("aaa".to_string(), shard_with("aaa"));
        shards.insert("bbb".to_string(), shard_with("bbb"));
        let enumerated = vec![
            ("aaa".to_string(), vec!["2.0.0".into(), "1.0.0".into()]),
            ("bbb".to_string(), vec!["1.5.0".into(), "1.4.0".into()]),
        ];
        let pinned = vec![("aaa".to_string(), "1.0.0".to_string())];
        let frontier = build_frontier(&pinned, &enumerated, &shards, &platforms, &now());
        let order: Vec<(i64, &str, &str)> = frontier
            .iter()
            .map(|(r, p, v)| (*r, p.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                (-1, "aaa", "1.0.0"),
                (0, "aaa", "2.0.0"),
                (0, "bbb", "1.5.0"),
                (1, "aaa", "1.0.0"),
                (1, "bbb", "1.4.0"),
            ]
        );
    }

    #[test]
    fn frontier_skips_complete_versions() {
        let platforms = vec!["linux-amd64".to_string()];
        let mut shard = shard_with("aaa");
        let mut entry = Entry::default();
        entry.hashes.insert("linux-amd64".into(), Some("sha256-x".into()));
        shard.entries.insert("2.0.0".to_string(), entry);
        let mut shards = IndexMap::new();
        shards.insert("aaa".to_string(), shard);
        let enumerated = vec![("aaa".to_string(), vec!["2.0.0".into(), "1.0.0".into()])];
        let frontier = build_frontier(&[], &enumerated, &shards, &platforms, &now());
        assert_eq!(frontier.len(), 1);
        assert_eq!(frontier[0].2, "1.0.0");
    }

    #[test]
    fn legacy_null_hash_is_retryable() {
        let platforms = vec!["linux-amd64".to_string()];
        let mut shard = shard_with("aaa");
        let mut entry = Entry::default();
        entry.hashes.insert("linux-amd64".into(), None);
        shard.entries.insert("1.0.0".to_string(), entry);
        assert!(!shard.is_complete("1.0.0", &platforms));
        // No miss timestamp for it either, so it retries at natural rank.
        assert!(!shard.is_demoted("1.0.0", &platforms, &now()));
    }

    #[test]
    fn stale_misses_demote_fresh_misses_do_not() {
        let platforms = vec!["linux-amd64".to_string(), "linux-arm64".to_string()];
        let mut shard = shard_with("aaa");
        let mut entry = Entry::default();
        entry.hashes.insert("linux-amd64".into(), Some("sha256-x".into()));
        let stale = (now() - chrono::Duration::seconds(MISS_GRACE_SECS + 60)).to_rfc3339();
        entry.misses.insert("linux-arm64".into(), stale);
        shard.entries.insert("2.0.0".to_string(), entry.clone());
        assert!(shard.is_demoted("2.0.0", &platforms, &now()));

        // Same shape but the miss is fresh (mid-publish window): no demotion.
        entry.misses.insert("linux-arm64".into(), now().to_rfc3339());
        shard.entries.insert("1.0.0".to_string(), entry);
        assert!(!shard.is_demoted("1.0.0", &platforms, &now()));

        // Demoted versions sort behind every natural-rank version.
        let mut shards = IndexMap::new();
        shards.insert("aaa".to_string(), shard);
        let enumerated = vec![(
            "aaa".to_string(),
            vec!["2.0.0".into(), "1.0.0".into()],
        )];
        let frontier = build_frontier(&[], &enumerated, &shards, &platforms, &now());
        let versions: Vec<&str> = frontier.iter().map(|(_, _, v)| v.as_str()).collect();
        assert_eq!(versions, vec!["1.0.0", "2.0.0"]);
        assert!(frontier[1].0 >= MISS_DEMOTION);
    }

    #[test]
    fn checksum_parsing_matches_python_walker() {
        let text = "\
f9da7477b282cff6961289719a705cacd4df26c9  ./sha1-ignored.tar.gz
0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  ./kept.tar.gz
ABCDEF6789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  *starred.tar.gz
";
        let parsed = parse_checksums_txt(text);
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains_key("kept.tar.gz"));
        assert_eq!(
            parsed["starred.tar.gz"],
            "abcdef6789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn sri_conversion() {
        // Golden pair from the committed examples/random/pulumi-lock.json
        // (random 4.21.1 linux-amd64), cross-checked with the Python tool.
        assert_eq!(
            sha256_sri("ca22a2ef599179777130e6bf5ff0facb15a28da3ab06a36634e7cff3b0118708")
                .unwrap(),
            "sha256-yiKi71mReXdxMOa/X/D6yxWijaOrBqNmNOfP87ARhwg="
        );
    }

    #[test]
    fn version_ordering() {
        let mut versions = vec!["9.0.0", "10.0.0", "9.10.0", "9.9.9"];
        versions.sort_by_key(|v| std::cmp::Reverse(version_key(v)));
        assert_eq!(versions, vec!["10.0.0", "9.10.0", "9.9.9", "9.0.0"]);
        assert_eq!(version_key("1.2.3-alpha"), None);
        assert_eq!(version_key("1.2"), None);
    }
}
