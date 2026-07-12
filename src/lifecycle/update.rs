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
use fs2::FileExt;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::lifecycle::artifact::{self, CosignOutcome};
use crate::lifecycle::binary_path::{require_self_modifiable, BinaryInfo, InstallKind};

const REPO: &str = "RantAI-dev/RantAIClaw";

/// Expected cosign signing identity for RantAIClaw binary releases — the
/// project's `pub-release.yml` workflow on a tag ref. Passed to the shared
/// `lifecycle::artifact::verify_cosign` so this module doesn't hard-code it
/// inside the shared verifier (the `ui` console installer pins its own
/// claw-ui identity instead).
const RANTAICLAW_COSIGN_IDENTITY: &str =
    r"^https://github\.com/RantAI-dev/RantAIClaw/\.github/workflows/pub-release\.yml@.*$";

/// First-launch verification timeout. The freshly-installed binary
/// must respond to `update verify` and exit 0 within this window or
/// the swap is rolled back.
const VERIFY_TIMEOUT_SECS: u64 = 8;

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
    /// Opt-in full-profile tarball backup before swap. Mirrors
    /// Hermes' `update --backup` for "high-value profiles". Adds time
    /// proportional to profile size (sessions.db + skills/* + secrets);
    /// the lightweight pre-update state snapshot still runs
    /// unconditionally regardless of this flag.
    pub backup: bool,
}

pub fn run(opts: UpdateOpts) -> Result<()> {
    // Single-update guarantee. Two concurrent `rantaiclaw update`
    // invocations would race on the staged-binary rename and could
    // leave a corrupt state. flock the lockfile for the duration of
    // the run; the lock is released automatically when `_lock` drops
    // or the process exits.
    let _lock = acquire_update_lock().context("acquire update lock")?;

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
        // Hermes-parity: surface release metadata so users can decide
        // whether to pull. `--check` still exits 1 on "newer
        // available," 0 on up-to-date — same script-friendly contract.
        if let Ok(notes) = fetch_release_notes(&target_tag) {
            print_release_summary(&notes);
        }
        std::process::exit(1);
    }

    require_self_modifiable(&info, "update")?;

    if info.kind == InstallKind::Cargo {
        println!(
            "⚠ updating a cargo-installed binary in place; `cargo install --list` \
             will show a stale version until you reinstall via cargo."
        );
    }

    // Pre-flight: the swap path writes `<binary>.new`, `<binary>.old`, and
    // renames into the install directory. If we can't write there, the
    // download is wasted bandwidth — bail early with a clear "re-run with
    // sudo" hint that tells the user exactly which dir is the problem.
    //
    // Common case: bootstrap.sh installed to /usr/local/bin (writable only
    // to root) but the user runs `rantaiclaw update` as themselves.
    require_install_dir_writable(&info.path)?;

    // Pre-flight: confirm there's enough free space across the three dirs
    // the update will touch (work dir for download+extract, snapshot dir,
    // install dir for `.new`/`.old` staging). Without this, we'd download
    // the archive only to die at the staged-binary write with `No space
    // left on device` — exactly the failure mode v0.6.54 surfaced on a
    // 2.5 GB Ubuntu VM where every megabyte counts.
    require_disk_space_for_update(&info.path)?;

    if !opts.yes && !confirm() {
        println!("aborted");
        return Ok(());
    }

    // Pre-swap: lightweight state snapshot. Best-effort — never aborts
    // the update. Mirrors Hermes' "Pairing-data snapshot" default.
    let rantaiclaw_root = crate::profile::paths::rantaiclaw_root();
    let active_profile = crate::profile::ProfileManager::resolve_active_name();
    let bak_binary = info.path.with_extension("old");
    let snapshot_summary = match crate::lifecycle::update_snapshot::create(
        &rantaiclaw_root,
        &current,
        &target_version,
        &active_profile,
        Some(&bak_binary),
    ) {
        Ok(snap) => {
            println!("✓ state snapshot: {}", snap.dir.display());
            Some(snap.dir)
        }
        Err(e) => {
            // Don't fail the update; warn so the user knows rollback
            // won't have a snapshot to restore from.
            eprintln!("⚠ pre-update snapshot failed: {e:#}");
            eprintln!("  proceeding without snapshot — rollback will rely on `.old` binary only");
            None
        }
    };

    if opts.backup {
        match crate::lifecycle::update_snapshot::full_backup_archive(&rantaiclaw_root, &current) {
            Ok(p) => println!("✓ full backup: {}", p.display()),
            Err(e) => eprintln!("⚠ --backup tarball failed: {e:#}"),
        }
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
        artifact::download_to(&archive_url, &archive_path)?;
        println!("⤓ {sums_url}");
        artifact::download_to(&sums_url, &sums_path)?;

        artifact::verify_sha256(&archive_path, &sums_path, &archive_name)?;
        println!("✓ SHA256 verified");

        // Cosign signature check on top of the SHA. The pub-release
        // workflow signs every artifact with cosign keyless OIDC and
        // attaches `<archive>.bundle`. Verifying the bundle confirms
        // the release actually came from the project's release
        // workflow on tags from the project's repo — protects against
        // a compromised release server that could otherwise swap
        // archive + SHA256SUMS atomically.
        //
        // Graceful degradation: if cosign isn't installed locally,
        // print a warning and proceed with SHA-only. Users on
        // air-gapped or minimal hosts shouldn't lose `update`; users
        // with cosign get the stronger guarantee. Once cosign is in
        // wider distros (it's already in homebrew, apt, AUR) this
        // becomes a hard requirement in a future cut.
        let bundle_url = format!("{base_url}/{archive_name}.bundle");
        match artifact::verify_cosign(
            &base_url,
            &archive_path,
            &archive_name,
            &work_dir,
            RANTAICLAW_COSIGN_IDENTITY,
        )? {
            CosignOutcome::Verified => println!("✓ cosign signature verified ({bundle_url})"),
            CosignOutcome::CosignNotInstalled => {
                // Already printed the "cosign not found on PATH" warning
                // from inside the helper. Don't double-warn with a
                // "bundle not found" message that misrepresents the
                // actual cause (the bundle IS published; we just can't
                // verify it without cosign).
            }
            CosignOutcome::BundleMissing => println!(
                "⚠ no cosign bundle published at {bundle_url} (pre-v0.6.44 release?). \
                 SHA-only verification will continue."
            ),
        }

        let extracted = extract_binary(&archive_path, &work_dir)?;
        println!("✓ extracted {}", extracted.display());

        swap_binary(&info.path, &extracted)?;

        // First-launch verification. Spawn the freshly-installed
        // binary in a short-timeout `update verify` mode. If it
        // crashes, hangs, or returns the wrong version, restore the
        // `.old` backup in-place so the user is never left with a
        // broken install. This is the "auto-rollback on bad swap"
        // guard — the snapshot rollback path stays available for
        // post-update issues but doesn't need to be invoked here.
        match verify_installed_binary(&info.path, &target_version) {
            Ok(()) => println!("✓ updated to {target_version}"),
            Err(e) => {
                eprintln!("⚠ first-launch verification failed: {e:#}");
                let backup = info.path.with_extension("old");
                if backup.is_file() {
                    if let Err(restore_err) = fs::rename(&backup, &info.path) {
                        bail!(
                            "first-launch verify failed AND auto-rollback failed: \
                             {restore_err:#}. Restore manually: `mv {} {}`",
                            backup.display(),
                            info.path.display()
                        );
                    }
                    eprintln!("↺ rolled back to {}", current);
                    bail!("update aborted: new binary failed first-launch verify");
                } else {
                    bail!(
                        "first-launch verify failed and no .old backup exists \
                         (snapshot at {}). Restore from snapshot or reinstall.",
                        snapshot_summary
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "<missing>".into())
                    );
                }
            }
        }

        // Post-swap: restart managed daemon service so the running
        // process picks up the new binary instead of staying on the
        // old in-memory code. Best-effort.
        match crate::lifecycle::update_service_restart::restart_managed_service() {
            Ok(true) => println!("✓ daemon service restarted"),
            Ok(false) => {} // no managed service — nothing to do
            Err(e) => eprintln!("⚠ daemon restart failed: {e:#}"),
        }

        // Print rollback hint so users don't have to remember the
        // command.
        if snapshot_summary.is_some() {
            println!(
                "  rollback: `rantaiclaw rollback` (latest snapshot) \
                 or `rantaiclaw rollback --list` to inspect"
            );
        } else if bak_binary.is_file() {
            println!(
                "  rollback: `mv {} {}` then re-run rantaiclaw",
                bak_binary.display(),
                info.path.display()
            );
        }
        Ok(())
    })();

    cleanup();
    result
}

/// Allocate a unique temp dir under `std::env::temp_dir()` without pulling
/// in the `tempfile` crate (which is dev-deps-only in this workspace).
/// Pre-flight: confirm the install directory is writable by the current
/// process before we burn bandwidth downloading the release archive.
///
/// We need write access to the install directory (not just the binary
/// file itself) because the swap path writes `<binary>.new`, `<binary>.old`,
/// and renames into the parent dir. A binary that's 0755 root:root in
/// /usr/local/bin gives the user *read* access to the binary but no write
/// access to the parent — exactly the case bootstrap.sh creates when
/// users install with `sudo` once and then update without it.
///
/// Probe by attempting `O_CREAT | O_EXCL` on a uniquely-named dotfile in
/// the parent directory. This is the same semantics the real swap will
/// use (creating `<binary>.new`). On PermissionDenied, bail with a
/// sudo-aware message that names the directory and shows the exact
/// re-run command.
fn require_install_dir_writable(running: &Path) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let parent = running.parent().ok_or_else(|| {
        anyhow!(
            "install-dir writability check failed: binary path {} has no parent",
            running.display()
        )
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = parent.join(format!(
        ".rantaiclaw-update-probe-{}-{:x}",
        std::process::id(),
        nanos
    ));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            // Best-effort cleanup; if remove fails we don't care, the
            // probe file is harmless and likely already unlinked by
            // someone else (extremely unlikely race).
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let parent_display = parent.display();
            let argv0 = std::env::args()
                .next()
                .unwrap_or_else(|| "rantaiclaw".to_string());
            bail!(
                "install directory not writable: {parent_display}\n\
                 \n\
                 The current user can't write `{parent_display}/rantaiclaw.new` — \
                 the `update` swap needs write access to the install dir, not just \
                 the binary file.\n\
                 \n\
                 Re-run with sudo (preserving env so your config/profile is found):\n\
                 \n    sudo -E {argv0} update\n\
                 \n\
                 Or reinstall to a user-owned dir like ~/.local/bin and re-run \
                 without sudo:\n\
                 \n    RANTAICLAW_INSTALL_DIR=$HOME/.local/bin curl -fsSL \\\n      \
                 https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash"
            )
        }
        Err(e) => bail!(
            "install-dir writability check failed for {}: {e}",
            parent.display()
        ),
    }
}

/// Headroom needed by an update, in bytes. Sized generously to cover:
///   - downloaded archive (~25 MB compressed across platforms today)
///   - extracted binary (~50-80 MB)
///   - staged `.new` file (same size as extracted)
///   - retained `.old` backup (same size as extracted)
///   - state snapshot (kilobytes; included in the margin)
///   - cargo-style build artifacts? no — `update` is binary-only
///
/// 200 MB gives ~3-4× the worst-case real consumption so users with
/// slightly larger future binaries don't immediately hit the wall.
const UPDATE_MIN_FREE_BYTES: u64 = 200 * 1024 * 1024;

/// Pre-flight: confirm there's at least `UPDATE_MIN_FREE_BYTES` available
/// on each filesystem the update will write to.
///
/// Checks the three relevant locations:
///   - **Work dir** — `$TMPDIR` / `/tmp` (download + extract)
///   - **Snapshot dir** — `<rantaiclaw_root>/.update-snapshots/`
///   - **Install dir** — parent of the running binary (`.new` + `.old`)
///
/// These often map to different filesystems (e.g. `/tmp` is sometimes
/// a tmpfs, `/usr/local/bin` is the root partition, `$HOME` may be a
/// separate volume), so we check each independently. The tightest one
/// is reported so the user knows where to free space.
fn require_disk_space_for_update(running: &Path) -> Result<()> {
    let install_parent = running.parent().ok_or_else(|| {
        anyhow!(
            "disk-space check failed: binary path {} has no parent",
            running.display()
        )
    })?;
    let work_root = std::env::temp_dir();
    let snapshot_root = crate::profile::paths::rantaiclaw_root();
    // Ensure the snapshot dir exists for the statvfs call (a missing
    // path on Linux makes statvfs walk up; on macOS it can error out).
    let _ = fs::create_dir_all(&snapshot_root);

    let locations: [(&str, &Path); 3] = [
        ("work dir (download/extract)", work_root.as_path()),
        ("snapshot dir", snapshot_root.as_path()),
        ("install dir", install_parent),
    ];

    let mut tightest: Option<(&str, &Path, u64)> = None;
    for (label, path) in &locations {
        let available = match fs2::available_space(path) {
            Ok(b) => b,
            Err(_) => continue, // best-effort: skip unprobeable paths
        };
        if available < UPDATE_MIN_FREE_BYTES
            && tightest.map_or(true, |(_, _, prev)| available < prev)
        {
            tightest = Some((label, path, available));
        }
    }

    if let Some((label, path, available)) = tightest {
        let need_mb = UPDATE_MIN_FREE_BYTES / (1024 * 1024);
        let avail_mb = available / (1024 * 1024);
        bail!(
            "not enough free disk space for update.\n\
             \n\
             {label} ({}) has {avail_mb} MB free; the update needs ~{need_mb} MB \
             (archive + extracted binary + .new staging + .old backup + snapshot).\n\
             \n\
             Free space, then retry:\n\
             \n    df -h\n\
             \n  Common space-savers:\n\
             \n    rm -rf {snapshot_root}/.update-snapshots\n\
             \n    rm -rf /tmp/rantaiclaw-update-*\n\
             \n    sudo apt-get clean   # or your package manager's equivalent\n\
             \n  Or skip `update` entirely and re-bootstrap:\n\
             \n    curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash",
            path.display(),
            snapshot_root = snapshot_root.display(),
        );
    }
    Ok(())
}

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
    let bin_name = if cfg!(windows) {
        "rantaiclaw.exe"
    } else {
        "rantaiclaw"
    };
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
    bail!(
        "`{bin_name}` not found in extracted archive at {}",
        extract_dir.display()
    )
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

        // fsync the staged binary before rename. Without this, a
        // crash between the rename and the kernel's deferred
        // write-back can persist the rename while leaving the file
        // content empty — the user is left with a 0-byte binary at
        // `running` that won't even invoke the rollback path.
        {
            let staged = fs::File::open(&new_local)
                .with_context(|| format!("reopen staged binary {}", new_local.display()))?;
            staged
                .sync_all()
                .with_context(|| format!("fsync staged binary {}", new_local.display()))?;
        }

        fs::rename(running, &backup)
            .with_context(|| format!("backup current binary to {}", backup.display()))?;
        if let Err(e) = fs::rename(&new_local, running) {
            // Roll back.
            let _ = fs::rename(&backup, running);
            return Err(e).with_context(|| format!("install new binary at {}", running.display()));
        }

        // fsync the parent directory so the two rename operations
        // are durably recorded. Without this, ext4/xfs can leave the
        // user post-crash with the old binary at `<running>.old` and
        // nothing at `<running>` at all.
        if let Some(parent) = running.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        // Keep the `.old` backup in place so `rantaiclaw rollback` can
        // restore it. Pre-v0.6.32 we deleted it on success, which made
        // the rollback story manual ("you have to know to grab a copy
        // before updating"). Mirrors Hermes' rollback affordance.
        // The backup is one binary's worth of disk; the next successful
        // update overwrites it via the `let _ = fs::remove_file(&backup)`
        // up-stack, so it doesn't accumulate across multiple updates.
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

/// Open + flock `<rantaiclaw_root>/.update.lock`. Returns the held
/// file; dropping it releases the lock automatically. Used by
/// `update::run` to serialise concurrent updates so two invocations
/// can't race on the staged-binary rename.
///
/// Uses `try_lock_exclusive` so a second invocation gets a clear
/// "another update is already running" error instead of blocking
/// forever.
fn acquire_update_lock() -> Result<fs::File> {
    let root = crate::profile::paths::rantaiclaw_root();
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let lock_path = root.join(".update.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open update lockfile {}", lock_path.display()))?;
    lock_file.try_lock_exclusive().map_err(|e| {
        anyhow!(
            "another `rantaiclaw update` appears to be running ({}). \
             If you're sure no other update is in flight, delete {}.",
            e,
            lock_path.display()
        )
    })?;
    Ok(lock_file)
}

/// Spawn the freshly-installed binary in `update verify` mode with
/// a short timeout, and confirm it (a) starts at all and (b) reports
/// the expected target version. Returns `Ok(())` only when both
/// conditions hold; otherwise the caller should auto-rollback to the
/// `.old` backup.
fn verify_installed_binary(installed: &Path, expected_version: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(installed)
        .args(["update", "--verify"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {} for first-launch verify", installed.display()))?;

    let deadline = Instant::now() + Duration::from_secs(VERIFY_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        let _ = s.read_to_string(&mut buf);
                        buf
                    })
                    .unwrap_or_default();
                let stderr = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        let _ = s.read_to_string(&mut buf);
                        buf
                    })
                    .unwrap_or_default();
                if !status.success() {
                    bail!(
                        "new binary exited {} (stderr: {})",
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "?".into()),
                        stderr.trim()
                    );
                }
                // `update verify` prints the version it sees as the
                // first line. Confirm it matches what we just
                // installed; mismatch means the swap didn't actually
                // land (e.g. PATH shadow, hardlink redirect).
                let reported = stdout.lines().next().unwrap_or("").trim();
                if reported != expected_version {
                    bail!(
                        "version mismatch after swap: expected {expected_version}, \
                         got {reported:?}"
                    );
                }
                return Ok(());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    bail!(
                        "new binary did not respond to `update verify` within {}s",
                        VERIFY_TIMEOUT_SECS
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => bail!("wait on `update verify` child: {e}"),
        }
    }
}

/// Entry point for `rantaiclaw update verify` — a tiny health check
/// the update orchestrator calls on the freshly-installed binary.
/// Just prints the compiled-in version and exits 0. Kept deliberately
/// minimal so a fault in any other subsystem can't cause a false
/// rollback. If you must add a side effect here, it has to be
/// strictly read-only.
pub fn run_verify() -> Result<()> {
    println!("{}", current_version());
    Ok(())
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

/// GitHub release metadata pulled by `update --check` to surface the
/// release notes without doing the actual binary download.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseNotes {
    pub tag_name: String,
    pub name: String,
    pub html_url: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub published_at: String,
}

/// Fetch release metadata for a specific tag. Used by `update --check`.
/// Errors are non-fatal — `--check` still exits 1 on "newer available"
/// even when notes can't be loaded (offline mirror, GitHub rate limit,
/// etc.).
pub fn fetch_release_notes(tag: &str) -> Result<ReleaseNotes> {
    let client = Client::builder()
        .user_agent(format!("rantaiclaw-update/{}", current_version()))
        .build()?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "GitHub releases API returned {} for tag {tag}",
            resp.status()
        );
    }
    let notes: ReleaseNotes = resp.json().context("parse release JSON")?;
    Ok(notes)
}

/// Pretty-print release notes — title, URL, first ~12 lines of body.
/// `update --check` calls this after the version-delta header.
pub fn print_release_summary(notes: &ReleaseNotes) {
    println!();
    println!(
        "release: {}",
        if notes.name.is_empty() {
            &notes.tag_name
        } else {
            &notes.name
        }
    );
    println!("   url:  {}", notes.html_url);
    if !notes.published_at.is_empty() {
        println!(" published: {}", notes.published_at);
    }
    if !notes.body.is_empty() {
        println!();
        let body = notes.body.replace("\r\n", "\n");
        let mut shown = 0usize;
        for line in body.lines().take(12) {
            println!("   {}", line);
            shown += 1;
        }
        if body.lines().count() > shown {
            println!("   …");
            println!("   (full notes: {})", notes.html_url);
        }
    }
    println!();
}

/// `rantaiclaw rollback` entry point. With no args, restores the most
/// recent snapshot + previous binary. With `--list`, prints available
/// snapshots and exits without restoring. With `--snapshot <path>`,
/// restores that specific snapshot.
pub fn rollback(opts: RollbackOpts) -> Result<()> {
    let info = BinaryInfo::detect()?;
    let rantaiclaw_root = crate::profile::paths::rantaiclaw_root();
    let snapshots = crate::lifecycle::update_snapshot::list_all(&rantaiclaw_root)?;

    if opts.list {
        if snapshots.is_empty() {
            println!(
                "No snapshots found in {}/.update-snapshots/",
                rantaiclaw_root.display()
            );
            return Ok(());
        }
        println!("Available snapshots (newest first):");
        println!();
        for s in &snapshots {
            println!(
                "  {}  {} → {}  (profile: {})",
                s.manifest.created_at,
                s.manifest.version_from,
                s.manifest.version_to,
                s.manifest.active_profile
            );
            println!("    {}", s.dir.display());
            if let Some(bak) = &s.manifest.bak_binary_path {
                let exists = std::path::Path::new(bak).is_file();
                println!(
                    "    .old binary: {bak} {}",
                    if exists { "[present]" } else { "[missing]" }
                );
            }
        }
        return Ok(());
    }

    let target = if let Some(path) = &opts.snapshot {
        snapshots
            .into_iter()
            .find(|s| s.dir == *path)
            .ok_or_else(|| anyhow!("snapshot {} not found", path))?
    } else {
        snapshots.into_iter().next().ok_or_else(|| {
            anyhow!(
                "no snapshots in {}/.update-snapshots/ — there's nothing to roll back to",
                rantaiclaw_root.display()
            )
        })?
    };

    println!(
        "rolling back to {} ({} → {}) …",
        target.manifest.created_at, target.manifest.version_to, target.manifest.version_from,
    );

    if !opts.yes && !confirm() {
        println!("aborted");
        return Ok(());
    }

    let summary = crate::lifecycle::update_snapshot::restore(&target, &rantaiclaw_root)?;
    if !summary.files_restored.is_empty() {
        println!("✓ restored {} state file(s)", summary.files_restored.len());
    }
    if summary.binary_restored {
        println!("✓ binary rolled back");
    } else {
        println!(
            "ℹ binary not rolled back (no `.old` available at {})",
            info.path.with_extension("old").display()
        );
    }

    // Try to restart the managed service so the rolled-back binary is
    // what's running, mirroring the post-update path.
    match crate::lifecycle::update_service_restart::restart_managed_service() {
        Ok(true) => println!("✓ daemon service restarted"),
        Ok(false) => {}
        Err(e) => eprintln!("⚠ daemon restart failed: {e:#}"),
    }

    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct RollbackOpts {
    /// Print available snapshots and exit without restoring.
    pub list: bool,
    /// Specific snapshot dir to restore (full path). Defaults to the
    /// most recent.
    pub snapshot: Option<String>,
    /// Skip the confirmation prompt.
    pub yes: bool,
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
        assert_eq!(split_pre("0.6.1-alpha"), ("0.6.1".into(), "alpha".into()));
        assert_eq!(split_pre("0.6.1-rc.1"), ("0.6.1".into(), "rc.1".into()));
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

    // ── Integrity gate: checksum verification ───────────────────────────────
    // `compute_sha256`/`read_sha_for_file` moved to `lifecycle::artifact`
    // (Task 1) along with their direct-unit tests; these two stay here as
    // an integration check that update.rs's call into the shared
    // `artifact::verify_sha256` still behaves as before.

    #[test]
    fn verify_sha256_accepts_the_matching_sum() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("rantaiclaw.tar.gz");
        fs::write(&archive, b"the release bytes").unwrap();
        // SHA-256 of the literal bytes "the release bytes".
        let sum = "86c0ea10853cdebc340b41ccd1a151ff282ec570bad37f52fa2321dd34b7479";
        let sums = dir.path().join("SHA256SUMS");
        fs::write(&sums, format!("{sum}  rantaiclaw.tar.gz\n")).unwrap();

        artifact::verify_sha256(&archive, &sums, "rantaiclaw.tar.gz")
            .expect("matching sum must verify");
    }

    #[test]
    fn verify_sha256_rejects_a_tampered_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("rantaiclaw.tar.gz");
        fs::write(&archive, b"tampered bytes").unwrap();
        let sums = dir.path().join("SHA256SUMS");
        // A digest that does not match the archive's real content.
        fs::write(
            &sums,
            "0000000000000000000000000000000000000000000000000000000000000000  rantaiclaw.tar.gz\n",
        )
        .unwrap();

        let err = artifact::verify_sha256(&archive, &sums, "rantaiclaw.tar.gz").unwrap_err();
        assert!(
            err.to_string().contains("SHA256 mismatch"),
            "expected a mismatch error, got: {err}"
        );
    }

    // ── Post-swap check: the new binary reports the expected version ───────

    #[cfg(unix)]
    fn write_version_stub(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verify_installed_binary_accepts_matching_version() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rantaiclaw-stub");
        write_version_stub(&bin, "#!/bin/sh\necho '0.7.5-alpha'\n");
        verify_installed_binary(&bin, "0.7.5-alpha").expect("matching version must verify");
    }

    #[cfg(unix)]
    #[test]
    fn verify_installed_binary_rejects_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rantaiclaw-stub");
        write_version_stub(&bin, "#!/bin/sh\necho '0.0.1-wrong'\n");
        let err = verify_installed_binary(&bin, "0.7.5-alpha").unwrap_err();
        assert!(
            err.to_string().contains("version mismatch"),
            "expected a version-mismatch error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_installed_binary_rejects_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rantaiclaw-stub");
        write_version_stub(&bin, "#!/bin/sh\necho boom >&2\nexit 3\n");
        let err = verify_installed_binary(&bin, "0.7.5-alpha").unwrap_err();
        assert!(
            err.to_string().contains("exited"),
            "expected a nonzero-exit error, got: {err}"
        );
    }
}
