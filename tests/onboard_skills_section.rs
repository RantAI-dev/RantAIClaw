// MutexGuard held across .await — intentional, serializes HOME-env mutation
// against parallel test workers.
#![allow(clippy::await_holding_lock)]

//! Integration tests for Wave 2D — bundled starter pack and ClawHub
//! list_top caching. See `docs/superpowers/plans/2026-04-27-onboarding-depth-v2.md`,
//! Task 2D.5.
//!
//! Tests redirect `$HOME` to a per-test `tempfile::TempDir` so they never
//! touch the real `~/.rantaiclaw`. The same `Mutex` pattern used in
//! `tests/profile_lifecycle.rs` serializes the suite — `set_var("HOME",
//! ...)` is process-global and Cargo runs tests in parallel by default.

use std::sync::Mutex;
use std::time::Duration;

use rantaiclaw::profile::ProfileManager;
use rantaiclaw::skills::bundled::{self, STARTER_PACK};
use rantaiclaw::skills::clawhub::{self, CLAWHUB_BASE_URL_ENV};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `$HOME` pointed at a fresh tempdir. Restores afterwards.
fn with_home<F: FnOnce()>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let prev_home = std::env::var_os("HOME");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    let prev_clawhub = std::env::var_os(CLAWHUB_BASE_URL_ENV);
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");
    std::env::remove_var(CLAWHUB_BASE_URL_ENV);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(p) = prev_profile {
        std::env::set_var("RANTAICLAW_PROFILE", p);
    } else {
        std::env::remove_var("RANTAICLAW_PROFILE");
    }
    if let Some(c) = prev_clawhub {
        std::env::set_var(CLAWHUB_BASE_URL_ENV, c);
    } else {
        std::env::remove_var(CLAWHUB_BASE_URL_ENV);
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ---------------------------------------------------------------------------
// install_starter_pack
// ---------------------------------------------------------------------------

#[test]
fn install_starter_pack_creates_5_skill_dirs() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().expect("ensure default");
        let installed = bundled::install_starter_pack(&profile).expect("install");
        assert_eq!(installed.len(), 5, "expected 5 newly-installed skills");
        for s in STARTER_PACK {
            let dir = profile.skills_dir().join(s.slug);
            assert!(dir.is_dir(), "{:?} not created", dir);
            let md = dir.join("SKILL.md");
            assert!(md.is_file(), "{:?} missing", md);
            let content = std::fs::read_to_string(&md).unwrap();
            assert!(
                content.trim_start().starts_with('#'),
                "{} SKILL.md should start with a markdown heading",
                s.slug
            );
        }
    });
}

#[test]
fn install_starter_pack_is_idempotent() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().expect("ensure default");
        let first = bundled::install_starter_pack(&profile).expect("install 1");
        assert_eq!(first.len(), 5);

        // Second invocation should be a no-op — the function must NOT
        // overwrite or re-create any of the existing directories.
        let second = bundled::install_starter_pack(&profile).expect("install 2");
        assert!(
            second.is_empty(),
            "second install should report no new slugs, got {:?}",
            second
        );

        // All 5 skills still on disk.
        for s in STARTER_PACK {
            assert!(profile.skills_dir().join(s.slug).is_dir());
        }
    });
}

#[test]
fn install_starter_pack_does_not_overwrite_existing() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().expect("ensure default");

        // Pre-create one of the starter-pack dirs with custom user content.
        let existing = profile.skills_dir().join("web-search");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("SKILL.md"), "# user-customised\n").unwrap();
        std::fs::write(existing.join("notes.txt"), "private notes").unwrap();

        let installed = bundled::install_starter_pack(&profile).expect("install");
        // 4 new (web-search was skipped).
        assert_eq!(installed.len(), 4);
        assert!(!installed.iter().any(|s| s == "web-search"));

        // User content preserved verbatim.
        let md = std::fs::read_to_string(existing.join("SKILL.md")).unwrap();
        assert_eq!(md, "# user-customised\n", "user SKILL.md was overwritten");
        let notes = std::fs::read_to_string(existing.join("notes.txt")).unwrap();
        assert_eq!(notes, "private notes", "user notes were overwritten");

        // Other 4 skills did install.
        for s in STARTER_PACK.iter().filter(|s| s.slug != "web-search") {
            assert!(profile.skills_dir().join(s.slug).is_dir());
        }
    });
}

// ---------------------------------------------------------------------------
// clawhub::list_top — mock HTTP server
// ---------------------------------------------------------------------------

/// Counts requests against the mock server so we can assert the second
/// `list_top` call hits the disk cache rather than the network.
#[derive(Default)]
struct MockState {
    requests: std::sync::atomic::AtomicUsize,
    paths: std::sync::Mutex<Vec<String>>,
}

/// Spawn a single-shot HTTP server that responds to GET requests with a
/// canned ClawHub listing. Returns `(base_url, state, shutdown_handle)`.
async fn spawn_mock_clawhub() -> (
    String,
    std::sync::Arc<MockState>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = std::sync::Arc::new(MockState::default());
    let state_clone = state.clone();

    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let st = state_clone.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = match sock.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                st.requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                st.paths.lock().unwrap().push(path);

                let body = serde_json::json!({
                    "items": [
                        {"slug": "alpha", "displayName": "Alpha", "summary": "first",
                         "stats": {"stars": 99, "downloads": 10}},
                        {"slug": "beta", "displayName": "Beta", "summary": "second",
                         "stats": {"stars": 42, "downloads": 5}},
                    ]
                });
                let body = body.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let base = format!("http://{}", addr);
    (base, state, handle)
}

#[tokio::test(flavor = "current_thread")]
async fn clawhub_list_top_uses_stars_sort_and_caches() {
    // Re-use the with_home pattern but for an async block: lock + temp HOME
    // + run the future on the current single-threaded reactor.
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let prev_home = std::env::var_os("HOME");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    let prev_clawhub = std::env::var_os(CLAWHUB_BASE_URL_ENV);
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");

    // Spawn the mock and point the client at it for this test only.
    let (base, state, handle) = spawn_mock_clawhub().await;
    std::env::set_var(CLAWHUB_BASE_URL_ENV, &base);

    // First call → network.
    let first = clawhub::list_top(20).await.expect("list_top first");
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].slug, "alpha");
    assert_eq!(first[0].stats.stars, 99);
    assert_eq!(first[1].slug, "beta");
    assert_eq!(state.requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        state.paths.lock().unwrap()[0].contains("sort=stars"),
        "request must include sort=stars; saw {:?}",
        state.paths.lock().unwrap()
    );

    // Cache file written.
    let cache = tmp
        .path()
        .join(".rantaiclaw")
        .join("cache")
        .join("clawhub")
        .join("top-skills.json");
    assert!(cache.exists(), "expected cache file at {:?}", cache);

    // Second call → cache hit, no new HTTP request.
    let second = clawhub::list_top(20).await.expect("list_top second");
    assert_eq!(second.len(), 2);
    assert_eq!(
        state.requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "second call should hit the cache, not the network"
    );

    // `n` honoured.
    let limited = clawhub::list_top(1).await.expect("list_top limited");
    assert_eq!(limited.len(), 1);

    // Cleanup
    handle.abort();
    let _ = tokio::time::timeout(Duration::from_millis(50), handle).await;

    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(p) = prev_profile {
        std::env::set_var("RANTAICLAW_PROFILE", p);
    } else {
        std::env::remove_var("RANTAICLAW_PROFILE");
    }
    if let Some(c) = prev_clawhub {
        std::env::set_var(CLAWHUB_BASE_URL_ENV, c);
    } else {
        std::env::remove_var(CLAWHUB_BASE_URL_ENV);
    }
}

/// Mock that responds to the *three* endpoints `install_one` walks:
///   - `/skills/<slug>`                       → metadata with latestVersion.version
///   - `/skills/<slug>/versions/<v>`          → manifest with files[*]
///   - `/skills/<slug>/versions/<v>/files/<p>` → raw file body
///
/// Returns base url + a thread-shared state struct so tests can introspect
/// what got requested.
async fn spawn_mock_clawhub_full(
    skill_md_body: Vec<u8>,
    skill_md_sha: String,
) -> (
    String,
    std::sync::Arc<MockState>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = std::sync::Arc::new(MockState::default());
    let state_clone = state.clone();
    let body = std::sync::Arc::new((skill_md_body, skill_md_sha));

    let handle = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let st = state_clone.clone();
            let payload = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = match sock.read(&mut buf).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                st.requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                st.paths.lock().unwrap().push(path.clone());

                let (status, ctype, body): (&str, &str, Vec<u8>) =
                    if path.contains("/files/SKILL.md") {
                        ("200 OK", "text/markdown", payload.0.clone())
                    } else if path.contains("/versions/") {
                        let manifest = serde_json::json!({
                            "version": {
                                "version": "1.0.0",
                                "files": [{
                                    "path": "SKILL.md",
                                    "size": payload.0.len(),
                                    "sha256": payload.1,
                                    "contentType": "text/markdown",
                                }]
                            }
                        });
                        (
                            "200 OK",
                            "application/json",
                            manifest.to_string().into_bytes(),
                        )
                    } else {
                        // /skills/<slug> detail
                        let detail = serde_json::json!({
                            "skill": {"slug": "demo"},
                            "latestVersion": {"version": "1.0.0"}
                        });
                        (
                            "200 OK",
                            "application/json",
                            detail.to_string().into_bytes(),
                        )
                    };

                let header = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status, ctype, body.len()
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let base = format!("http://{}", addr);
    (base, state, handle)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[tokio::test(flavor = "current_thread")]
async fn clawhub_install_one_writes_real_skill_body() {
    // Regression test for the audit finding 2026-04-29 §7.2: the old
    // install_one only ever wrote a placeholder "# slug\nInstalled from..."
    // because it read a non-existent `latestVersion.readme` field. The new
    // implementation walks `version.files`, fetches each, verifies sha256.
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let prev_home = std::env::var_os("HOME");
    let prev_clawhub = std::env::var_os(CLAWHUB_BASE_URL_ENV);
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");

    let body = b"# demo skill\n\nReal SKILL.md content from the manifest.\n".to_vec();
    let body_sha = sha256_hex(&body);
    let (base, state, handle) = spawn_mock_clawhub_full(body.clone(), body_sha).await;
    std::env::set_var(CLAWHUB_BASE_URL_ENV, &base);

    let profile = ProfileManager::ensure_default().unwrap();
    clawhub::install_one(&profile, "demo")
        .await
        .expect("install_one succeeds");

    // Three GETs: detail → version manifest → file body.
    assert_eq!(
        state.requests.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "expected detail + version + file fetch; saw {:?}",
        state.paths.lock().unwrap()
    );

    let installed = profile.skills_dir().join("demo").join("SKILL.md");
    let content = std::fs::read(&installed).expect("SKILL.md must exist");
    assert_eq!(content, body, "installed SKILL.md must match upstream body");
    assert!(
        !String::from_utf8_lossy(&content).starts_with("# demo skill\n\nReal") == false,
        "content {:?}",
        String::from_utf8_lossy(&content)
    );

    handle.abort();
    let _ = tokio::time::timeout(Duration::from_millis(50), handle).await;
    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(c) = prev_clawhub {
        std::env::set_var(CLAWHUB_BASE_URL_ENV, c);
    } else {
        std::env::remove_var(CLAWHUB_BASE_URL_ENV);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clawhub_install_one_aborts_on_sha_mismatch_and_cleans_up() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let prev_home = std::env::var_os("HOME");
    let prev_clawhub = std::env::var_os(CLAWHUB_BASE_URL_ENV);
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");

    // Manifest claims this body is some other content; mock returns the
    // real bytes anyway, so the verifier should reject and abort.
    let real = b"actual content".to_vec();
    let bogus_sha = "0".repeat(64);
    let (base, _state, handle) = spawn_mock_clawhub_full(real, bogus_sha).await;
    std::env::set_var(CLAWHUB_BASE_URL_ENV, &base);

    let profile = ProfileManager::ensure_default().unwrap();
    let err = clawhub::install_one(&profile, "demo")
        .await
        .expect_err("sha mismatch must error");
    assert!(
        err.to_string().contains("sha256") || err.chain().any(|e| e.to_string().contains("sha256")),
        "expected sha256 error chain, got {err:?}",
    );
    assert!(
        !profile.skills_dir().join("demo").exists(),
        "partial install should be cleaned up",
    );

    handle.abort();
    let _ = tokio::time::timeout(Duration::from_millis(50), handle).await;
    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(c) = prev_clawhub {
        std::env::set_var(CLAWHUB_BASE_URL_ENV, c);
    } else {
        std::env::remove_var(CLAWHUB_BASE_URL_ENV);
    }
}
