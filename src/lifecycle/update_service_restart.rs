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

use crate::service::{is_dev_build, DEV_BUILD_SKIP_MESSAGE};

/// Restart the rantaiclaw daemon service if one is registered. Returns
/// `Ok(true)` if a service was restarted, `Ok(false)` if no managed
/// service was detected (manual gateway, no daemon, etc.). Errors mean
/// "we tried but the service manager said no" — caller logs and moves
/// on.
pub fn restart_managed_service() -> Result<bool> {
    if is_dev_build() {
        tracing::info!("{DEV_BUILD_SKIP_MESSAGE}");
        return Ok(false);
    }
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
    if is_dev_build() {
        tracing::debug!("{DEV_BUILD_SKIP_MESSAGE}");
        return None;
    }
    let candidates = ["rantaiclaw.service"];
    for unit in candidates {
        // Probe with `systemctl --user cat` rather than `is-active`.
        //
        // Pre-fix: we read `is-active` stdout and treated `inactive`
        // as "unit exists, restart it" — but `is-active` ALSO prints
        // `inactive` (with non-zero exit) for units that are NOT
        // installed at all. That made every fresh CLI/TUI install
        // emit a red `Failed to restart rantaiclaw.service: Unit
        // rantaiclaw.service not found.` line right after a perfectly
        // successful `rantaiclaw update`, scaring users.
        //
        // `cat` succeeds (exit 0) only when systemd can locate the
        // unit file; if the unit was never installed (the default
        // for everyone running CLI/TUI without `rantaiclaw daemon`
        // managed by systemd), we get a clean None and the caller's
        // `Ok(false)` branch silently no-ops. Installed-but-stopped
        // units still get restarted because `cat` returns the unit
        // body regardless of runtime state.
        let cat = Command::new("systemctl")
            .args(["--user", "cat", "--no-pager", unit])
            .output()
            .ok()?;
        if cat.status.success() {
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
    if is_dev_build() {
        tracing::debug!("{DEV_BUILD_SKIP_MESSAGE}");
        return None;
    }
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

#[cfg(test)]
mod dev_build_skip_tests {
    //! A debug or test build never reaches `systemctl`/`launchctl`. The fake
    //! on PATH would write a marker on restart; the dev-build guard short-
    //! circuits before the fake is reached, and `detect_*` returns the same
    //! "nothing registered" answer it would have given for an uninstalled
    //! service.

    use super::{detect_systemd_unit, restart_managed_service};
    use crate::service::is_dev_build;

    /// With no unit file present, `restart_managed_service` already returns
    /// `Ok(false)` in release. The test pins the dev-build path too: even when
    /// a unit file exists under a temp `XDG_CONFIG_HOME` and the fake answers
    /// `cat` successfully, the guard must short-circuit.
    #[cfg(target_os = "linux")]
    #[test]
    fn restart_managed_service_dev_build_returns_false_and_does_not_spawn() {
        assert!(
            is_dev_build(),
            "this test only proves its point under a debug/test build"
        );
        let _lock = crate::test_env::ENV_LOCK.blocking_lock();
        let fake = crate::test_env::FakeServiceManager::new();
        let xdg = tempfile::tempdir().expect("xdg tempdir");
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
        let unit_dir = xdg.path().join("systemd").join("user");
        std::fs::create_dir_all(&unit_dir).expect("systemd unit dir");
        std::fs::write(
            unit_dir.join("rantaiclaw.service"),
            "[Unit]\nDescription=test\n",
        )
        .expect("write unit file");

        let outcome = restart_managed_service().expect("dev build returns Ok");
        assert!(!outcome, "dev build returns Ok(false) — no managed restart");
        assert!(
            !fake.restart_marker().exists(),
            "fake systemctl must not be reached; marker at {}",
            fake.restart_marker().display()
        );
    }

    /// `detect_systemd_unit` is private to this module. The public path that
    /// reaches it (`restart_managed_service`) is guarded above; this test pins
    /// the helper directly so a regression there is caught even if the public
    /// guard is removed.
    #[cfg(target_os = "linux")]
    #[test]
    fn detect_systemd_unit_dev_build_returns_none_without_spawning() {
        assert!(
            is_dev_build(),
            "this test only proves its point under a debug/test build"
        );
        let _lock = crate::test_env::ENV_LOCK.blocking_lock();
        let fake = crate::test_env::FakeServiceManager::new();
        let xdg = tempfile::tempdir().expect("xdg tempdir");
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
        let unit_dir = xdg.path().join("systemd").join("user");
        std::fs::create_dir_all(&unit_dir).expect("systemd unit dir");
        std::fs::write(
            unit_dir.join("rantaiclaw.service"),
            "[Unit]\nDescription=test\n",
        )
        .expect("write unit file");

        assert_eq!(detect_systemd_unit(), None);
        assert!(
            !fake.restart_marker().exists(),
            "fake systemctl must not be reached"
        );
    }
}
