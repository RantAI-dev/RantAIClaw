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
const CACHE_TTL: Duration = Duration::from_hours(24);

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
    /// Publisher handle. ClawHub namespaces skills per owner, so one slug
    /// can belong to several publishers. Empty when the source endpoint
    /// omits it: `/skills?sort=stars` (what `list_top` reads) carries no
    /// owner at all, while `/search` does. An empty handle therefore means
    /// "unknown", not "unowned" — callers must not treat it as a match.
    #[serde(default, alias = "ownerHandle", alias = "owner_handle")]
    pub owner_handle: String,
    /// ClawHub's own "official publisher" marker. Displayed when asking the
    /// user which publisher they meant; never used to auto-select one.
    #[serde(default)]
    pub official: bool,
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

/// Server-side search across the ClawHub catalogue. Hits
/// `/api/v1/search?q=<q>&type=skill` which returns results ranked by a
/// fuzzy-match score (covers display name, slug, summary, tags). Empty
/// query returns an empty list — callers should fall back to `list_top`
/// when the user hasn't typed anything yet.
///
/// Unlike the listing endpoint, search reports `ownerHandle` and `official`
/// per result — the only discovery path that does. That matters because a
/// slug alone no longer identifies a skill: searching `weather` returns four
/// publishers, one of them a verbatim fork of another, and without the owner
/// the rows are indistinguishable.
///
/// Result entries carry no `stats` block (the server puts `downloads` at the
/// top level and reports no star count), so `stars` stays 0 here while
/// `downloads` is mapped through.
pub async fn search(query: &str) -> Result<Vec<ClawHubSkill>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let url = format!(
        "{}/search?q={}&type=skill",
        base_url(),
        urlencoding::encode(query)
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;
    let response = client
        .get(&url)
        .send()
        .await
        .context("GET clawhub search")?;
    if !response.status().is_success() {
        anyhow::bail!("clawhub search returned status {}", response.status());
    }

    #[derive(Deserialize)]
    struct SearchEnvelope {
        #[serde(default)]
        results: Vec<SearchResult>,
    }
    #[derive(Deserialize)]
    struct SearchResult {
        slug: String,
        #[serde(default, alias = "displayName", alias = "display_name")]
        display_name: String,
        #[serde(default)]
        summary: String,
        #[serde(default, alias = "ownerHandle", alias = "owner_handle")]
        owner_handle: String,
        #[serde(default)]
        official: bool,
        #[serde(default)]
        downloads: u64,
    }

    let env: SearchEnvelope = response
        .json()
        .await
        .context("parse clawhub search response")?;
    Ok(env
        .results
        .into_iter()
        .map(|r| ClawHubSkill {
            slug: r.slug,
            display_name: r.display_name,
            summary: r.summary,
            owner_handle: r.owner_handle,
            official: r.official,
            stats: ClawHubStats {
                stars: 0,
                downloads: r.downloads,
            },
            tags: serde_json::Value::Null,
        })
        .collect())
}

/// Pretty-print a skill's ClawHub metadata + security-scan summary to
/// stdout. Used by `rantaiclaw skills inspect <slug>` so a user can vet
/// a skill before installing.
pub async fn inspect_to_stdout(reference: &str) -> Result<()> {
    let skill_ref = parse_skill_ref(reference)?;
    let slug = skill_ref.slug.as_str();
    // Named apart from the `owner` read out of the detail payload below,
    // which is the handle we print rather than the one we query with.
    let owner_param = skill_ref.owner.as_deref();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("build reqwest client")?;

    // Detail. Goes through `fetch_with_retry` (not `error_for_status`) so an
    // ambiguous slug reports its candidate publishers instead of a bare 409 —
    // inspecting is exactly when a user is trying to tell them apart.
    let detail_url = with_owner(format!("{}/skills/{}", base_url(), slug), owner_param);
    let detail: serde_json::Value = fetch_with_retry(&client, &detail_url, slug)
        .await
        .with_context(|| format!("clawhub skill `{slug}` not found"))?
        .json()
        .await?;

    let skill = detail.get("skill").unwrap_or(&detail);
    let display = skill
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or(slug);
    let summary = skill.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let stars = skill
        .get("stats")
        .and_then(|v| v.get("stars"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let installs = skill
        .get("stats")
        .and_then(|v| v.get("installsAllTime"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let latest = detail
        .get("latestVersion")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let owner = detail
        .get("owner")
        .and_then(|v| v.get("handle"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    println!("{display} ({slug})  ★{stars}  installs:{installs}");
    println!("  latest:    {latest}");
    println!("  owner:     {owner}");
    if !summary.is_empty() {
        println!("  summary:   {summary}");
    }

    // Version manifest — files + security scan.
    let version_url = with_owner(
        format!("{}/skills/{}/versions/{}", base_url(), slug, latest),
        owner_param,
    );
    let manifest: serde_json::Value = fetch_with_retry(&client, &version_url, slug)
        .await
        .context("fetch version manifest")?
        .json()
        .await?;

    let version = manifest.get("version").unwrap_or(&manifest);

    if let Some(files) = version.get("files").and_then(|v| v.as_array()) {
        println!("  files:     {} file(s)", files.len());
        for f in files.iter().take(10) {
            let path = f.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let size = f.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("    - {path}  ({size} bytes)");
        }
        if files.len() > 10 {
            println!("    … {} more", files.len() - 10);
        }
    }

    if let Some(security) = version.get("security") {
        let status = security
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let warnings = security
            .get("hasWarnings")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "  security:  {status}{}",
            if warnings { " (with warnings)" } else { "" }
        );
        if let Some(scanners) = security.get("scanners") {
            for (name, body) in scanners.as_object().into_iter().flatten() {
                let s = body.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let v = body.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
                println!(
                    "    - {name:<6}  {s}{}",
                    if v.is_empty() {
                        String::new()
                    } else {
                        format!("  ({v})")
                    }
                );
            }
        }
    } else {
        println!("  security:  (no scan info available)");
    }

    if let Some(metadata) = manifest.get("metadata").or_else(|| version.get("metadata")) {
        if !metadata.is_null() {
            println!("  metadata:  {}", metadata);
        }
    }

    Ok(())
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
pub async fn install_one(profile: &Profile, reference: &str) -> Result<()> {
    let skill_ref = parse_skill_ref(reference)?;
    ensure_base_url_safe()?;
    let dir = profile.skills_dir().join(&skill_ref.slug);
    if dir.exists() {
        // Idempotent — leave existing user state alone. Callers wanting a
        // clean re-install should `fs::remove_dir_all` first. The one case
        // that is NOT a no-op is a different publisher of the same slug:
        // silently serving the installed one would answer "installed" for
        // code the caller never asked for.
        return ensure_existing_matches(&dir, &skill_ref);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build reqwest client")?;

    let result = install_one_inner(&client, &skill_ref, &dir).await;
    if result.is_err() {
        // Partial-install cleanup so the next attempt starts fresh and the
        // idempotent skip-if-exists guard above doesn't lock the user into
        // a broken state.
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

/// Guard for the skip-if-exists path: a slug already on disk is only the
/// same skill when it came from the same publisher.
///
/// Installs made before provenance was recorded have no marker. Those stay
/// idempotent (bailing would break every pre-existing install); the marker
/// gets backfilled the next time the skill is updated.
fn ensure_existing_matches(dir: &std::path::Path, skill_ref: &SkillRef) -> Result<()> {
    let (Some(requested), Some(existing)) = (skill_ref.owner.as_deref(), read_provenance(dir))
    else {
        return Ok(());
    };
    if existing.owner.is_empty() || existing.owner == requested {
        return Ok(());
    }
    anyhow::bail!(
        "`{}` is already installed from @{} — remove it first if you meant to switch to @{requested}",
        skill_ref.slug,
        existing.owner
    )
}

async fn install_one_inner(
    client: &reqwest::Client,
    skill_ref: &SkillRef,
    dir: &std::path::Path,
) -> Result<()> {
    let slug = skill_ref.slug.as_str();
    let owner = skill_ref.owner.as_deref();

    // Step 1 — resolve latest version.
    let detail_url = with_owner(format!("{}/skills/{}", base_url(), slug), owner);
    let detail_resp = fetch_with_retry(client, &detail_url, slug).await?;
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
    let version_url = with_owner(
        format!("{}/skills/{}/versions/{}", base_url(), slug, version),
        owner,
    );
    let version_resp = fetch_with_retry(client, &version_url, slug).await?;
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

    let version_obj = version_body.get("version").unwrap_or(&version_body);
    ensure_scan_allows(slug, version_obj)?;

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
        // The provenance marker records which publisher this directory came
        // from, and `skills update` trusts it to re-fetch from the same one.
        // A manifest that ships a file at that path could overwrite it and
        // forge its own origin, so the name is reserved.
        if is_reserved_manifest_path(&safe_rel) {
            anyhow::bail!(
                "clawhub: manifest for {slug} ships a reserved file name `{PROVENANCE_FILE}`"
            );
        }
        let target = dir.join(&safe_rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }

        // ClawHub serves file bytes via a singular `/file` endpoint with the
        // version and path as query parameters. The plural `/versions/:v/files/:path`
        // shape returns 404 — that was the v0.6.x install regression.
        let file_url = with_owner(
            format!(
                "{}/skills/{}/file?version={}&path={}",
                base_url(),
                slug,
                urlencoding::encode(&version),
                urlencoding::encode(&file.path),
            ),
            owner,
        );
        // SKILL.md is required; auxiliary files (README.md, LICENSE, etc.)
        // are best-effort. If the upstream manifest references a file that
        // 404s, treat non-SKILL.md as a warning so the install still
        // succeeds. This was the v0.6.1-alpha "Clawhub Error Install" bug
        // — a stale README.md reference broke the entire install.
        let is_required = file.path.eq_ignore_ascii_case("SKILL.md");
        ensure_required_hash(is_required, &file.sha256)?;
        let resp = match fetch_with_retry(client, &file_url, slug).await {
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

    // Written last, after every manifest file has landed, so a manifest can
    // never clobber it (the reserved-name guard above covers the direct
    // attempt; ordering covers the rest).
    write_provenance(
        dir,
        &Provenance {
            owner: owner.unwrap_or_default().to_string(),
            slug: slug.to_string(),
            version,
        },
    )?;

    Ok(())
}

/// Name of the provenance marker inside an installed skill directory.
/// Dot-prefixed so it sorts out of the way and reads as metadata; the skill
/// loader keys off `SKILL.md`/`SKILL.toml` and ignores it.
pub(crate) const PROVENANCE_FILE: &str = ".clawhub.json";

/// Which ClawHub publisher and version a local skill directory came from.
///
/// Without this there is no record of origin at all, and `skills update`
/// re-fetches by bare slug — which, now that slugs are ambiguous, could
/// silently swap one publisher's code for another's on update.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Provenance {
    /// Empty when the skill was installed without an owner (a slug unique
    /// enough that ClawHub answered directly).
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub version: String,
}

/// Does this manifest-relative path resolve to the provenance marker?
///
/// Compared component-wise rather than as a string: `./.clawhub.json` joins
/// to the same file as `.clawhub.json`, and `sanitize_relative_path` lets
/// `CurDir` through. Case-insensitive because macOS and Windows would treat
/// `.CLAWHUB.JSON` as the same file even though a byte comparison would not.
/// Nested paths cannot collide — the marker only ever lives at the root of
/// the skill directory.
fn is_reserved_manifest_path(rel: &std::path::Path) -> bool {
    let mut components = rel
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir));
    let Some(std::path::Component::Normal(first)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }
    first
        .to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(PROVENANCE_FILE))
}

pub(crate) fn read_provenance(dir: &std::path::Path) -> Option<Provenance> {
    let raw = std::fs::read(dir.join(PROVENANCE_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn write_provenance(dir: &std::path::Path, provenance: &Provenance) -> Result<()> {
    let path = dir.join(PROVENANCE_FILE);
    let json = serde_json::to_vec_pretty(provenance)?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))
}

/// Append `owner=<handle>` to a ClawHub URL that may already carry a query.
fn with_owner(url: String, owner: Option<&str>) -> String {
    let Some(owner) = owner else {
        return url;
    };
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}owner={}", urlencoding::encode(owner))
}

/// One candidate publisher from a `409 AMBIGUOUS_SKILL_SLUG` body.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AmbiguousMatch {
    #[serde(default, alias = "ownerHandle")]
    pub owner_handle: String,
    #[serde(default, rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub url: String,
}

/// ClawHub answered `409 Conflict` because the bare slug matches several
/// publishers. Carries the candidate list so callers can act on it: the CLI
/// prints it, the TUI/wizard turn it into a picker, the gateway forwards it
/// as a 409 body.
///
/// Deliberately NOT auto-resolved. Installing a skill stages remote code that
/// the agent will later read and act on, so picking a publisher on the user's
/// behalf — by popularity, by "official" flag, or by list order — would let
/// whoever squats a popular slug reach the operator's machine. The choice
/// stays explicit at every surface.
#[derive(Debug, Clone)]
pub struct AmbiguousSkill {
    pub slug: String,
    pub matches: Vec<AmbiguousMatch>,
}

impl std::fmt::Display for AmbiguousSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is published by {} owners on ClawHub — say which one you mean:",
            self.slug,
            self.matches.len()
        )?;
        for m in &self.matches {
            write!(f, "\n    @{}/{}", m.owner_handle, self.slug)?;
            if !m.url.is_empty() {
                write!(f, "  {}", m.url)?;
            }
        }
        if let Some(first) = self.matches.first() {
            write!(
                f,
                "\n  For example: `rantaiclaw skills install @{}/{}`",
                first.owner_handle, self.slug
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for AmbiguousSkill {}

/// Shape of the `409` body. Only `matches` is load-bearing — a 409 without
/// candidates is not actionable, so it falls through to the generic error.
#[derive(Debug, Deserialize)]
struct AmbiguousBody {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    matches: Vec<AmbiguousMatch>,
}

/// Fetch with two retries on `429 Too Many Requests`, with capped exponential
/// backoff. ClawHub aggressively rate-limits the file endpoints; transparent
/// retry is the difference between "install succeeds" and "user gives up".
///
/// `409` is decoded rather than retried: it means the slug is ambiguous, and
/// no amount of retrying resolves that. `fallback_slug` labels the error when
/// the body omits its own slug.
async fn fetch_with_retry(
    client: &reqwest::Client,
    url: &str,
    fallback_slug: &str,
) -> Result<reqwest::Response> {
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
        // A 409 that carries candidate publishers becomes the actionable
        // error; anything else (including a 409 we can't parse) falls
        // through to the generic message below.
        if status.as_u16() == 409 {
            if let Some(ambiguous) = decode_ambiguous(resp, fallback_slug).await {
                return Err(ambiguous.into());
            }
        }
        anyhow::bail!("clawhub returned status {status} for {url}");
    }
    unreachable!("loop exits via return or bail above")
}

/// Decode a `409` body into [`AmbiguousSkill`], or `None` when it isn't the
/// ambiguity error (unparsable body, or no candidates to choose from).
async fn decode_ambiguous(resp: reqwest::Response, fallback_slug: &str) -> Option<AmbiguousSkill> {
    let body = resp.text().await.ok()?;
    let parsed: AmbiguousBody = serde_json::from_str(&body).ok()?;
    if parsed.matches.is_empty() {
        return None;
    }
    let slug = if parsed.slug.is_empty() {
        fallback_slug.to_string()
    } else {
        parsed.slug
    };
    Some(AmbiguousSkill {
        slug,
        matches: parsed.matches,
    })
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

/// Read `(status, has_warnings)` out of a manifest's `version.security` block.
/// Returns `("", false)` when no scan info is present (unknown, not blocking).
fn scan_status(version: &serde_json::Value) -> (String, bool) {
    match version.get("security") {
        Some(sec) => {
            let status = sec
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let warnings = sec
                .get("hasWarnings")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (status, warnings)
        }
        None => (String::new(), false),
    }
}

/// A `fail` verdict blocks the install. Unknown / `pass` / empty do not.
fn is_blocking_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("fail")
}

/// Fail-closed gate: bail on a blocking scan verdict; warn (non-fatal) when the
/// scan reports warnings. `version` is the manifest's `version` object.
fn ensure_scan_allows(slug: &str, version: &serde_json::Value) -> Result<()> {
    let (status, warnings) = scan_status(version);
    if is_blocking_status(&status) {
        anyhow::bail!(
            "clawhub: refusing to install `{slug}` — security scan verdict is `{status}`. \
             Vet it with `rantaiclaw skills inspect {slug}` before installing."
        );
    }
    if warnings {
        tracing::warn!(
            slug,
            status = status.as_str(),
            "clawhub: skill has security-scan warnings; installing anyway"
        );
    }
    Ok(())
}

/// Required files (SKILL.md) must ship a verifiable hash. Optional files may
/// omit it (best-effort, unchanged).
fn ensure_required_hash(is_required: bool, sha256: &str) -> Result<()> {
    if is_required && sha256.is_empty() {
        anyhow::bail!(
            "clawhub: required file has no sha256 in the manifest — refusing to write it unverified"
        );
    }
    Ok(())
}

/// Reject a `RANTAICLAW_CLAWHUB_BASE_URL` override that is not https, unless it
/// targets loopback (127.0.0.1 / [::1] / localhost) — loopback is how the mock
/// server tests point the client locally and carries no MITM exposure.
fn ensure_base_url_safe() -> Result<()> {
    let Ok(url) = std::env::var(CLAWHUB_BASE_URL_ENV) else {
        return Ok(());
    };
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    let is_loopback = lower.starts_with("http://127.0.0.1")
        || lower.starts_with("http://[::1]")
        || lower.starts_with("http://localhost");
    if is_loopback {
        return Ok(());
    }
    anyhow::bail!(
        "refusing insecure {CLAWHUB_BASE_URL_ENV}=`{url}` — clawhub installs require https (loopback http allowed for tests)"
    );
}

/// A ClawHub install target: a slug, optionally qualified by the publisher
/// that owns it.
///
/// ClawHub namespaces skills per owner, so a bare slug is no longer a unique
/// identity — `GET /skills/<slug>` answers `409 AMBIGUOUS_SKILL_SLUG` when
/// several publishers use it (18 of the top 20 slugs, as of 2026-07). The
/// qualified form disambiguates via `?owner=<handle>`.
///
/// `slug` stays the local directory name either way, so an owner-qualified
/// install lands in the same place a bare one would. Two publishers of the
/// same slug therefore cannot coexist locally — [`install_one`] rejects the
/// second rather than silently serving the first (see the provenance marker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRef {
    pub owner: Option<String>,
    pub slug: String,
}

impl std::fmt::Display for SkillRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.owner {
            Some(owner) => write!(f, "@{owner}/{}", self.slug),
            None => write!(f, "{}", self.slug),
        }
    }
}

/// Parse an install target in any of the three accepted spellings:
/// `slug`, `owner/slug`, `@owner/slug`.
///
/// Both segments are validated with [`validate_slug`], so the traversal and
/// charset guards that have always protected the slug now cover the owner
/// too — the owner is interpolated into a URL query, and an unvalidated one
/// would be a parameter-injection point on the `/file` endpoint (which
/// already carries `version` and `path`).
pub(crate) fn parse_skill_ref(raw: &str) -> Result<SkillRef> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty skill reference");
    }
    let body = trimmed.strip_prefix('@').unwrap_or(trimmed);

    let Some((owner, slug)) = body.split_once('/') else {
        validate_slug(body)?;
        return Ok(SkillRef {
            owner: None,
            slug: body.to_string(),
        });
    };

    if slug.contains('/') {
        anyhow::bail!(
            "invalid skill reference {trimmed:?} — expected `slug` or `@owner/slug`, not a path"
        );
    }
    validate_slug(owner).context("invalid owner handle")?;
    validate_slug(slug).context("invalid slug")?;
    Ok(SkillRef {
        owner: Some(owner.to_string()),
        slug: slug.to_string(),
    })
}

/// Slug guard — keep this aligned with ClawHub's own `^[a-z0-9-]+$` rule plus
/// the path-traversal carve-outs we've always had. `pub(crate)` so the
/// gateway's `/api/v1/skills/*` mutating routes can reuse it as their
/// `{name}`/`slug` guard instead of re-implementing traversal rejection.
///
/// Deliberately NOT widened to accept `owner/slug`: these same rules guard
/// the `{name}` path parameter on the gateway's `DELETE`/`PUT` skill routes,
/// where accepting a separator would reopen traversal. Owner-qualified input
/// is split by [`parse_skill_ref`] first, then each segment comes back
/// through here.
pub(crate) fn validate_slug(slug: &str) -> Result<()> {
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
            owner_handle: "demo_owner".into(),
            official: true,
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
    fn parse_skill_ref_accepts_bare_owner_and_at_prefixed_forms() {
        let bare = parse_skill_ref("weather").unwrap();
        assert_eq!(bare.owner, None);
        assert_eq!(bare.slug, "weather");

        for raw in [
            "steipete/weather",
            "@steipete/weather",
            "  @steipete/weather  ",
        ] {
            let qualified = parse_skill_ref(raw).unwrap();
            assert_eq!(qualified.owner.as_deref(), Some("steipete"), "for {raw:?}");
            assert_eq!(qualified.slug, "weather", "for {raw:?}");
        }

        // Display round-trips to the canonical spelling the CLI advertises.
        assert_eq!(
            parse_skill_ref("steipete/weather").unwrap().to_string(),
            "@steipete/weather"
        );
    }

    #[test]
    fn parse_skill_ref_rejects_paths_and_traversal() {
        for raw in [
            "",
            "   ",
            "@",
            "@/weather",
            "steipete/",
            "../etc/passwd",
            "@../../etc/passwd",
            "@steipete/../../etc/passwd",
            "a/b/c",
            "@own er/weather",
            "@steipete/with space",
            "@steipete\\weather",
        ] {
            assert!(
                parse_skill_ref(raw).is_err(),
                "expected {raw:?} to be rejected"
            );
        }
    }

    #[test]
    fn with_owner_appends_using_the_right_separator() {
        assert_eq!(
            with_owner("https://h/api/v1/skills/weather".into(), Some("steipete")),
            "https://h/api/v1/skills/weather?owner=steipete"
        );
        assert_eq!(
            with_owner(
                "https://h/api/v1/skills/weather/file?version=1&path=SKILL.md".into(),
                Some("steipete")
            ),
            "https://h/api/v1/skills/weather/file?version=1&path=SKILL.md&owner=steipete"
        );
        // No owner means the URL is untouched — the bare-slug path is
        // unchanged for slugs only one publisher uses.
        assert_eq!(
            with_owner("https://h/api/v1/skills/weather".into(), None),
            "https://h/api/v1/skills/weather"
        );
    }

    #[test]
    fn with_owner_percent_encodes_the_handle() {
        // `parse_skill_ref` already restricts the handle to the slug
        // charset, so this is defence in depth: the handle lands in a query
        // string next to `version` and `path`, and encoding here means a
        // future caller that skips validation still cannot inject a
        // parameter.
        let url = with_owner(
            "https://h/api/v1/skills/weather/file?version=1".into(),
            Some("evil&path=../../etc/passwd"),
        );
        assert!(
            url.ends_with("&owner=evil%26path%3D..%2F..%2Fetc%2Fpasswd"),
            "{url}"
        );
        assert_eq!(url.matches("path=").count(), 0, "{url}");
    }

    #[test]
    fn is_reserved_manifest_path_covers_curdir_and_case_variants() {
        for raw in [".clawhub.json", "./.clawhub.json", ".CLAWHUB.JSON"] {
            assert!(
                is_reserved_manifest_path(std::path::Path::new(raw)),
                "expected {raw:?} to be reserved"
            );
        }
        // Nested paths cannot reach the root-level marker, and ordinary
        // skill files stay installable.
        for raw in ["SKILL.md", "assets/.clawhub.json", "clawhub.json"] {
            assert!(
                !is_reserved_manifest_path(std::path::Path::new(raw)),
                "expected {raw:?} to be allowed"
            );
        }
    }

    #[test]
    fn ambiguous_409_body_becomes_an_actionable_error() {
        // Verbatim shape of a real `GET /api/v1/skills/weather` 409, trimmed
        // to two candidates.
        let body = r#"{"code":"AMBIGUOUS_SKILL_SLUG",
            "message":"Found multiple skills with the slug \"weather\";",
            "slug":"weather",
            "matches":[
              {"ownerHandle":"steipete","slug":"weather","ref":"@steipete/weather",
               "url":"https://clawhub.ai/steipete/skills/weather"},
              {"ownerHandle":"lfengwa2","slug":"weather","ref":"@lfengwa2/weather",
               "url":"https://clawhub.ai/lfengwa2/skills/weather"}
            ]}"#;
        let parsed: AmbiguousBody = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.slug, "weather");
        assert_eq!(parsed.matches.len(), 2);
        assert_eq!(parsed.matches[0].owner_handle, "steipete");
        assert_eq!(parsed.matches[0].reference, "@steipete/weather");

        let rendered = AmbiguousSkill {
            slug: parsed.slug,
            matches: parsed.matches,
        }
        .to_string();
        // The message has to name every candidate and spell out a command
        // the user can actually run — a bare "409 Conflict" is what this
        // whole path exists to replace.
        assert!(rendered.contains("@steipete/weather"), "{rendered}");
        assert!(rendered.contains("@lfengwa2/weather"), "{rendered}");
        assert!(
            rendered.contains("rantaiclaw skills install @steipete/weather"),
            "{rendered}"
        );
    }

    #[test]
    fn provenance_roundtrips_through_the_marker_file() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_provenance(dir.path()).is_none());

        let written = Provenance {
            owner: "steipete".into(),
            slug: "weather".into(),
            version: "1.0.0".into(),
        };
        write_provenance(dir.path(), &written).unwrap();
        assert_eq!(read_provenance(dir.path()).unwrap(), written);
    }

    #[test]
    fn ensure_existing_matches_rejects_a_different_publisher() {
        let dir = tempfile::TempDir::new().unwrap();
        write_provenance(
            dir.path(),
            &Provenance {
                owner: "steipete".into(),
                slug: "weather".into(),
                version: "1.0.0".into(),
            },
        )
        .unwrap();

        let other = parse_skill_ref("@lfengwa2/weather").unwrap();
        let err = ensure_existing_matches(dir.path(), &other).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already installed from @steipete"), "{msg}");

        // Same publisher stays idempotent.
        let same = parse_skill_ref("@steipete/weather").unwrap();
        ensure_existing_matches(dir.path(), &same).unwrap();
    }

    #[test]
    fn ensure_existing_matches_allows_installs_predating_the_marker() {
        // No marker at all: every skill installed before provenance existed
        // looks like this, and bailing would break re-running an install
        // that used to be a no-op.
        let dir = tempfile::TempDir::new().unwrap();
        ensure_existing_matches(dir.path(), &parse_skill_ref("@steipete/weather").unwrap())
            .unwrap();

        // Marker present but ownerless (installed via an unambiguous slug).
        write_provenance(
            dir.path(),
            &Provenance {
                owner: String::new(),
                slug: "weather".into(),
                version: "1.0.0".into(),
            },
        )
        .unwrap();
        ensure_existing_matches(dir.path(), &parse_skill_ref("@steipete/weather").unwrap())
            .unwrap();

        // A bare request never conflicts — there is no owner to disagree with.
        ensure_existing_matches(dir.path(), &parse_skill_ref("weather").unwrap()).unwrap();
    }

    #[test]
    fn deserializes_owner_and_official_from_search_shape() {
        let json = r#"{"slug":"weather","displayName":"Weather","ownerHandle":"steipete","official":true}"#;
        let s: ClawHubSkill = serde_json::from_str(json).unwrap();
        assert_eq!(s.owner_handle, "steipete");
        assert!(s.official);

        // The listing endpoint reports no owner at all; that must decode as
        // "unknown" rather than failing the whole listing.
        let listing = r#"{"slug":"weather","displayName":"Weather"}"#;
        let s2: ClawHubSkill = serde_json::from_str(listing).unwrap();
        assert_eq!(s2.owner_handle, "");
        assert!(!s2.official);
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

    #[test]
    fn scan_fail_verdict_blocks_install() {
        let v = serde_json::json!({ "security": { "status": "fail", "hasWarnings": true } });
        assert!(is_blocking_status(&scan_status(&v).0));
        assert!(ensure_scan_allows("evil-skill", &v).is_err());
    }

    #[test]
    fn scan_pass_and_warnings_do_not_block() {
        let pass = serde_json::json!({ "security": { "status": "pass" } });
        assert!(ensure_scan_allows("ok", &pass).is_ok());
        let warn = serde_json::json!({ "security": { "status": "pass", "hasWarnings": true } });
        assert!(ensure_scan_allows("ok", &warn).is_ok()); // warns, does not fail
        let none = serde_json::json!({});
        assert!(ensure_scan_allows("ok", &none).is_ok()); // no scan info: not blocking
    }

    #[test]
    fn required_file_without_hash_is_rejected() {
        assert!(ensure_required_hash(true, "").is_err()); // SKILL.md, empty hash
        assert!(ensure_required_hash(true, "abc123").is_ok());
        assert!(ensure_required_hash(false, "").is_ok()); // optional file, ok
    }

    #[test]
    fn non_https_non_loopback_base_url_is_rejected() {
        let _guard = crate::test_env::ENV_LOCK.blocking_lock();
        let prev = std::env::var(CLAWHUB_BASE_URL_ENV).ok();
        std::env::set_var(CLAWHUB_BASE_URL_ENV, "http://evil.example/api/v1");
        assert!(ensure_base_url_safe().is_err());
        std::env::set_var(CLAWHUB_BASE_URL_ENV, "http://127.0.0.1:8080/api/v1");
        assert!(ensure_base_url_safe().is_ok()); // loopback allowed for tests
        std::env::set_var(CLAWHUB_BASE_URL_ENV, "https://clawhub.ai/api/v1");
        assert!(ensure_base_url_safe().is_ok());
        // restore
        match prev {
            Some(v) => std::env::set_var(CLAWHUB_BASE_URL_ENV, v),
            None => std::env::remove_var(CLAWHUB_BASE_URL_ENV),
        }
    }

    #[tokio::test]
    async fn search_returns_empty_for_blank_query() {
        // Server-side search with blank query is a guaranteed waste of a
        // round-trip — the function short-circuits and returns []. This
        // is also the contract callers rely on to fall back to list_top.
        let result = search("").await.unwrap();
        assert!(result.is_empty());
        let result = search("   ").await.unwrap();
        assert!(result.is_empty());
    }
}
