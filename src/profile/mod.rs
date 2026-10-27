//! Profile manager — multi-profile storage layout for v0.5.0.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Profile manager" and §"Storage layout".
//!
//! A `Profile` is a self-contained directory under
//! `~/.rantaiclaw/profiles/<name>/` that holds config, persona, workspace,
//! memory, skills, sessions, policy, secrets and runtime state. The default
//! profile is auto-created on first run; users may create more via
//! `rantaiclaw profile create <name> [--clone <src>]`.
//!
//! Active-profile resolution precedence (first match wins):
//!   1. `RANTAICLAW_PROFILE` env var (set by the `--profile` CLI flag).
//!   2. `~/.rantaiclaw/active_profile` file contents.
//!   3. The literal string `"default"`.

pub mod commands;
pub mod config_override;
pub mod dev_guard;
pub mod migration;
pub mod paths;
pub mod sentinel;

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// A live profile on disk. Construction implies the directory tree exists
/// (callers must go through `ProfileManager::ensure*`).
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub root: PathBuf,
}

impl Profile {
    // Every accessor derives from `root`, the directory this profile actually
    // lives in, rather than re-deriving it from `name` through `paths`.
    //
    // The two are identical in production — `ProfileManager::ensure` sets
    // `root` to `paths::profile_dir(name)` and the struct doc requires callers
    // to go through it, so nothing builds a `Profile` whose root points
    // elsewhere. `profile_paths_match_the_global_layout` pins that.
    //
    // The difference is that `paths` resolves `$HOME` on every call, so a
    // `Profile` handed a scratch directory still wrote to the real
    // `~/.rantaiclaw`. Tests could only isolate by mutating the process-global
    // `HOME`, which raced every other test reading it — a smoke harness doing
    // exactly that measurably destabilised the suite. Reading `root` gives
    // them isolation with no environment mutation at all.
    pub fn config_toml(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn config_staging(&self) -> PathBuf {
        self.root.join("config.toml.staging")
    }
    pub fn workspace_dir(&self) -> PathBuf {
        self.root.join("workspace")
    }
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }
    pub fn sessions_db_path(&self) -> PathBuf {
        self.sessions_dir().join("sessions.db")
    }
    pub fn kb_db_path(&self) -> PathBuf {
        self.root.join("kb.db")
    }
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }
    pub fn persona_dir(&self) -> PathBuf {
        self.root.join("persona")
    }
    pub fn policy_dir(&self) -> PathBuf {
        self.root.join("policy")
    }
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }
    pub fn audit_log(&self) -> PathBuf {
        self.root.join("audit.log")
    }
}

/// Optional knobs for `ProfileManager::create` when `--clone` is set.
///
/// Defaults are conservative: do NOT copy memory or secrets across profiles.
/// Users opt in explicitly per spec §"Storage layout / Profile clone semantics".
#[derive(Debug, Clone, Copy, Default)]
pub struct CloneOpts {
    pub include_secrets: bool,
    pub include_memory: bool,
}

/// Stateless façade over the profiles directory.
///
/// All entry points are static — there is one process-wide profile root
/// (computed via `paths`) and we never cache it.
pub struct ProfileManager;

impl ProfileManager {
    /// Idempotently ensure the `default` profile exists; return its handle.
    pub fn ensure_default() -> Result<Profile> {
        Self::ensure("default")
    }

    /// Idempotently ensure a named profile and its full sub-tree exist.
    pub fn ensure(name: &str) -> Result<Profile> {
        validate_profile_name(name)?;
        let dir = paths::profile_dir(name);
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("create profile dir {}", dir.display()))?;
        }
        // Subdirectories — create lazily but unconditionally so partial
        // trees from earlier crashed runs are healed on next launch.
        for sub in &[
            "workspace",
            "memory",
            "sessions",
            "skills",
            "persona",
            "policy",
            "secrets",
        ] {
            let p = dir.join(sub);
            if !p.exists() {
                fs::create_dir_all(&p)
                    .with_context(|| format!("create profile subdir {}", p.display()))?;
            }
        }
        // Best-effort tighten on secrets/.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let secrets = dir.join("secrets");
            if let Ok(meta) = fs::metadata(&secrets) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                let _ = fs::set_permissions(&secrets, perms);
            }
        }
        Ok(Profile {
            name: name.to_string(),
            root: dir,
        })
    }

    /// Resolve + ensure the active profile in one call. The hot path for
    /// every config-reading entry point.
    ///
    /// This is the RESOLVER, not a switch: it reads the profile the
    /// environment names and never moves one. The rule that keeps an
    /// override from splitting an install lives at the doors that switch or
    /// select a profile — [`ProfileManager::use_profile_with`] for the
    /// switch, [`ProfileManager::require_selectable`] for a selection.
    pub fn active() -> Result<Profile> {
        let name = Self::resolve_active_name();
        // A name that names nothing must not silently become a new, empty
        // profile directory; `default` stays self-bootstrapping for the
        // first run.
        Self::require_existing(&name)?;
        Self::ensure(&name)
    }

    /// Pure resolution — no filesystem mutation. See module docstring for
    /// precedence.
    ///
    /// A resolved name that could escape the profiles directory (a
    /// hand-edited marker, an env var set to `../x`) is refused here: it
    /// would be joined onto the profiles path by every caller. The name
    /// falls back to `"default"` and the operator is told, rather than the
    /// path being built.
    pub fn resolve_active_name() -> String {
        let name = Self::resolve_active_name_raw();
        if let Err(error) = validate_profile_name(&name) {
            tracing::warn!(
                "Ignoring unusable active profile name {name:?} ({error}); using \"default\". \
                 The name came from RANTAICLAW_PROFILE or the active-profile marker."
            );
            return "default".to_string();
        }
        name
    }

    /// The resolution itself, without the safety net of
    /// [`ProfileManager::resolve_active_name`].
    fn resolve_active_name_raw() -> String {
        // 1. Env var (set by --profile flag in main.rs)
        if let Ok(name) = std::env::var("RANTAICLAW_PROFILE") {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        // 2. ~/.rantaiclaw/active_profile file
        if let Ok(s) = fs::read_to_string(paths::active_profile_file()) {
            let trimmed = s.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        // 3. Hard default
        "default".to_string()
    }

    /// Sorted list of existing profile names. Returns empty vec if the
    /// `profiles/` directory does not yet exist.
    pub fn list() -> Result<Vec<String>> {
        let root = paths::rantaiclaw_root().join("profiles");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut names: Vec<String> = vec![];
        for entry in fs::read_dir(&root).with_context(|| format!("readdir {}", root.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(s) = entry.file_name().to_str() {
                    names.push(s.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// Create a new profile, optionally cloning persona/skills from a source
    /// profile. See spec §"Profile clone semantics".
    pub fn create(name: &str, clone_from: Option<&str>, opts: CloneOpts) -> Result<Profile> {
        // A name that creates a profile is held to the stricter rules.
        validate_new_profile_name(name)?;
        let dst_dir = paths::profile_dir(name);
        if dst_dir.exists() {
            bail!("profile {name:?} already exists at {}", dst_dir.display());
        }
        // The clone source must EXIST. `ensure` would have created it, so a
        // typo in the source name silently cloned an empty profile — and the
        // check has to happen before anything is created, so a refused
        // clone leaves no directory behind.
        let src = match clone_from {
            Some(src_name) => Some(Self::existing(src_name)?),
            None => None,
        };
        let dst = Self::ensure(name)?;
        if let Some(src) = src {
            clone_into(&src, &dst, opts)?;
        }
        Ok(dst)
    }

    /// A profile that must already exist on disk.
    ///
    /// Unlike [`ProfileManager::ensure`] this never creates anything: it is
    /// the door for a name that NAMES an existing profile (a clone source).
    fn existing(name: &str) -> Result<Profile> {
        Self::require_existing(name)?;
        Ok(Profile {
            name: name.to_string(),
            root: paths::profile_dir(name),
        })
    }

    /// Refuse a profile name that does not exist on disk. `default` is
    /// exempt so the first run can still bootstrap it. The door for the
    /// `--profile` flag and for [`ProfileManager::active`]: a name that
    /// names nothing must not silently become a new, empty profile
    /// directory.
    pub fn require_existing(name: &str) -> Result<()> {
        validate_profile_name(name)?;
        if name == "default" || paths::profile_dir(name).exists() {
            return Ok(());
        }
        let dir = paths::profile_dir(name);
        bail!(
            "profile {name:?} does not exist at {}; create it with `rantaiclaw profile create {name}`",
            dir.display()
        )
    }

    /// The door a profile *selection* passes: the `--profile` flag and the
    /// `RANTAICLAW_PROFILE` environment variable.
    ///
    /// Two rules, in this order:
    ///
    /// 1. An override that pins the config directory refuses a profile it
    ///    does not keep in effect: the run would read the config from one
    ///    profile and the per-profile stores from another.
    /// 2. A name that names nothing is refused: it used to become a new,
    ///    empty profile directory. `default` stays bootstrappable for the
    ///    first run.
    pub fn require_selectable(name: &str) -> Result<()> {
        validate_profile_name(name)?;
        config_override::refuse_switch(name)?;
        Self::require_existing(name)
    }

    /// Set the active profile. Refuses unknown profile names so users get a
    /// clear error instead of a silent typo.
    pub fn use_profile(name: &str) -> Result<()> {
        let control = crate::daemon::handoff::detect_init_system();
        Self::use_profile_with(name, control.as_ref())
    }

    /// Test seam — same as [`ProfileManager::use_profile`] with a
    /// caller-chosen `DaemonControl`, so a test can prove which unit a
    /// profile switch restarts without an init system on the host.
    pub fn use_profile_with(
        name: &str,
        control: &dyn crate::daemon::handoff::DaemonControl,
    ) -> Result<()> {
        validate_profile_name(name)?;
        let dir = paths::profile_dir(name);
        if !dir.exists() {
            bail!("profile {name:?} does not exist at {}", dir.display());
        }
        // An override that pins the config directory refuses a switch to
        // another profile: the switch would move the per-profile stores and
        // leave the config behind.
        config_override::refuse_switch(name)?;
        // The profile the marker names BEFORE this switch — not
        // `resolve_active_name`, which prefers the env var. A daemon that
        // follows the marker is registered under the profile the marker
        // named, so that sentinel is the one a switch has to look at.
        let previous = Self::marker_profile();
        let path = paths::active_profile_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Atomic-ish write: tempfile + rename in same dir.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{name}\n"))?;
        fs::rename(&tmp, &path)?;

        // Hand the daemon that was serving the OLD profile over to the new
        // one. Errors here are downgraded to warnings — a stale daemon must
        // never block the profile switch itself.
        match crate::daemon::handoff::restart_daemon_for_profile_with(&previous, control) {
            Ok(outcome) => report_handoff_outcome(name, &outcome),
            Err(e) => eprintln!("Warning: daemon handoff for profile {name:?} failed: {e}"),
        }

        Ok(())
    }

    /// The profile the active-profile marker names, ignoring the env var.
    /// `"default"` when no marker is written yet — the resolution a daemon
    /// started before any switch used.
    pub fn marker_profile() -> String {
        fs::read_to_string(paths::active_profile_file())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_string())
    }

    /// Delete a profile. Refuses to delete the active profile unless
    /// `force` is true (caller has acknowledged the consequences).
    pub fn delete(name: &str, force: bool) -> Result<()> {
        validate_profile_name(name)?;
        let dir = paths::profile_dir(name);
        if !dir.exists() {
            bail!("profile {name:?} does not exist at {}", dir.display());
        }
        let active = Self::resolve_active_name();
        if active == name && !force {
            bail!(
                "refusing to delete active profile {name:?}; pass --force or `rantaiclaw profile use <other>` first"
            );
        }
        fs::remove_dir_all(&dir)
            .with_context(|| format!("remove profile dir {}", dir.display()))?;
        // If we just deleted the active profile, clear the marker so the
        // next launch falls back to "default" (and re-creates it).
        if active == name {
            let marker = paths::active_profile_file();
            let _ = fs::remove_file(&marker);
        }
        Ok(())
    }
}

/// Tell the operator what the switch did with the daemon that was serving
/// the previous profile. One line per outcome; a restart line names the
/// unit, a pinned/hand-started line says what will NOT move and why.
fn report_handoff_outcome(active: &str, outcome: &crate::daemon::handoff::HandoffOutcome) {
    use crate::daemon::handoff::HandoffOutcome as H;
    match outcome {
        H::Restarted { unit } => {
            println!("Restarted {unit} — the daemon now follows profile {active:?}");
        }
        H::Pinned { profile, unit } => {
            println!(
                "The daemon for profile {profile:?} stays on {profile:?}: its service pins \
                 that profile ({unit}). Run `rantaiclaw service install` from the new profile \
                 to change that."
            );
        }
        H::HandStarted { profile } => {
            println!(
                "The daemon for profile {profile:?} was started by hand; restart it yourself \
                 to pick up profile {active:?}."
            );
        }
        H::NotRunning => {}
        H::NoInit { profile, pid } => {
            println!(
                "The daemon for profile {profile:?} (pid {pid}) is running, but no supported \
                 init system was detected (systemd / launchd); restart it yourself to pick up \
                 profile {active:?}."
            );
        }
    }
}

/// Reject profile names that would escape the profiles dir or land on weird
/// filesystem entries. Restrict to a Unix-friendly subset.
///
/// This is the check for a name that NAMES AN EXISTING PROFILE: the
/// `use`, `delete`, `--profile`, env-var, marker and sentinel doors. It
/// only refuses what could escape the profiles directory, so a profile
/// created before [`validate_new_profile_name`] existed stays usable.
///
/// A name that CREATES a profile goes through [`validate_new_profile_name`],
/// which is stricter.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("profile name cannot be empty");
    }
    if name == "." || name == ".." {
        bail!("profile name cannot be '.' or '..'");
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        bail!("profile name {name:?} contains forbidden characters");
    }
    Ok(())
}

/// The rules a NEW profile name must satisfy.
///
/// A new name becomes a directory name, a systemd instance name and part of
/// a unit file, so it is restricted to a subset that is safe in all three:
/// letters, digits, `-` and `_`, at most 64 characters, and not starting
/// with `-` (a leading dash reads as a unit option).
///
/// An existing profile is not held to this: a profile created before the
/// rule existed keeps working, and only its path safety is checked
/// ([`validate_profile_name`]).
pub fn validate_new_profile_name(name: &str) -> Result<()> {
    validate_profile_name(name)?;
    if name.chars().count() > 64 {
        bail!("profile name {name:?} is longer than 64 characters");
    }
    if name.starts_with('-') {
        bail!("profile name {name:?} cannot start with '-'");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_'))
    {
        bail!(
            "profile name {name:?} contains {bad:?}; a new profile name may hold only letters, \
             digits, '-' and '_'"
        );
    }
    Ok(())
}

fn clone_into(src: &Profile, dst: &Profile, opts: CloneOpts) -> Result<()> {
    // Files cloned at root: config.toml (non-secret keys are simply the file as-is;
    // secret encryption is opt-in per spec).
    let cfg_src = src.config_toml();
    if cfg_src.exists() {
        let cfg_dst = dst.config_toml();
        if let Some(parent) = cfg_dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&cfg_src, &cfg_dst)?;
    }

    // Directories cloned by default.
    copy_dir_all(&src.persona_dir(), &dst.persona_dir())?;

    // policy/ — not cloned: the clone starts with a fresh policy, and the
    // credential floor is in code, so there is no forbidden-paths file to carry.

    copy_dir_all(&src.skills_dir(), &dst.skills_dir())?;

    if opts.include_memory {
        copy_dir_all(&src.memory_dir(), &dst.memory_dir())?;
    }
    if opts.include_secrets {
        copy_dir_all(&src.secrets_dir(), &dst.secrets_dir())?;
    }
    // workspace/, sessions/, audit.log never copied — see spec table.
    Ok(())
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &target)?;
        }
        // symlinks intentionally skipped — clone semantics are file-data only.
    }
    Ok(())
}

#[cfg(test)]
mod profile_root_tests {
    use super::*;

    #[tokio::test]
    async fn profile_paths_match_the_global_layout() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp home");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("RANTAICLAW_PROFILE");
        let profile = ProfileManager::ensure_default().expect("profile");
        let name = profile.name.clone();
        assert_eq!(profile.root, paths::profile_dir(&name));
        assert_eq!(profile.skills_dir(), paths::skills_dir(&name));
        assert_eq!(profile.persona_dir(), paths::persona_dir(&name));
        assert_eq!(profile.policy_dir(), paths::policy_dir(&name));
        assert_eq!(profile.sessions_db_path(), paths::sessions_db(&name));
        match prev {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
    }

    /// The reproduction: the daemon was serving the profile the MARKER named
    /// (`default`), and `profile use work` must restart THAT daemon's unit —
    /// the unit comes back and follows the marker onto `work`. The handoff
    /// used to read the NEW profile's sentinel, found nothing, and never
    /// restarted anything.
    #[test]
    fn profile_use_restarts_the_unit_of_the_daemon_following_the_marker() {
        use crate::daemon::handoff::{DaemonControl, HandoffOutcome};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Stub {
            restarts: AtomicUsize,
            units: std::sync::Mutex<Vec<String>>,
        }
        impl DaemonControl for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn restart(&self, unit: &str) -> Result<()> {
                self.restarts.fetch_add(1, Ordering::SeqCst);
                self.units.lock().unwrap().push(unit.to_string());
                Ok(())
            }
            fn is_active(&self, _unit: &str) -> Result<bool> {
                Ok(true)
            }
        }

        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
        std::env::remove_var("RANTAICLAW_PROFILE");
        // No config-dir override in this test: an override that pins the
        // config refuses a profile switch, because the switch would move the
        // per-profile stores and leave the config behind.
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ProfileManager::ensure("default").unwrap();
            ProfileManager::ensure("work").unwrap();
            // The marker names `default`; the live daemon serving it is unit-
            // managed and follows the marker.
            std::fs::write(paths::active_profile_file(), "default\n").unwrap();
            crate::profile::sentinel::write_sentinel(
                "default",
                &crate::profile::sentinel::DaemonSentinel {
                    pid: std::process::id(),
                    unit: Some("rantaiclaw.service".to_string()),
                    started_at: None,
                    profile_pinned: false,
                },
            )
            .unwrap();

            let stub = std::sync::Arc::new(Stub {
                restarts: AtomicUsize::new(0),
                units: std::sync::Mutex::new(vec![]),
            });
            ProfileManager::use_profile_with("work", stub.as_ref()).unwrap();

            assert_eq!(
                stub.restarts.load(Ordering::SeqCst),
                1,
                "the daemon following the old marker must be restarted exactly once"
            );
            assert_eq!(
                stub.units.lock().unwrap().clone(),
                vec!["rantaiclaw.service".to_string()]
            );
            assert_eq!(
                ProfileManager::marker_profile(),
                "work",
                "the marker must name the new profile after the switch"
            );
            let _ = HandoffOutcome::NotRunning; // variant exhaustiveness guard
        }));
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match prev_profile {
            Some(p) => std::env::set_var("RANTAICLAW_PROFILE", p),
            None => std::env::remove_var("RANTAICLAW_PROFILE"),
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    /// The pinned twin: a daemon whose launcher pinned its profile must NOT
    /// be restarted by a `profile use` — a restart cannot move it.
    #[test]
    fn profile_use_leaves_a_pinned_daemon_unrestarted() {
        use crate::daemon::handoff::{DaemonControl, HandoffOutcome};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Stub {
            restarts: AtomicUsize,
        }
        impl DaemonControl for Stub {
            fn name(&self) -> &str {
                "stub"
            }
            fn restart(&self, _unit: &str) -> Result<()> {
                self.restarts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn is_active(&self, _unit: &str) -> Result<bool> {
                Ok(true)
            }
        }

        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
        std::env::remove_var("RANTAICLAW_PROFILE");
        // No config-dir override in this test: an override that pins the
        // config refuses a profile switch, because the switch would move the
        // per-profile stores and leave the config behind.
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ProfileManager::ensure("default").unwrap();
            ProfileManager::ensure("work").unwrap();
            std::fs::write(paths::active_profile_file(), "default\n").unwrap();
            crate::profile::sentinel::write_sentinel(
                "default",
                &crate::profile::sentinel::DaemonSentinel {
                    pid: std::process::id(),
                    unit: Some("rantaiclaw.service".to_string()),
                    started_at: None,
                    profile_pinned: true,
                },
            )
            .unwrap();

            let stub = std::sync::Arc::new(Stub {
                restarts: AtomicUsize::new(0),
            });
            ProfileManager::use_profile_with("work", stub.as_ref()).unwrap();

            assert_eq!(
                stub.restarts.load(Ordering::SeqCst),
                0,
                "a pinned daemon must not be restarted"
            );
            let _ = HandoffOutcome::NotRunning; // variant exhaustiveness guard
        }));
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match prev_profile {
            Some(p) => std::env::set_var("RANTAICLAW_PROFILE", p),
            None => std::env::remove_var("RANTAICLAW_PROFILE"),
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn a_scratch_root_keeps_every_path_inside_it() {
        let tmp = tempfile::tempdir().expect("scratch root");
        let profile = Profile {
            name: "scratch".to_string(),
            root: tmp.path().to_path_buf(),
        };
        for path in [
            profile.config_toml(),
            profile.skills_dir(),
            profile.persona_dir(),
            profile.policy_dir(),
            profile.secrets_dir(),
        ] {
            assert!(
                path.starts_with(tmp.path()),
                "{path:?} escaped the scratch root"
            );
        }
    }

    /// D2: while `RANTAICLAW_CONFIG_DIR` pins the config directory, a
    /// switch to another profile is refused — the switch would move the
    /// per-profile stores and leave the config, memory and cron behind.
    /// This is the split the audit saw.
    #[test]
    fn a_config_dir_override_refuses_a_switch_to_another_profile() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let scratch = tempfile::tempdir().expect("scratch config dir");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", scratch.path());

        ProfileManager::ensure("default").unwrap();
        ProfileManager::ensure("work").unwrap();
        std::fs::write(paths::active_profile_file(), "default\n").unwrap();

        let error = ProfileManager::use_profile("work").expect_err("the switch must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("RANTAICLAW_CONFIG_DIR"),
            "the refusal must name the override: {message}"
        );
        assert!(
            message.contains("unset RANTAICLAW_CONFIG_DIR"),
            "the refusal must say how to clear the override: {message}"
        );
        assert_eq!(
            ProfileManager::marker_profile(),
            "default",
            "a refused switch must not move the marker"
        );
    }

    /// The pinned profile itself stays switchable: an override that pins a
    /// profile directory keeps THAT profile in effect, and a switch to it
    /// changes nothing.
    #[test]
    fn a_switch_to_the_profile_an_override_pins_is_allowed() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let pinned = tmp.path().join(".rantaiclaw/profiles/work");
        let _g_config_dir = crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", &pinned);

        ProfileManager::ensure("default").unwrap();
        ProfileManager::ensure("work").unwrap();
        std::fs::write(paths::active_profile_file(), "default\n").unwrap();

        ProfileManager::use_profile("work").expect("the pinned profile is the one in effect");
        assert_eq!(ProfileManager::marker_profile(), "work");
    }

    /// Without an override, a switch works as before.
    #[test]
    fn a_switch_without_an_override_is_not_refused() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");

        ProfileManager::ensure("default").unwrap();
        ProfileManager::ensure("work").unwrap();

        ProfileManager::use_profile("work").expect("no override, no refusal");
        assert_eq!(ProfileManager::marker_profile(), "work");
    }

    /// A marker that names a directory that is not a profile directory of
    /// this root stays an override, so it refuses a switch too.
    #[test]
    fn an_override_marker_refuses_a_switch_to_another_profile() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");

        let root = tmp.path().join(".rantaiclaw");
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(
            root.join(config_override::ACTIVE_WORKSPACE_MARKER),
            format!(
                "config_dir = {:?}\n",
                root.join("elsewhere").to_string_lossy()
            ),
        )
        .unwrap();

        ProfileManager::ensure("default").unwrap();
        ProfileManager::ensure("work").unwrap();

        let error = ProfileManager::use_profile("work").expect_err("the switch must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains(config_override::ACTIVE_WORKSPACE_MARKER),
            "the refusal must name the marker: {message}"
        );
    }

    /// A clone source that does not exist is an error, and nothing is
    /// created: `ensure` used to build the missing source as an empty
    /// profile, so a typo silently cloned nothing.
    #[test]
    fn cloning_a_missing_source_creates_nothing() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        let error = ProfileManager::create("copy1", Some("nosuch"), CloneOpts::default())
            .expect_err("the source must exist");
        assert!(
            format!("{error:#}").contains("nosuch"),
            "the refusal must name the missing source: {error:#}"
        );
        assert!(
            !paths::profile_dir("copy1").exists(),
            "a refused clone must not create the destination"
        );
        // The direct `clone` command goes through the same door.
        let error = ProfileManager::create("copy2", Some("nosuch"), CloneOpts::default())
            .expect_err("the source must exist");
        assert!(format!("{error:#}").contains("nosuch"), "{error:#}");
        assert!(!paths::profile_dir("copy2").exists());
    }

    /// A new profile name is restricted to the unit-safe subset; an
    /// existing profile created before the rule keeps working.
    #[test]
    fn a_new_profile_name_is_unit_safe_but_an_old_name_still_loads() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        for bad in ["bad name!", "-lead", "../escape", &"x".repeat(65)] {
            let error = ProfileManager::create(bad, None, CloneOpts::default())
                .expect_err("a new name must satisfy the unit-safe rules");
            assert!(
                !format!("{error:#}").is_empty(),
                "the refusal must say why: {bad:?}"
            );
            assert!(
                !paths::profile_dir(bad).exists(),
                "a refused name must not create a directory: {bad:?}"
            );
        }
        ProfileManager::create("Work_2", None, CloneOpts::default())
            .expect("letters, digits, '-' and '_' are fine");

        // An existing directory with an old-style name still loads: the
        // path-safety rules are the only ones applied to it.
        let legacy = paths::profile_dir("old name");
        std::fs::create_dir_all(&legacy).unwrap();
        ProfileManager::use_profile("old name")
            .expect("a profile created before the rule stays usable");
        assert_eq!(ProfileManager::marker_profile(), "old name");
    }

    /// D2, selection door: a profile selected for this invocation — by the
    /// `--profile` flag or by `RANTAICLAW_PROFILE` — is refused while an
    /// override keeps another profile in effect. It is the same one
    /// sentence the switch door gives.
    #[test]
    fn an_override_refuses_a_selection_it_does_not_pin() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let scratch = tempfile::tempdir().expect("scratch config dir");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", scratch.path());
        let _g_profile = crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "work");

        ProfileManager::ensure("work").unwrap();

        let error =
            ProfileManager::require_selectable("work").expect_err("the selection must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("RANTAICLAW_CONFIG_DIR") && message.contains("work"),
            "the refusal must name the override and the profile: {message}"
        );
    }

    /// The resolver is not a switch: it reads the profile the environment
    /// names and never refuses one because an override is in effect. The
    /// refusal belongs to the doors that switch or select a profile, so a
    /// run that only reads a profile is not stopped by the rule that keeps
    /// a switch from splitting an install.
    #[test]
    fn the_resolver_follows_the_environment_under_an_override() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let scratch = tempfile::tempdir().expect("scratch config dir");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", scratch.path());
        let _g_profile = crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "work");

        ProfileManager::ensure("work").unwrap();

        let profile = ProfileManager::active().expect("the profile resolves");
        assert_eq!(profile.name, "work", "the resolver follows the environment");
    }

    /// The existence door: `--profile` and `active()` refuse a name that
    /// does not exist, where the name used to silently become a new, empty
    /// profile directory. `default` stays bootstrappable for the first run.
    #[test]
    fn a_profile_name_that_names_nothing_is_refused_by_the_existence_door() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        let error = ProfileManager::require_existing("work").expect_err("nothing exists yet");
        let message = format!("{error:#}");
        assert!(
            message.contains("work") && message.contains("profile create work"),
            "the refusal must name the profile and the remedy: {message}"
        );

        ProfileManager::require_existing("default")
            .expect("default is exempt: the first run bootstraps it");
        ProfileManager::ensure_default().unwrap();
        ProfileManager::require_existing("default").unwrap();

        ProfileManager::ensure("work").unwrap();
        ProfileManager::require_existing("work").expect("an existing profile passes the door");
    }

    /// `active()` refuses a resolved name that does not exist — from the
    /// environment or from the marker — where it used to create the
    /// directory. A stale marker is healed by switching back to `default`,
    /// which does not go through `active()`.
    #[test]
    fn active_refuses_a_resolved_profile_that_does_not_exist() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        let g_profile = crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "work");
        let error = ProfileManager::active().expect_err("work does not exist");
        assert!(
            format!("{error:#}").contains("profile create work"),
            "{error:#}"
        );
        assert!(
            !paths::profile_dir("work").exists(),
            "a refused name must not create the directory"
        );
        drop(g_profile);

        ProfileManager::ensure_default().unwrap();
        std::fs::write(paths::active_profile_file(), "ghost\n").unwrap();
        let error = ProfileManager::active().expect_err("ghost does not exist");
        assert!(format!("{error:#}").contains("ghost"), "{error:#}");
        assert!(!paths::profile_dir("ghost").exists());

        ProfileManager::use_profile("default").expect("the switch back heals a stale marker");
        ProfileManager::active().expect("default exists again");
    }

    /// A name from the marker or the environment that could escape the
    /// profiles directory is refused where paths are built from it.
    #[test]
    fn a_name_that_could_escape_the_profiles_dir_never_becomes_a_path() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::tempdir().expect("temp home");
        let _g_home = crate::test_env::EnvGuard::set("HOME", tmp.path());
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        ProfileManager::ensure_default().unwrap();
        std::fs::write(paths::active_profile_file(), "../escape\n").unwrap();
        assert_eq!(
            ProfileManager::resolve_active_name(),
            "default",
            "an escaping marker name falls back to default, loudly"
        );
        // And the sentinel path builder refuses it outright.
        assert!(
            crate::profile::sentinel::sentinel_path("../escape").is_err(),
            "an escaping name must not be joined onto the profiles dir"
        );
    }
}
