use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Re-run when the checked-out commit changes.
    //
    // A pull on a checked-out branch does NOT rewrite the `HEAD` symref —
    // it still reads `ref: refs/heads/<branch>` after the pull — but it
    // DOES rewrite the file under `refs/heads/<branch>` (or
    // `packed-refs` when the ref has been packed). Declaring any
    // `rerun-if-changed` narrows cargo's default; watching only `HEAD`
    // would miss the common pull case.
    //
    // Note: cargo treats a missing `rerun-if-changed` path as dirty
    // (`Dirty … the file '…' is missing`), reruns the build script and
    // recompiles the crate — so we only emit a directive for a path we
    // have confirmed exists.
    //
    // In a git worktree the directory holding `HEAD` is the per-worktree
    // git dir (`git rev-parse --git-dir`), while `refs/` and `packed-refs`
    // live in the shared common dir (`git rev-parse --git-common-dir`).
    // Resolve both via git and emit the directives only for paths that
    // exist. When the working tree is not a repo (e.g. a release source
    // tarball) or git is not on PATH, fall back to watching `build.rs`
    // alone; the timestamp id is then stable until `build.rs` itself
    // changes.
    println!("cargo:rerun-if-changed=build.rs");

    let Some(head_path) = git_dir_head_path() else {
        let build_id = git_short_head().unwrap_or_else(timestamp_fallback);
        println!("cargo:rustc-env=RANTAICLAW_BUILD_ID={build_id}");
        return;
    };
    let Some(common_dir) = git_common_dir() else {
        let build_id = git_short_head().unwrap_or_else(timestamp_fallback);
        println!("cargo:rustc-env=RANTAICLAW_BUILD_ID={build_id}");
        return;
    };

    if head_path.exists() {
        println!("cargo:rerun-if-changed={}", head_path.display());
    }
    let packed = common_dir.join("packed-refs");
    if packed.exists() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }
    if let Some(ref_path) = resolved_ref_path(&head_path, &common_dir) {
        if ref_path.exists() {
            println!("cargo:rerun-if-changed={}", ref_path.display());
        }
    }

    let build_id = git_short_head().unwrap_or_else(timestamp_fallback);
    println!("cargo:rustc-env=RANTAICLAW_BUILD_ID={build_id}");
}

/// `git rev-parse --git-dir`, with `/HEAD` appended. Returns the absolute
/// (in a worktree) or relative (in a normal repo) path to the `HEAD` file
/// when git answers; otherwise `None` (no git on PATH, not a repo). Empty
/// output is filtered by `git_stdout`, which returns `None` for it.
fn git_dir_head_path() -> Option<PathBuf> {
    let s = git_stdout(&["rev-parse", "--git-dir"])?;
    let mut p = PathBuf::from(s);
    p.push("HEAD");
    Some(p)
}

/// `git rev-parse --git-common-dir`. Same return contract as
/// `git_dir_head_path`. Holds `refs/` and `packed-refs` in worktrees and is
/// identical to `--git-dir` in a normal repo. Empty output is filtered by
/// `git_stdout`, which returns `None` for it.
fn git_common_dir() -> Option<PathBuf> {
    let s = git_stdout(&["rev-parse", "--git-common-dir"])?;
    Some(PathBuf::from(s))
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
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

/// Returns the on-disk ref path `HEAD` points to when it is a symref,
/// e.g. `<common-dir>/refs/heads/main`. Returns `None` for a detached HEAD
/// (raw SHA on the first line), a missing `HEAD`, a symref target that
/// does not resolve under the common dir, or a non-git build directory.
/// The caller checks `.exists()` and skips the rerun directive when the
/// target is not on disk (e.g. a freshly cloned, never-checked-out ref).
fn resolved_ref_path(head_path: &std::path::Path, common_dir: &std::path::Path) -> Option<PathBuf> {
    let head = std::fs::read_to_string(head_path).ok()?;
    let target = head.lines().next()?.trim().strip_prefix("ref: ")?.trim();
    if target.is_empty() {
        return None;
    }
    Some(common_dir.join(target))
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
