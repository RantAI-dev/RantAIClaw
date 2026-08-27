//! Daemon handoff on profile switch.
//!
//! When the user runs `rantaiclaw profile use <name>`, any background
//! daemon running for the previously-active profile holds stale config in
//! memory. This module bridges the gap: detect the host's init system,
//! find the right unit, and ask the init system to restart it.
//!
//! Behaviour is intentionally best-effort:
//! - On a host with neither `systemctl` nor `launchctl` we print a
//!   friendly skip notice and return Ok — profile switching never blocks.
//! - If no `.daemon_active` sentinel exists for this profile we no-op.
//! - Restart errors bubble up but the caller in `ProfileManager::use_profile`
//!   downgrades them to warnings.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md` —
//! Wave 4B "Daemon handoff on profile switch".

use std::process::Command;

use anyhow::{Context, Result};

use crate::profile::sentinel;

/// Pluggable init-system control surface. Tests inject a stub; production
/// uses `Systemd`, `Launchd`, or `None`.
pub trait DaemonControl: Send + Sync {
    /// Short name for log lines / user messages, e.g. `"systemd"`.
    fn name(&self) -> &str;
    /// Restart the named unit. Implementations swallow stdout/stderr by
    /// default — wrap with `--user`/`--global` etc. as appropriate.
    fn restart(&self, unit: &str) -> Result<()>;
    /// Best-effort check: is the unit currently active?
    fn is_active(&self, unit: &str) -> Result<bool>;
}

/// Linux user-level systemd via `systemctl --user`.
pub struct Systemd;

impl DaemonControl for Systemd {
    fn name(&self) -> &str {
        "systemd"
    }

    fn restart(&self, unit: &str) -> Result<()> {
        // daemon-reload first so a freshly-edited unit file is picked up;
        // ignore its exit since `restart` will surface real errors.
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let status = Command::new("systemctl")
            .args(["--user", "restart", unit])
            .status()
            .with_context(|| format!("spawn `systemctl --user restart {unit}`"))?;
        if !status.success() {
            anyhow::bail!("`systemctl --user restart {unit}` exited with {status}");
        }
        Ok(())
    }

    fn is_active(&self, unit: &str) -> Result<bool> {
        let output = Command::new("systemctl")
            .args(["--user", "is-active", unit])
            .output()
            .with_context(|| format!("spawn `systemctl --user is-active {unit}`"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim() == "active")
    }
}

/// macOS user-level launchd via `launchctl`.
pub struct Launchd;

impl DaemonControl for Launchd {
    fn name(&self) -> &str {
        "launchd"
    }

    fn restart(&self, unit: &str) -> Result<()> {
        // launchd has no first-class "restart"; kickstart -k cycles a
        // running service. Caller passes the launchd label
        // (`com.rantaiclaw.daemon`) here; the gui/<uid> domain prefix is
        // added so we don't need root.
        let target = if unit.contains('/') {
            unit.to_string()
        } else {
            let uid = users_uid();
            format!("gui/{uid}/{unit}")
        };
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .status()
            .with_context(|| format!("spawn `launchctl kickstart -k {target}`"))?;
        if !status.success() {
            anyhow::bail!("`launchctl kickstart -k {target}` exited with {status}");
        }
        Ok(())
    }

    fn is_active(&self, unit: &str) -> Result<bool> {
        let output = Command::new("launchctl")
            .args(["list"])
            .output()
            .context("spawn `launchctl list`")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().any(|line| line.contains(unit)))
    }
}

/// Sentinel impl returned when neither systemd nor launchd is detected.
/// Everything is a no-op so callers don't have to special-case "no init".
pub struct NoInit;

impl DaemonControl for NoInit {
    fn name(&self) -> &str {
        "none"
    }

    fn restart(&self, _unit: &str) -> Result<()> {
        // Caller already printed a skip notice in
        // `restart_daemon_for_profile`; nothing to do.
        Ok(())
    }

    fn is_active(&self, _unit: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Detect the host's init system. Order:
///   1. `which systemctl` → `Systemd`
///   2. `which launchctl` → `Launchd`
///   3. otherwise → `NoInit`
///
/// We use `which::which` (already a workspace dependency) rather than
/// hard-coded `/run/systemd/system` checks so the same code path works in
/// containers and rootless setups.
pub fn detect_init_system() -> Box<dyn DaemonControl> {
    if which::which("systemctl").is_ok() {
        return Box::new(Systemd);
    }
    if which::which("launchctl").is_ok() {
        return Box::new(Launchd);
    }
    Box::new(NoInit)
}

/// Default unit name for a given profile.
///
/// Matches the systemd `rantaiclaw@.service` template convention: a single
/// template unit takes the profile as its instance argument so users can
/// run multiple profiles simultaneously.
pub fn default_unit_name(profile: &str) -> String {
    format!("rantaiclaw@{profile}.service")
}

/// Top-level entry point — invoked from `ProfileManager::use_profile`.
///
/// Flow:
/// 1. If no daemon sentinel exists for this profile → no-op.
/// 2. Detect init system; if none → print friendly skip notice, return Ok.
/// 3. Resolve unit name (sentinel override, falling back to default).
/// 4. Restart via the resolved `DaemonControl`.
/// 5. Print a one-line success message.
pub fn restart_daemon_for_profile(profile: &str) -> Result<()> {
    let control = detect_init_system();
    restart_daemon_for_profile_with(profile, control.as_ref())
}

/// Test seam — same as `restart_daemon_for_profile` but with a caller-chosen
/// `DaemonControl`. Exposed `pub` so integration tests can substitute a
/// recording stub.
pub fn restart_daemon_for_profile_with(profile: &str, control: &dyn DaemonControl) -> Result<()> {
    let Some(sentinel) = sentinel::read_sentinel(profile).ok().flatten() else {
        // Either no sentinel, or unparseable — both mean "nothing live for
        // this profile". `read_sentinel` returns Ok(None) for absent;
        // `.ok().flatten()` also handles parse failures the same way
        // (advisory file).
        tracing::debug!("No daemon sentinel for profile {profile:?}; skipping handoff");
        return Ok(());
    };

    // The recorded daemon is gone (crashed, or the operator stopped it) — the
    // sentinel is stale. Do NOT restart: resurrecting a daemon the operator
    // isn't running would be a surprise on `profile use`. Clear the stale
    // sentinel and skip. `pid_is_live_daemon` also guards against PID reuse.
    if !sentinel::pid_is_live_daemon(sentinel.pid) {
        tracing::debug!(
            "Daemon sentinel for {profile:?} is stale (pid {} not a live daemon); clearing",
            sentinel.pid
        );
        let _ = sentinel::clear_sentinel(profile);
        return Ok(());
    }

    if control.name() == "none" {
        eprintln!(
            "Note: profile {profile:?} has a registered daemon (pid {pid}) but no \
             supported init system was detected (systemd / launchd). \
             If the daemon is running manually, restart it yourself to pick up \
             the new profile.",
            pid = sentinel.pid,
        );
        return Ok(());
    }

    let unit = sentinel
        .unit
        .clone()
        .unwrap_or_else(|| default_unit_name(profile));

    // Only cycle a unit the init system reports as active. Some controllers
    // treat `restart` as start-if-stopped, so an unconditional restart could
    // launch a daemon the operator had deliberately stopped.
    if !control
        .is_active(&unit)
        .with_context(|| format!("query active state of {unit} via {}", control.name()))?
    {
        tracing::debug!("Unit {unit} is not active; skipping restart for {profile:?}");
        return Ok(());
    }

    control
        .restart(&unit)
        .with_context(|| format!("restart {unit} via {}", control.name()))?;

    println!("Restarted {unit}");
    Ok(())
}

/// Best-effort current uid for launchd `gui/<uid>/<label>` targeting. We
/// avoid pulling another crate just for this; `id -u` is universally
/// available on macOS.
#[cfg(target_os = "macos")]
fn users_uid() -> String {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "0".to_string())
}

#[cfg(not(target_os = "macos"))]
fn users_uid() -> String {
    // Non-macOS hosts never reach the launchd path in production, but the
    // helper is referenced from generic code; return a placeholder so it
    // compiles cross-platform.
    "0".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Recording stub — counts restart calls and logs the unit names.
    pub struct RecordingControl {
        pub name: String,
        pub restarts: AtomicUsize,
        pub units: Mutex<Vec<String>>,
        pub fail_with: Mutex<Option<String>>,
        /// What `is_active` reports. Defaults to `true` so restart-path tests
        /// exercise the restart unless they explicitly mark the unit inactive.
        pub active: AtomicBool,
    }

    impl RecordingControl {
        pub fn new(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                restarts: AtomicUsize::new(0),
                units: Mutex::new(vec![]),
                fail_with: Mutex::new(None),
                active: AtomicBool::new(true),
            })
        }

        pub fn set_active(&self, active: bool) {
            self.active.store(active, Ordering::SeqCst);
        }
    }

    impl DaemonControl for RecordingControl {
        fn name(&self) -> &str {
            &self.name
        }
        fn restart(&self, unit: &str) -> Result<()> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            self.units.lock().unwrap().push(unit.to_string());
            if let Some(msg) = self.fail_with.lock().unwrap().clone() {
                anyhow::bail!(msg);
            }
            Ok(())
        }
        fn is_active(&self, _unit: &str) -> Result<bool> {
            Ok(self.active.load(Ordering::SeqCst))
        }
    }

    // Handoff tests touch the process-global HOME (profile paths resolve from
    // it); serialize against the crate-shared ENV_LOCK like the sentinel tests.
    fn with_home<F: FnOnce()>(f: F) {
        let _g = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        if let Some(h) = prev {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    fn write_sentinel_for(profile: &str, pid: u32, unit: Option<&str>) {
        crate::profile::ProfileManager::ensure(profile).unwrap();
        crate::profile::sentinel::write_sentinel(
            profile,
            &crate::profile::sentinel::DaemonSentinel {
                pid,
                unit: unit.map(str::to_string),
                started_at: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn dead_pid_does_not_restart_and_clears_sentinel() {
        with_home(|| {
            // 2^31-2 is effectively never a live PID.
            write_sentinel_for("alpha", 2_147_483_646, Some("rantaiclaw.service"));
            let stub = RecordingControl::new("systemd");
            restart_daemon_for_profile_with("alpha", stub.as_ref()).unwrap();
            assert_eq!(stub.restarts.load(Ordering::SeqCst), 0);
            assert!(
                !crate::profile::sentinel::is_daemon_active("alpha"),
                "stale sentinel should be cleared"
            );
        });
    }

    #[test]
    fn inactive_unit_does_not_restart() {
        with_home(|| {
            write_sentinel_for("alpha", std::process::id(), Some("rantaiclaw.service"));
            let stub = RecordingControl::new("systemd");
            stub.set_active(false);
            restart_daemon_for_profile_with("alpha", stub.as_ref()).unwrap();
            assert_eq!(stub.restarts.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn live_active_daemon_restarts_recorded_unit() {
        with_home(|| {
            // The current test process is a RantaiClaw binary from the guard's
            // point of view (comm matches self), so it counts as a live daemon.
            write_sentinel_for("alpha", std::process::id(), Some("rantaiclaw.service"));
            let stub = RecordingControl::new("systemd");
            stub.set_active(true);
            restart_daemon_for_profile_with("alpha", stub.as_ref()).unwrap();
            assert_eq!(stub.restarts.load(Ordering::SeqCst), 1);
            assert_eq!(
                stub.units.lock().unwrap().clone(),
                vec!["rantaiclaw.service".to_string()]
            );
        });
    }

    #[test]
    fn default_unit_name_uses_template_form() {
        assert_eq!(default_unit_name("default"), "rantaiclaw@default.service");
        assert_eq!(default_unit_name("work"), "rantaiclaw@work.service");
    }

    #[test]
    fn no_init_restart_is_noop() {
        let n = NoInit;
        assert_eq!(n.name(), "none");
        n.restart("rantaiclaw@x.service").unwrap();
        assert!(!n.is_active("rantaiclaw@x.service").unwrap());
    }

    #[test]
    fn recording_stub_counts_calls() {
        let stub = RecordingControl::new("stub");
        stub.restart("rantaiclaw@a.service").unwrap();
        stub.restart("rantaiclaw@b.service").unwrap();
        assert_eq!(stub.restarts.load(Ordering::SeqCst), 2);
        assert_eq!(
            stub.units.lock().unwrap().clone(),
            vec![
                "rantaiclaw@a.service".to_string(),
                "rantaiclaw@b.service".to_string()
            ],
        );
    }
}
