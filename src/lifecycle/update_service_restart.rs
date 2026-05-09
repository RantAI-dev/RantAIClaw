//! Post-update service restart — Hermes parity.
//!
//! After a successful binary swap, if `rantaiclaw daemon` is running
//! under systemd (Linux user) or launchd (macOS), the running process
//! still has the old binary's code in memory until it's restarted.
//! Pre-fix the user had to know this and run `systemctl --user restart
//! rantaiclaw` themselves; mid-version drift was easy.
//!
//! This module detects the service unit and restarts it. Best-effort:
//! failure is logged, never aborts the update flow.

use anyhow::{Context, Result};
use std::process::Command;

/// Restart the rantaiclaw daemon service if one is registered. Returns
/// `Ok(true)` if a service was restarted, `Ok(false)` if no managed
/// service was detected (manual gateway, no daemon, etc.). Errors mean
/// "we tried but the service manager said no" — caller logs and moves
/// on.
pub fn restart_managed_service() -> Result<bool> {
    if let Some(unit) = detect_systemd_unit() {
        let status = Command::new("systemctl")
            .args(["--user", "restart", &unit])
            .status()
            .context("run systemctl --user restart")?;
        if !status.success() {
            anyhow::bail!("systemctl --user restart {unit} exited {:?}", status.code());
        }
        return Ok(true);
    }
    if let Some(label) = detect_launchd_label() {
        // launchctl kickstart -k <label> stops + starts in one call,
        // matching what Hermes does for managed gateways on macOS.
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", &label])
            .status()
            .context("run launchctl kickstart -k")?;
        if !status.success() {
            anyhow::bail!("launchctl kickstart {label} exited {:?}", status.code());
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn detect_systemd_unit() -> Option<String> {
    let candidates = ["rantaiclaw.service"];
    for unit in candidates {
        let out = Command::new("systemctl")
            .args(["--user", "is-active", unit])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let s = stdout.trim();
        // is-active prints `active`, `inactive`, `failed`, etc.
        // Restart even when failed/inactive — user may want to retry
        // after the binary swap. Only skip when totally unregistered
        // (returns `unknown`).
        if matches!(s, "active" | "activating" | "reloading" | "failed" | "inactive") {
            return Some((*unit).to_string());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn detect_systemd_unit() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
fn detect_launchd_label() -> Option<String> {
    let label = "com.rantaiclaw.daemon";
    let user_id = users_uid();
    let target = format!("gui/{user_id}/{label}");
    let out = Command::new("launchctl")
        .args(["print", &target])
        .output()
        .ok()?;
    if out.status.success() {
        Some(label.to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn users_uid() -> u32 {
    // SAFETY: getuid is async-signal-safe and never fails.
    unsafe { libc::getuid() }
}

#[cfg(not(target_os = "macos"))]
fn detect_launchd_label() -> Option<String> {
    None
}
