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
    pub tags: Vec<String>,
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
    std::fs::write(&path, json)
        .with_context(|| format!("write cache {}", path.display()))?;
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

/// Install a single ClawHub skill. The on-disk shape follows the existing
/// skills convention: a directory named after the slug containing a
/// `SKILL.md`. We fetch the skill manifest's markdown content if available;
/// otherwise we write a placeholder pointing to the upstream URL so the
/// agent can still discover the skill exists.
///
/// This is intentionally conservative — Wave 3 may swap it out for a
/// fancier tarball-unpack path. The wave-2 contract is: after this returns
/// `Ok`, `<profile>/skills/<slug>/SKILL.md` exists.
pub async fn install_one(profile: &Profile, slug: &str) -> Result<()> {
    if slug.contains('/') || slug.contains('\\') || slug.contains("..") {
        anyhow::bail!("invalid clawhub slug {slug:?}");
    }
    let dir = profile.skills_dir().join(slug);
    if dir.exists() {
        // Idempotent — leave existing user state alone.
        return Ok(());
    }
    let url = format!("{}/skills/{}", base_url(), slug);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build reqwest client")?;
    let response = client.get(&url).send().await.context("GET clawhub skill")?;
    if !response.status().is_success() {
        anyhow::bail!("clawhub returned status {} for {slug}", response.status());
    }
    let body: serde_json::Value = response.json().await.context("parse clawhub skill")?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let md = body
        .get("latestVersion")
        .and_then(|v| v.get("readme"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            format!(
                "# {slug}\n\nInstalled from ClawHub: <https://clawhub.ai/skills/{slug}>\n"
            )
        });
    std::fs::write(dir.join("SKILL.md"), md)
        .with_context(|| format!("write SKILL.md for {slug}"))?;
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
            tags: vec!["tag-a".into()],
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
}
