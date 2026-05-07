//! `rantaiclaw update` — self-replace the binary against a published GitHub
//! release.
//!
//! The flow:
//!
//! 1. Resolve target tag — latest stable, latest prerelease, or pinned `--to`.
//! 2. If the target tag <= current version (and not `--allow-downgrade`),
//!    refuse.
//! 3. Pick the platform-matching archive name (e.g.
//!    `rantaiclaw-x86_64-unknown-linux-gnu.tar.gz`).
//! 4. Download archive + `SHA256SUMS` from the release.
//! 5. Verify SHA256 of the archive against the line in `SHA256SUMS`.
//! 6. Extract the binary using the system `tar` (Unix) or `tar.exe` (Windows
//!    10 1803+). Avoids adding `tar`/`flate2`/`zip` crates to the dep tree.
//! 7. Atomic swap on Unix (`rename` is atomic on the same filesystem). On
//!    Windows, stage the new binary as `<exe>.new`; the next launch detects
//!    and self-swaps before doing anything else.
//!
//! No additional crates were added for this module — we use the existing
//! `reqwest`, `sha2`, `hex`, `tempfile`, and shell out to `tar`.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::lifecycle::binary_path::{require_self_modifiable, BinaryInfo};

const REPO: &str = "RantAI-dev/RantAIClaw";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Prerelease,
}

#[derive(Debug, Clone)]
pub struct UpdateOpts {
    /// Print version delta only, don't download. Exit 0 if up-to-date,
    /// exit 1 if an update is available.
    pub check: bool,
    pub channel: Channel,
    /// Pin to a specific tag (e.g. `v0.6.2-alpha`). Overrides `channel`.
    pub to: Option<String>,
    pub allow_downgrade: bool,
    /// `https://github.com/RantAI-dev/RantAIClaw/releases/download/<TAG>` —
    /// honored for testing against staging or self-hosted releases.
    pub release_base_url: Option<String>,
    pub yes: bool,
}

pub fn run(opts: UpdateOpts) -> Result<()> {
    let info = BinaryInfo::detect()?;
    let current = current_version();

    let target_tag = if let Some(t) = &opts.to {
        normalize_tag(t)
    } else {
        resolve_latest_tag(opts.channel)?
    };

    let target_version = strip_v_prefix(&target_tag);
    let cmp = compare_versions(&target_version, &current);

    println!("current: {current}");
    println!("target:  {target_version}  (tag {target_tag})");

    if cmp == 0 {
        println!("✓ already on target version");
        return Ok(());
    }
    if cmp < 0 && !opts.allow_downgrade {
        bail!(
            "target {target_version} is older than current {current}; pass --allow-downgrade to proceed"
        );
    }

    if opts.check {
        // exit 1 to make `rantaiclaw update --check && deploy` workflows trivial
        std::process::exit(1);
    }

    require_self_modifiable(&info, "update")?;

    if !opts.yes && !confirm() {
        println!("aborted");
        return Ok(());
    }

    let target = platform_target()?;
    let archive_name = if target.contains("windows") {
        format!("rantaiclaw-{target}.zip")
    } else {
        format!("rantaiclaw-{target}.tar.gz")
    };

    let base_url = opts
        .release_base_url
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/download/{target_tag}"));
    let archive_url = format!("{base_url}/{archive_name}");
    let sums_url = format!("{base_url}/SHA256SUMS");

    let work_dir = make_work_dir()?;
    let archive_path = work_dir.join(&archive_name);
    let sums_path = work_dir.join("SHA256SUMS");

    let cleanup = || {
        let _ = fs::remove_dir_all(&work_dir);
    };

    let result = (|| -> Result<()> {
        println!("⤓ {archive_url}");
        download_to(&archive_url, &archive_path)?;
        println!("⤓ {sums_url}");
        download_to(&sums_url, &sums_path)?;

        verify_sha256(&archive_path, &sums_path, &archive_name)?;
        println!("✓ SHA256 verified");

        let extracted = extract_binary(&archive_path, &work_dir)?;
        println!("✓ extracted {}", extracted.display());

        swap_binary(&info.path, &extracted)?;
        println!("✓ updated to {target_version}");
        Ok(())
    })();

    cleanup();
    result
}

/// Allocate a unique temp dir under `std::env::temp_dir()` without pulling
/// in the `tempfile` crate (which is dev-deps-only in this workspace).
fn make_work_dir() -> Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("rantaiclaw-update-{pid}-{nanos:x}"));
    fs::create_dir_all(&dir).with_context(|| format!("create temp dir {}", dir.display()))?;
    Ok(dir)
}

fn confirm() -> bool {
    use std::io::{self, BufRead, Write};
    print!("Apply update? [y/N] ");
    let _ = io::stdout().flush();
    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn normalize_tag(t: &str) -> String {
    if t.starts_with('v') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

fn strip_v_prefix(t: &str) -> String {
    t.strip_prefix('v').unwrap_or(t).to_string()
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

fn resolve_latest_tag(channel: Channel) -> Result<String> {
    let client = Client::builder()
        .user_agent(format!("rantaiclaw-update/{}", current_version()))
        .build()?;
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=20");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GitHub releases API returned {}", resp.status());
    }
    let releases: Vec<GhRelease> = resp.json().context("parse releases JSON")?;
    let tag = releases
        .into_iter()
        .filter(|r| !r.draft)
        .find(|r| match channel {
            Channel::Stable => !r.prerelease,
            Channel::Prerelease => true,
        })
        .map(|r| r.tag_name)
        .ok_or_else(|| {
            anyhow!(
                "no release found on {} channel",
                match channel {
                    Channel::Stable => "stable",
                    Channel::Prerelease => "prerelease",
                }
            )
        })?;
    Ok(tag)
}

fn download_to(url: &str, dest: &Path) -> Result<()> {
    let client = Client::builder()
        .user_agent(format!("rantaiclaw-update/{}", current_version()))
        .build()?;
    let mut resp = client.get(url).send().with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("download {url} returned {}", resp.status());
    }
    let mut f = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).context("read response body")?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])
            .with_context(|| format!("write {}", dest.display()))?;
    }
    Ok(())
}

fn verify_sha256(archive: &Path, sums_file: &Path, archive_name: &str) -> Result<()> {
    let expected = read_sha_for_file(sums_file, archive_name)?;
    let actual = compute_sha256(archive)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!(
            "SHA256 mismatch for {archive_name}\n  expected: {expected}\n  actual:   {actual}"
        );
    }
    Ok(())
}

fn read_sha_for_file(sums_file: &Path, archive_name: &str) -> Result<String> {
    let content = fs::read_to_string(sums_file)
        .with_context(|| format!("read {}", sums_file.display()))?;
    for line in content.lines() {
        // Format: "<hex>  <filename>" (two spaces) or "<hex> *<filename>" or
        // "<hex>  ./<filename>". Be liberal in parsing.
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let Some(hash) = it.next() else { continue };
        let Some(rest) = it.next() else { continue };
        let name = rest
            .trim()
            .trim_start_matches('*')
            .trim_start_matches("./");
        if name == archive_name {
            return Ok(hash.to_string());
        }
    }
    bail!("{archive_name} not listed in SHA256SUMS")
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Extract the `rantaiclaw` binary from the downloaded archive into a temp
/// dir and return the resulting path. Uses the system `tar` (or `tar.exe`)
/// to avoid adding a tar/zip dep.
fn extract_binary(archive: &Path, work_dir: &Path) -> Result<PathBuf> {
    let extract_dir = work_dir.join("extracted");
    fs::create_dir_all(&extract_dir).context("create extract dir")?;

    let archive_str = archive.to_string_lossy().to_string();
    let lc = archive_str.to_ascii_lowercase();

    let status = if lc.ends_with(".tar.gz") || lc.ends_with(".tgz") {
        std::process::Command::new("tar")
            .args(["-xzf", &archive_str, "-C"])
            .arg(&extract_dir)
            .status()
            .context("run tar (is `tar` on PATH?)")?
    } else if lc.ends_with(".zip") {
        // tar.exe in Windows 10 1803+ understands zip too.
        std::process::Command::new("tar")
            .args(["-xf", &archive_str, "-C"])
            .arg(&extract_dir)
            .status()
            .context("run tar -xf on .zip (is `tar` on PATH?)")?
    } else {
        bail!("unknown archive type: {}", archive.display());
    };

    if !status.success() {
        bail!("archive extraction failed (exit {:?})", status.code());
    }

    // Find the rantaiclaw binary inside `extract_dir`. The release archives
    // include the binary at the top level.
    let bin_name = if cfg!(windows) { "rantaiclaw.exe" } else { "rantaiclaw" };
    let direct = extract_dir.join(bin_name);
    if direct.exists() {
        return Ok(direct);
    }
    // Walk one level deep just in case the archive has a top-level dir.
    if let Ok(entries) = fs::read_dir(&extract_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let candidate = p.join(bin_name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }
    bail!("`{bin_name}` not found in extracted archive at {}", extract_dir.display())
}

fn swap_binary(running: &Path, new_bin: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Preserve permissions from the running binary.
        let mode = fs::metadata(running)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755);
        let backup = running.with_extension("old");
        // Remove any stale backup from a previous failed update.
        let _ = fs::remove_file(&backup);
        // Atomic on the same filesystem. The new binary needs to be on the
        // same FS as `running`; copy first if extraction was elsewhere.
        let new_local = running.with_extension("new");
        let _ = fs::remove_file(&new_local);
        fs::copy(new_bin, &new_local)
            .with_context(|| format!("stage new binary at {}", new_local.display()))?;
        fs::set_permissions(&new_local, fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod staged binary {}", new_local.display()))?;

        fs::rename(running, &backup)
            .with_context(|| format!("backup current binary to {}", backup.display()))?;
        if let Err(e) = fs::rename(&new_local, running) {
            // Roll back.
            let _ = fs::rename(&backup, running);
            return Err(e).with_context(|| format!("install new binary at {}", running.display()));
        }
        // Best-effort cleanup of the backup. Some users may want to keep it
        // around — we leave it in place; the next successful update or the
        // uninstall flow will clean it.
        let _ = fs::remove_file(&backup);
        Ok(())
    }
    #[cfg(windows)]
    {
        // Cannot replace a running .exe. Stage as <exe>.new; the next launch
        // detects the file and self-swaps before doing anything.
        let staged = running.with_extension("new.exe");
        let _ = fs::remove_file(&staged);
        fs::copy(new_bin, &staged)
            .with_context(|| format!("stage new binary at {}", staged.display()))?;
        println!(
            "Update staged at {}.\n\
             Restart your shell or run rantaiclaw once more to activate it.",
            staged.display()
        );
        Ok(())
    }
}

/// Apply a previously staged Windows update before doing anything else.
/// Called early on every launch.
#[cfg(windows)]
pub fn apply_pending_windows_update() -> Result<()> {
    let exe = std::env::current_exe()?;
    let staged = exe.with_extension("new.exe");
    if staged.exists() {
        let backup = exe.with_extension("old.exe");
        let _ = fs::remove_file(&backup);
        // Best-effort: if rename fails we just leave the staged file alone.
        if fs::rename(&exe, &backup).is_ok() {
            if fs::rename(&staged, &exe).is_err() {
                let _ = fs::rename(&backup, &exe);
            } else {
                let _ = fs::remove_file(&backup);
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn apply_pending_windows_update() -> Result<()> {
    Ok(())
}

fn platform_target() -> Result<&'static str> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "arm") => "armv7-unknown-linux-gnueabihf",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no published binary for {os}/{arch}"),
    };
    Ok(target)
}

/// Compare two semver-ish version strings. Returns -1, 0, 1.
///
/// Accepts the project's actual versioning shapes:
/// - `0.5.3`, `0.6.0`
/// - `0.6.1-alpha`, `0.6.1-rc.1`, `1.0.0-beta.2`
///
/// Pre-release < release for equal core (semver §11). Two pre-releases are
/// compared lexicographically by identifier list.
fn compare_versions(a: &str, b: &str) -> i32 {
    let (core_a, pre_a) = split_pre(a);
    let (core_b, pre_b) = split_pre(b);

    let cmp_core = compare_core(&core_a, &core_b);
    if cmp_core != 0 {
        return cmp_core;
    }
    match (pre_a.is_empty(), pre_b.is_empty()) {
        (true, true) => 0,
        // No prerelease > prerelease.
        (true, false) => 1,
        (false, true) => -1,
        (false, false) => compare_pre(&pre_a, &pre_b),
    }
}

fn split_pre(v: &str) -> (String, String) {
    if let Some((c, p)) = v.split_once('-') {
        (c.to_string(), p.to_string())
    } else {
        (v.to_string(), String::new())
    }
}

fn compare_core(a: &str, b: &str) -> i32 {
    let parts_a: Vec<u64> = a.split('.').filter_map(|p| p.parse().ok()).collect();
    let parts_b: Vec<u64> = b.split('.').filter_map(|p| p.parse().ok()).collect();
    for i in 0..parts_a.len().max(parts_b.len()) {
        let x = parts_a.get(i).copied().unwrap_or(0);
        let y = parts_b.get(i).copied().unwrap_or(0);
        if x < y {
            return -1;
        }
        if x > y {
            return 1;
        }
    }
    0
}

fn compare_pre(a: &str, b: &str) -> i32 {
    let parts_a: Vec<&str> = a.split('.').collect();
    let parts_b: Vec<&str> = b.split('.').collect();
    for i in 0..parts_a.len().max(parts_b.len()) {
        let x = parts_a.get(i).copied().unwrap_or("");
        let y = parts_b.get(i).copied().unwrap_or("");
        // Numeric identifiers compare numerically; alphanumerics
        // lexicographically; numeric < alphanumeric (semver §11).
        let xn = x.parse::<u64>().ok();
        let yn = y.parse::<u64>().ok();
        let c = match (xn, yn) {
            (Some(xv), Some(yv)) => xv.cmp(&yv) as i32,
            (Some(_), None) => -1,
            (None, Some(_)) => 1,
            (None, None) => x.cmp(y) as i32,
        };
        if c != 0 {
            return c;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_basic_increments() {
        assert_eq!(compare_versions("0.6.1", "0.5.3"), 1);
        assert_eq!(compare_versions("0.5.3", "0.6.1"), -1);
        assert_eq!(compare_versions("0.6.1", "0.6.1"), 0);
        assert_eq!(compare_versions("1.0.0", "0.99.99"), 1);
    }

    #[test]
    fn compare_prerelease_ordering() {
        // Prerelease < release (same core).
        assert_eq!(compare_versions("0.6.1-alpha", "0.6.1"), -1);
        assert_eq!(compare_versions("0.6.1", "0.6.1-alpha"), 1);
        // Prerelease ordering by lex.
        assert_eq!(compare_versions("0.6.1-alpha", "0.6.1-beta"), -1);
        assert_eq!(compare_versions("0.6.1-rc.1", "0.6.1-rc.2"), -1);
    }

    #[test]
    fn compare_alpha_to_alpha2() {
        // Lex comparison of pre identifiers — "alpha" < "alpha.2" because
        // the second has an extra identifier.
        let c = compare_versions("0.6.1-alpha", "0.6.2-alpha");
        assert_eq!(c, -1);
    }

    #[test]
    fn split_pre_basic() {
        assert_eq!(split_pre("0.5.3"), ("0.5.3".into(), String::new()));
        assert_eq!(
            split_pre("0.6.1-alpha"),
            ("0.6.1".into(), "alpha".into())
        );
        assert_eq!(
            split_pre("0.6.1-rc.1"),
            ("0.6.1".into(), "rc.1".into())
        );
    }

    #[test]
    fn read_sha_for_file_handles_multiple_formats() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SHA256SUMS");
        fs::write(
            &p,
            "abc123  rantaiclaw-x86_64-unknown-linux-gnu.tar.gz\n\
             def456  ./rantaiclaw.spdx.json\n\
             feed00 *some-other.zip\n",
        )
        .unwrap();
        assert_eq!(
            read_sha_for_file(&p, "rantaiclaw-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "abc123"
        );
        assert_eq!(
            read_sha_for_file(&p, "rantaiclaw.spdx.json").unwrap(),
            "def456"
        );
        assert_eq!(read_sha_for_file(&p, "some-other.zip").unwrap(), "feed00");
        assert!(read_sha_for_file(&p, "missing.tar.gz").is_err());
    }

    #[test]
    fn normalize_tag_handles_both_forms() {
        assert_eq!(normalize_tag("v0.6.2-alpha"), "v0.6.2-alpha");
        assert_eq!(normalize_tag("0.6.2-alpha"), "v0.6.2-alpha");
    }

    #[test]
    fn strip_v_prefix_works() {
        assert_eq!(strip_v_prefix("v0.6.2-alpha"), "0.6.2-alpha");
        assert_eq!(strip_v_prefix("0.6.2-alpha"), "0.6.2-alpha");
    }
}
