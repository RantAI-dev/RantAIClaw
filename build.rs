use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Re-run when the checked-out commit changes.
    //
    // A pull on a checked-out branch does NOT rewrite `.git/HEAD` — it
    // still reads `ref: refs/heads/<branch>` after the pull — but it DOES
    // rewrite the ref file under `.git/refs/heads/<branch>` (or
    // `.git/packed-refs` when the ref has been packed). Declaring any
    // `rerun-if-changed` narrows cargo's default, which is to re-run the
    // script on any package-file change; watching only `.git/HEAD` would
    // miss the common pull case and the previous commit's
    // `RANTAICLAW_BUILD_ID` would stick. Watch HEAD, the resolved ref,
    // and packed-refs so every ref-bumping event triggers a re-run.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(ref_path) = resolved_ref_path() {
        println!("cargo:rerun-if-changed={ref_path}");
    }
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let build_id = git_short_head().unwrap_or_else(timestamp_fallback);
    println!("cargo:rustc-env=RANTAICLAW_BUILD_ID={build_id}");
}

/// Returns the on-disk ref path `.git/HEAD` points to when it is a symref,
/// e.g. `.git/refs/heads/main`. Returns `None` for a detached HEAD (raw SHA
/// on the first line), a missing `.git/HEAD`, a non-git build directory, or
/// a symref target that does not exist on disk yet — cargo silently skips a
/// declared path it cannot find, so the caller's `if let` is the right gate.
fn resolved_ref_path() -> Option<String> {
    let head = std::fs::read_to_string(".git/HEAD").ok()?;
    let target = head.lines().next()?.trim().strip_prefix("ref: ")?.trim();
    if target.is_empty() {
        return None;
    }
    let path = format!(".git/{target}");
    if std::path::Path::new(&path).exists() {
        Some(path)
    } else {
        None
    }
}

/// `git rev-parse --short HEAD` succeeds for a normal checkout. Returns None if
/// git is missing (e.g. a release source tarball), the working tree is not a
/// repo, or the captured stdout is empty — all of which fall through to the
/// timestamp path.
fn git_short_head() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// ISO-8601 UTC timestamp. Prefer `$SOURCE_DATE_EPOCH` so a reproducible build
/// (release tarball, Nix flake, container) gets a stable string; fall back to
/// the wall clock otherwise.
fn timestamp_fallback() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    format_iso8601_utc(secs)
}

fn format_iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Civil-from-days algorithm (Howard Hinnant). Avoids pulling in `time`/`chrono`
/// just to format a fallback string. Returns (year, month, day) with January =
/// 1 and the year signed (proleptic Gregorian).
fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}
