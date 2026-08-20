//! Hermetic behavior tests against a local mock of GitHub releases.
//! (BFS ordering and pure logic are unit-tested in lib.rs; the network e2e
//! in scripts/index-e2e.sh covers real releases.)

use pulumi2nix_index::{cmd_verify, cmd_walk, Shard, VerifyOpts, WalkOpts};
use sha2::Digest;
use std::path::Path;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PLATFORM: &str = "linux-amd64";

fn walk_opts(server: &MockServer, index_dir: &Path, providers: &[&str]) -> WalkOpts {
    WalkOpts {
        index_dir: index_dir.to_path_buf(),
        providers: providers.iter().map(|s| s.to_string()).collect(),
        providers_file: None,
        platforms: vec![PLATFORM.to_string()],
        max_artifacts: 100,
        max_seconds: 60.0,
        concurrency: 4,
        base: server.uri(),
    }
}

fn artifact_path(provider: &str, version: &str) -> String {
    format!(
        "/pulumi/pulumi-{provider}/releases/download/v{version}/pulumi-resource-{provider}-v{version}-{PLATFORM}.tar.gz"
    )
}

fn checksums_path(provider: &str, version: &str) -> String {
    format!(
        "/pulumi/pulumi-{provider}/releases/download/v{version}/pulumi-{provider}_{version}_checksums.txt"
    )
}

fn sri_of(bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(bytes))
    )
}

fn recorded(index_dir: &Path, provider: &str, version: &str) -> Option<Option<String>> {
    let shard = Shard::load(index_dir, provider).unwrap();
    shard
        .entries
        .get(version)
        .and_then(|e| e.hashes.get(PLATFORM).cloned())
}

#[tokio::test]
async fn streams_and_hashes_artifacts() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let body = b"artifact-bytes".to_vec();
    Mock::given(method("GET"))
        .and(path(checksums_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let rc = cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    assert_eq!(rc, 0);
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), Some(Some(sri_of(&body))));
}

#[tokio::test]
async fn sha256_checksums_file_skips_downloads() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let body = b"checksummed-bytes";
    let hex = hex::encode(sha2::Sha256::digest(body));
    let listing = format!(
        "{hex}  ./pulumi-resource-aaa-v1.0.0-{PLATFORM}.tar.gz\n"
    );
    Mock::given(method("GET"))
        .and(path(checksums_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_string(listing))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200))
        .expect(0) // the whole point: no tarball download
        .mount(&server)
        .await;

    let rc = cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    assert_eq!(rc, 0);
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), Some(Some(sri_of(body))));
}

#[tokio::test]
async fn missing_asset_stamps_miss_once_and_stays_retryable() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    Mock::given(method("GET"))
        .and(path(checksums_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404))
        .expect(2) // 404 is never a fact: a second walk retries
        .mount(&server)
        .await;

    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), None); // no hash, no null
    let shard = Shard::load(tmp.path(), "aaa").unwrap();
    let first_miss = shard.entries["1.0.0"].misses[PLATFORM].clone();

    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    let shard = Shard::load(tmp.path(), "aaa").unwrap();
    // Write-once: the retry does not touch the first-miss timestamp.
    assert_eq!(shard.entries["1.0.0"].misses[PLATFORM], first_miss);
}

#[tokio::test]
async fn miss_clears_when_asset_appears_and_404_costs_no_budget() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    for (provider, version) in [("aaa", "1.0.0"), ("bbb", "1.0.0")] {
        Mock::given(method("GET"))
            .and(path(checksums_path(provider, version)))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
    }
    // aaa's asset is missing (mid-publish); bbb's exists.
    let gone = Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404));
    let guard = server.register_as_scoped(gone).await;
    let body = b"late-arrival".to_vec();
    Mock::given(method("GET"))
        .and(path(artifact_path("bbb", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"other".to_vec()))
        .mount(&server)
        .await;

    // Budget of 1: the 404 on aaa must not consume it — bbb still gets hashed.
    let mut opts = walk_opts(&server, tmp.path(), &["aaa@1.0.0", "bbb@1.0.0"]);
    opts.max_artifacts = 1;
    cmd_walk(opts).await.unwrap();
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), None);
    assert!(recorded(tmp.path(), "bbb", "1.0.0").flatten().is_some());

    // The asset finishes publishing; the next walk heals the gap and clears
    // the miss bookkeeping.
    drop(guard);
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), Some(Some(sri_of(&body))));
    let shard = Shard::load(tmp.path(), "aaa").unwrap();
    assert!(shard.entries["1.0.0"].misses.is_empty());
}

#[tokio::test]
async fn walk_is_append_only_across_runs() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    Mock::given(method("GET"))
        .and(path(checksums_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"v1".to_vec()))
        .expect(1) // exactly one fetch across both runs
        .mount(&server)
        .await;

    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    let first = recorded(tmp.path(), "aaa", "1.0.0");
    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), first);
}

#[tokio::test]
async fn zero_time_budget_walks_nothing() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    // No mounts with expectations: any request would 404 via wiremock's
    // default, and the shard must stay empty.
    let mut opts = walk_opts(&server, tmp.path(), &["aaa@1.0.0"]);
    opts.max_seconds = 0.0;
    let rc = cmd_walk(opts).await.unwrap();
    assert_eq!(rc, 0);
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), None);
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn verify_detects_drift_and_records_conflict_without_rewriting() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    Mock::given(method("GET"))
        .and(path(checksums_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let original = b"original".to_vec();
    let artifact = Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(original.clone()));
    let guard = server.register_as_scoped(artifact).await;
    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    drop(guard);

    // Upstream silently replaces the artifact.
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec()))
        .mount(&server)
        .await;

    let rc = cmd_verify(VerifyOpts {
        index_dir: tmp.path().to_path_buf(),
        sample: 10,
        base: server.uri(),
    })
    .await
    .unwrap();
    assert_eq!(rc, 3);

    let conflicts: Vec<_> = std::fs::read_dir(tmp.path().join("conflicts"))
        .unwrap()
        .collect();
    assert_eq!(conflicts.len(), 1);
    // The index entry itself was NOT rewritten.
    assert_eq!(recorded(tmp.path(), "aaa", "1.0.0"), Some(Some(sri_of(&original))));
}

#[tokio::test]
async fn verify_passes_on_honest_index() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let body = b"stable".to_vec();
    Mock::given(method("GET"))
        .and(path(checksums_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(artifact_path("aaa", "1.0.0")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    cmd_walk(walk_opts(&server, tmp.path(), &["aaa@1.0.0"])).await.unwrap();
    let rc = cmd_verify(VerifyOpts {
        index_dir: tmp.path().to_path_buf(),
        sample: 10,
        base: server.uri(),
    })
    .await
    .unwrap();
    assert_eq!(rc, 0);
}
