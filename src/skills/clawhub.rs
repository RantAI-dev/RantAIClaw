//! ClawHub API client — list and install skills from `clawhub.ai`.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Skills bootstrap" — `list_top(n)` is the data source for the wizard's
//! ClawHub multi-select picker; `install_many` is invoked after the user
//! confirms the selection.
//!
//! The cache lives at `~/.rantaiclaw/cache/clawhub/top-skills.json` and is
//! profile-agnostic (the catalog itself does not depend on the active
//! profile). 24h TTL — the wizard is one-shot UX, network hiccups are
//! tolerated by serving stale cache rather than blocking the user.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::profile::Profile;

/// Public override for the API base URL — set by tests so they can point
/// `list_top` at a local mock server. Production code never sets this.
pub const CLAWHUB_BASE_URL_ENV: &str = "RANTAICLAW_CLAWHUB_BASE_URL";
const DEFAULT_BASE_URL: &str = "https://clawhub.ai/api/v1";
const TOP_SKILLS_CACHE_FILE: &str = "top-skills.json";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

/// Subset of the ClawHub skill listing item that the wizard cares about.
/// Extra fields in the payload are ignored (serde default behaviour).
///
/// Note on `tags`: upstream returns a `{ "latest": "version-string" }`
/// object, not a list of strings. We model it as a free-form
/// [`serde_json::Value`] so deserialization succeeds — the picker only
/// uses `display_name`, `summary`, and `stats.stars` for labelling, so
/// the tags shape isn't load-bearing yet, but losing it silently would
/// have masked future surprises.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClawHubSkill {
    pub slug: String,
    #[serde(default, alias = "displayName", alias = "display_name")]
    pub display_name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub stats: ClawHubStats,
    #[serde(default)]
    pub tags: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClawHubStats {
    #[serde(default)]
    pub stars: u64,
    #[serde(default)]
    pub downloads: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct ClawHubListing {
    #[serde(default)]
    items: Vec<ClawHubSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    fetched_at: u64,
    items: Vec<ClawHubSkill>,
}

fn base_url() -> String {
    std::env::var(CLAWHUB_BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

fn cache_path() -> PathBuf {
    crate::profile::paths::rantaiclaw_root()
        .join("cache")
        .join("clawhub")
        .join(TOP_SKILLS_CACHE_FILE)
}

fn read_cache(max_age: Duration) -> Option<Vec<ClawHubSkill>> {
    let path = cache_path();
    let raw = std::fs::read(&path).ok()?;
    let env: CacheEnvelope = serde_json::from_slice(&raw).ok()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if now.saturating_sub(env.fetched_at) > max_age.as_secs() {
        return None;
    }
    Some(env.items)
}

fn write_cache(items: &[ClawHubSkill]) -> Result<()> {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create cache dir {}", parent.display()))?;
    }
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let env = CacheEnvelope {
        fetched_at: now,
        items: items.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&env)?;
    std::fs::write(&path, json).with_context(|| format!("write cache {}", path.display()))?;
    Ok(())
}

/// Fetch the top-`n` skills from ClawHub, sorted by stars. Cached for 24h
/// at `~/.rantaiclaw/cache/clawhub/top-skills.json`.
///
/// The first call hits the network and writes the cache; subsequent calls
/// inside the TTL serve from disk. On network failure with a stale cache,
/// the stale cache is returned (best-effort UX) and the error is dropped.
pub async fn list_top(n: usize) -> Result<Vec<ClawHubSkill>> {
    if let Some(items) = read_cache(CACHE_TTL) {
        return Ok(items.into_iter().take(n).collect());
    }
    let url = format!("{}/skills?sort=stars", base_url());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build reqwest client")?;
    let response = client.get(&url).send().await.context("GET clawhub")?;
    if !response.status().is_success() {
        // Serve any stale cache rather than failing the wizard.
        if let Some(items) = read_cache(Duration::MAX) {
            return Ok(items.into_iter().take(n).collect());
        }
        anyhow::bail!("clawhub returned status {}", response.status());
    }
    let listing: ClawHubListing = response.json().await.context("parse clawhub listing")?;
    let _ = write_cache(&listing.items); // best-effort
    Ok(listing.items.into_iter().take(n).collect())
}

/// Install a batch of ClawHub skills by slug. Errors on individual slugs
/// are logged but do not abort the batch — the function returns the slugs
/// that successfully installed.
pub async fn install_many(profile: &Profile, slugs: &[String]) -> Result<Vec<String>> {
    let mut installed = Vec::new();
    for slug in slugs {
        match install_one(profile, slug).await {
            Ok(()) => installed.push(slug.clone()),
            Err(err) => {
                tracing::warn!(slug = slug.as_str(), error = %err, "clawhub install failed");
            }
        }
    }
    Ok(installed)
}

/// File entry returned by `GET /skills/:slug/versions/:v` (`version.files[*]`).
#[derive(Debug, Clone, Deserialize)]
struct VersionFile {
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default, alias = "contentType")]
    content_type: String,
}

/// Install a single ClawHub skill. The on-disk shape mirrors the existing
/// bundled-skills convention: a directory named after the slug containing
/// a `SKILL.md` plus any auxiliary files (assets/, README.md, etc.) that
/// shipped with the version.
///
/// Three-step fetch:
/// 1. `GET /skills/:slug` → resolve the latest version string.
/// 2. `GET /skills/:slug/versions/:v` → list `version.files[*]` (path + size + sha256).
/// 3. For each file: `GET /skills/:slug/file?version=:v&path=<path>` → write to disk,
///    verify SHA-256 against the manifest.
///
/// On any failure (network, hash mismatch, path traversal in the manifest)
/// the partially-written directory is removed so the install is all-or-nothing.
pub async fn install_one(profile: &Profile, slug: &str) -> Result<()> {
    validate_slug(slug)?;
    let dir = profile.skills_dir().join(slug);
    if dir.exists() {
        // Idempotent — leave existing user state alone. Callers wanting a
        // clean re-install should `fs::remove_dir_all` first.
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build reqwest client")?;

    let result = install_one_inner(&client, profile, slug, &dir).await;
    if result.is_err() {
        // Partial-install cleanup so the next attempt starts fresh and the
        // idempotent skip-if-exists guard above doesn't lock the user into
        // a broken state.
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

async fn install_one_inner(
    client: &reqwest::Client,
    _profile: &Profile,
    slug: &str,
    dir: &std::path::Path,
) -> Result<()> {
    // Step 1 — resolve latest version.
    let detail_url = format!("{}/skills/{}", base_url(), slug);
    let detail_resp = fetch_with_retry(client, &detail_url).await?;
    let detail: serde_json::Value = detail_resp
        .json()
        .await
        .context("parse clawhub skill detail")?;
    let version = detail
        .get("latestVersion")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("clawhub: no latestVersion.version for {slug}"))?
        .to_string();

    // Step 2 — fetch version manifest with file list.
    let version_url = format!("{}/skills/{}/versions/{}", base_url(), slug, version);
    let version_resp = fetch_with_retry(client, &version_url).await?;
    let version_body: serde_json::Value = version_resp
        .json()
        .await
        .context("parse clawhub version manifest")?;
    let files: Vec<VersionFile> = version_body
        .get("version")
        .and_then(|v| v.get("files"))
        .map(|f| serde_json::from_value(f.clone()))
        .transpose()
        .context("parse version.files")?
        .unwrap_or_default();

    if files.is_empty() {
        anyhow::bail!("clawhub: version {version} of {slug} has no files");
    }

    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    // Step 3 — fetch + verify each file. We ban any path that escapes the
    // skill's own directory; ClawHub manifests should already be safe, but
    // a malicious or buggy upstream cannot trick us into writing outside
    // `<profile>/skills/<slug>/`.
    for file in &files {
        let safe_rel = sanitize_relative_path(&file.path)
            .with_context(|| format!("reject manifest path {:?}", file.path))?;
        let target = dir.join(&safe_rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }

        // ClawHub serves file bytes via a singular `/file` endpoint with the
        // version and path as query parameters. The plural `/versions/:v/files/:path`
        // shape returns 404 — that was the v0.6.x install regression.
        let file_url = format!(
            "{}/skills/{}/file?version={}&path={}",
            base_url(),
            slug,
            urlencoding::encode(&version),
            urlencoding::encode(&file.path),
        );
        // SKILL.md is required; auxiliary files (README.md, LICENSE, etc.)
        // are best-effort. If the upstream manifest references a file that
        // 404s, treat non-SKILL.md as a warning so the install still
        // succeeds. This was the v0.6.1-alpha "Clawhub Error Install" bug
        // — a stale README.md reference broke the entire install.
        let is_required = file.path.eq_ignore_ascii_case("SKILL.md");
        let resp = match fetch_with_retry(client, &file_url).await {
            Ok(r) => r,
            Err(e) if !is_required => {
                tracing::warn!(
                    "clawhub: skipping optional file {} for {}/{}: {}",
                    file.path,
                    slug,
                    version,
                    e
                );
                continue;
            }
            Err(e) => return Err(e),
        };
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("download {}", file.path))?;

        if !file.sha256.is_empty() {
            verify_sha256(&bytes, &file.sha256)
                .with_context(|| format!("hash check failed for {}", file.path))?;
        }

        std::fs::write(&target, &bytes).with_context(|| format!("write {}", target.display()))?;
    }

    // Defensive: ClawHub manifests are required to ship a SKILL.md per the
    // bundled-skills format. If a version somehow lacks one, surface that.
    if !dir.join("SKILL.md").exists() {
        anyhow::bail!("clawhub: version {version} of {slug} has no SKILL.md");
    }

    Ok(())
}

/// Fetch with two retries on `429 Too Many Requests`, with capped exponential
/// backoff. ClawHub aggressively rate-limits the file endpoints; transparent
/// retry is the difference between "install succeeds" and "user gives up".
async fn fetch_with_retry(client: &reqwest::Client, url: &str) -> Result<reqwest::Response> {
    let mut backoff_ms = 500u64;
    for attempt in 1..=3 {
        let resp = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status.as_u16() == 429 && attempt < 3 {
            tracing::warn!(url, attempt, backoff_ms, "clawhub 429, backing off");
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(8_000);
            continue;
        }
        anyhow::bail!("clawhub returned status {status} for {url}");
    }
    unreachable!("loop exits via return or bail above")
}

/// Reject any relative path that contains parent-dir traversal, leading
/// slashes, root anchors, or absolute drives. Returns the same path on success
/// so callers can `dir.join()` it directly.
fn sanitize_relative_path(raw: &str) -> Result<std::path::PathBuf> {
    let path = std::path::Path::new(raw);
    if path.is_absolute() {
        anyhow::bail!("absolute path");
    }
    for comp in path.components() {
        match comp {
            std::path::Component::Normal(_) => {}
            std::path::Component::CurDir => {}
            _ => anyhow::bail!("forbidden component {comp:?}"),
        }
    }
    Ok(path.to_path_buf())
}

/// Verify a body against its hex-encoded SHA-256. Comparison is case-insensitive.
fn verify_sha256(body: &[u8], expected_hex: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hex) {
        anyhow::bail!("sha256 mismatch: got {actual}, expected {expected_hex}");
    }
    Ok(())
}

/// Slug guard — keep this aligned with ClawHub's own `^[a-z0-9-]+$` rule plus
/// the path-traversal carve-outs we've always had.
fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() {
        anyhow::bail!("empty slug");
    }
    if slug.contains('/') || slug.contains('\\') || slug.contains("..") {
        anyhow::bail!("invalid clawhub slug {slug:?}");
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("clawhub slug {slug:?} has illegal characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_envelope_roundtrips() {
        let items = vec![ClawHubSkill {
            slug: "demo".into(),
            display_name: "Demo".into(),
            summary: "demo summary".into(),
            stats: ClawHubStats {
                stars: 7,
                downloads: 42,
            },
            tags: serde_json::json!({"latest": "1.0.0"}),
        }];
        let env = CacheEnvelope {
            fetched_at: 12_345,
            items: items.clone(),
        };
        let json = serde_json::to_vec(&env).unwrap();
        let back: CacheEnvelope = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.items, items);
    }

    #[test]
    fn deserializes_camel_and_snake_case() {
        let json = r#"{"slug":"a","displayName":"A","summary":"s","stats":{"stars":1}}"#;
        let s: ClawHubSkill = serde_json::from_str(json).unwrap();
        assert_eq!(s.display_name, "A");
        assert_eq!(s.stats.stars, 1);

        let json2 = r#"{"slug":"b","display_name":"B"}"#;
        let s2: ClawHubSkill = serde_json::from_str(json2).unwrap();
        assert_eq!(s2.display_name, "B");
    }

    #[test]
    fn deserializes_live_listing_shape() {
        // Snapshot of an actual ClawHub /skills?sort=stars item taken via
        // curl on 2026-04-29. Pinning the field shape here so future
        // upstream drift fails loud instead of silently emptying tags
        // again.
        let json = r#"{
            "slug": "self-improving-agent",
            "displayName": "self-improving-agent",
            "summary": "Captures learnings",
            "tags": {"latest": "3.0.18"},
            "stats": {
                "comments": 53,
                "downloads": 415412,
                "installsAllTime": 6434,
                "installsCurrent": 6108,
                "stars": 3393,
                "versions": 29
            },
            "createdAt": 1767632598365
        }"#;
        let s: ClawHubSkill = serde_json::from_str(json).expect("live shape parses");
        assert_eq!(s.slug, "self-improving-agent");
        assert_eq!(s.stats.stars, 3393);
        assert_eq!(s.stats.downloads, 415412);
        // tags is now a Value, not a Vec — the `latest` key is preserved
        // instead of silently emptied.
        assert_eq!(
            s.tags.get("latest").and_then(|v| v.as_str()),
            Some("3.0.18")
        );
    }

    #[test]
    fn validate_slug_rejects_traversal_and_path_separators() {
        validate_slug("ok-slug").unwrap();
        validate_slug("ok_slug_2").unwrap();
        assert!(validate_slug("").is_err());
        assert!(validate_slug("..").is_err());
        assert!(validate_slug("a/b").is_err());
        assert!(validate_slug("a\\b").is_err());
        assert!(validate_slug("with space").is_err());
        assert!(validate_slug("../etc/passwd").is_err());
    }

    #[test]
    fn sanitize_relative_path_accepts_nested_paths() {
        let p = sanitize_relative_path("assets/SKILL-TEMPLATE.md").unwrap();
        assert_eq!(p, std::path::PathBuf::from("assets/SKILL-TEMPLATE.md"));
        let q = sanitize_relative_path("README.md").unwrap();
        assert_eq!(q, std::path::PathBuf::from("README.md"));
    }

    #[test]
    fn sanitize_relative_path_rejects_traversal() {
        assert!(sanitize_relative_path("../escape").is_err());
        assert!(sanitize_relative_path("/etc/passwd").is_err());
        assert!(sanitize_relative_path("a/../b").is_err());
    }

    #[test]
    fn verify_sha256_accepts_correct_hash() {
        // SHA-256 of the empty string.
        let empty_sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        verify_sha256(b"", empty_sha).unwrap();

        // SHA-256 of "abc".
        let abc_sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_sha256(b"abc", abc_sha).unwrap();
    }

    #[test]
    fn verify_sha256_rejects_mismatch() {
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_sha256(b"abc", wrong).unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"));
    }

    #[test]
    fn verify_sha256_is_case_insensitive() {
        let upper = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        verify_sha256(b"abc", upper).unwrap();
    }
}
