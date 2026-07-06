//! `rantaiclaw uninstall` — remove profile data, optionally the binary.
//!
//! The transaction:
//!
//! 1. (optional, `--all`/`--purge`) tear down any installed daemon service unit.
//! 2. Remove the profile data directory (single profile by default; all
//!    profiles with `--all`).
//! 3. (optional, `--purge`) self-delete the binary itself.
//! 4. (best-effort) comment out the PATH amendment the installer added to
//!    the user's shell rc files.
//!
//! Each step is logged. `--dry-run` prints what would happen and exits 0.
//! `--keep-secrets` preserves `~/.rantaiclaw/.secret_key` so a future
//! re-install can decrypt prior encrypted credentials.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::lifecycle::binary_path::{BinaryInfo, InstallKind};
use crate::profile::{paths, sentinel, ProfileManager};

#[derive(Debug, Clone)]
pub struct UninstallOpts {
    /// Remove every profile + the global root, not just the active profile.
    pub all: bool,
    /// `--all` plus self-delete the binary.
    pub purge: bool,
    /// Preserve `~/.rantaiclaw/.secret_key` so re-install can decrypt.
    pub keep_secrets: bool,
    /// Skip the y/N prompt.
    pub yes: bool,
    /// Print what would be removed; touch nothing.
    pub dry_run: bool,
}

pub fn run(opts: UninstallOpts) -> Result<()> {
    let info = BinaryInfo::detect()?;

    let scope = Scope::resolve(&opts);
    // Detect daemons still bound to the profiles we're about to wipe. A live
    // daemon rewrites its profile dir every few seconds (daemon_state.json,
    // workspace), so removing data without stopping it leaves the install
    // looking "not uninstalled". Read sentinels now, before any deletion
    // removes them.
    let daemons = running_daemons(&scope.profiles);
    print_plan(&scope, &opts, &info, &daemons);

    if !opts.yes && !opts.dry_run && !confirm(&scope, &daemons) {
        println!("aborted");
        return Ok(());
    }

    if opts.dry_run {
        return Ok(());
    }

    // 1. Stop daemons before touching data. Foreground daemons are signalled
    //    directly by PID; service-managed units are torn down via
    //    `service uninstall` below (which also removes the unit file).
    stop_foreground_daemons(&daemons);
    if scope.touch_global {
        try_uninstall_daemon();
    }

    // 2. Remove profile data.
    for path in &scope.dirs_to_remove {
        if path.exists() {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
            println!("  removed {}", path.display());
        }
    }
    for path in &scope.files_to_remove {
        if path.exists() {
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            println!("  removed {}", path.display());
        }
    }

    // 3. Self-delete the binary (purge only).
    if opts.purge {
        if matches!(info.kind, InstallKind::Cargo) {
            eprintln!(
                "⚠ skipping --purge: cargo-installed binary at {}.\n  \
                 Run `cargo uninstall rantaiclaw` to remove.",
                info.path.display()
            );
        } else if matches!(info.kind, InstallKind::Workspace) {
            eprintln!(
                "⚠ skipping --purge: binary is running from a cargo workspace at {}.",
                info.path.display()
            );
        } else {
            self_delete_binary(&info.path)?;
        }
    }

    // 4. Best-effort PATH amendment cleanup.
    if scope.touch_global {
        let _ = clean_shell_rc_amendments();
    }

    print_summary(&scope, &opts, &info);
    Ok(())
}

#[derive(Debug)]
struct Scope {
    dirs_to_remove: Vec<PathBuf>,
    files_to_remove: Vec<PathBuf>,
    touch_global: bool,
    /// Profiles whose data is being removed — used to find daemons bound to
    /// them so they can be stopped before their profile dir is deleted.
    profiles: Vec<String>,
}

impl Scope {
    fn resolve(opts: &UninstallOpts) -> Self {
        if opts.all || opts.purge {
            // Wipe the entire ~/.rantaiclaw tree (minus .secret_key when
            // --keep-secrets is set).
            let root = paths::rantaiclaw_root();
            let mut dirs = vec![];
            let mut files = vec![];
            if root.exists() {
                if opts.keep_secrets {
                    // Walk top-level: remove everything except .secret_key.
                    if let Ok(entries) = fs::read_dir(&root) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            let name = entry.file_name();
                            if name == ".secret_key" {
                                continue;
                            }
                            if path.is_dir() {
                                dirs.push(path);
                            } else {
                                files.push(path);
                            }
                        }
                    }
                } else {
                    dirs.push(root);
                }
            }
            Self {
                dirs_to_remove: dirs,
                files_to_remove: files,
                touch_global: true,
                profiles: ProfileManager::list().unwrap_or_default(),
            }
        } else {
            // Default: only the active profile directory.
            let active = ProfileManager::resolve_active_name();
            let dir = paths::profile_dir(&active);
            let dirs = if dir.exists() { vec![dir] } else { vec![] };
            Self {
                dirs_to_remove: dirs,
                files_to_remove: vec![],
                touch_global: false,
                profiles: vec![active],
            }
        }
    }
}

/// A daemon found bound to a profile via its `.daemon_active` sentinel, whose
/// process is still alive.
#[derive(Debug, Clone)]
struct RunningDaemon {
    profile: String,
    pid: u32,
    /// `Some` when registered with a service manager (systemd/launchd); such
    /// daemons are removed via `service uninstall`, not a direct signal.
    unit: Option<String>,
}

/// Read each profile's sentinel and keep the ones whose PID is still alive.
/// Stale sentinels (crashed daemon, reused-or-dead PID) are dropped so we
/// never claim to stop — or signal — a process that isn't there.
fn running_daemons(profiles: &[String]) -> Vec<RunningDaemon> {
    let mut out = vec![];
    for profile in profiles {
        if let Ok(Some(s)) = sentinel::read_sentinel(profile) {
            if process_is_alive(s.pid) {
                out.push(RunningDaemon {
                    profile: profile.clone(),
                    pid: s.pid,
                    unit: s.unit,
                });
            }
        }
    }
    out
}

/// Signal foreground (non-service-managed) daemons to stop. Service-managed
/// units are left to `try_uninstall_daemon` / `service uninstall`.
fn stop_foreground_daemons(daemons: &[RunningDaemon]) {
    for d in daemons {
        if d.unit.is_some() {
            continue;
        }
        if stop_daemon_process(d.pid) {
            println!("  stopped daemon pid {} (profile {})", d.pid, d.profile);
        } else {
            eprintln!(
                "  ⚠ could not stop daemon pid {} (profile {}); it may recreate \
                 data. Stop it manually and re-run.",
                d.pid, d.profile
            );
        }
    }
}

/// Is a process with this PID alive? On Unix, `kill(pid, 0)` probes existence
/// without sending a signal.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // 0 == still there (or alive-but-not-ours, EPERM); ESRCH == gone.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Confirm a PID actually belongs to a rantaiclaw daemon before signalling it,
/// guarding against PID reuse (a stale sentinel pointing at an unrelated
/// process). Returns false when it cannot be positively confirmed — we would
/// rather leave a daemon running than kill the wrong process.
fn process_is_daemon(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        match std::fs::read(format!("/proc/{pid}/cmdline")) {
            // cmdline is NUL-separated argv; a rantaiclaw daemon always has
            // both the binary name and the `daemon` subcommand.
            Ok(raw) => {
                let s = String::from_utf8_lossy(&raw);
                s.contains("rantaiclaw") && s.contains("daemon")
            }
            Err(_) => false,
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        match std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
        {
            Ok(out) if out.status.success() => {
                let s = String::from_utf8_lossy(&out.stdout);
                s.contains("rantaiclaw") && s.contains("daemon")
            }
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Stop a foreground daemon: verify identity, SIGTERM, wait for graceful exit,
/// then SIGKILL if it lingers. Returns true when the process is confirmed gone.
fn stop_daemon_process(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if !process_is_daemon(pid) {
            return false;
        }
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        // Give it up to ~2s to unwind (clear sentinel, stop services).
        for _ in 0..20 {
            if !process_is_alive(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        std::thread::sleep(std::time::Duration::from_millis(100));
        !process_is_alive(pid)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn print_plan(scope: &Scope, opts: &UninstallOpts, info: &BinaryInfo, daemons: &[RunningDaemon]) {
    println!("rantaiclaw uninstall plan:");
    for d in daemons {
        match &d.unit {
            Some(unit) => println!(
                "  - stop service unit {} (pid {}, profile {})",
                unit, d.pid, d.profile
            ),
            None => println!("  - stop daemon pid {} (profile {})", d.pid, d.profile),
        }
    }
    let has_data = !scope.dirs_to_remove.is_empty() || !scope.files_to_remove.is_empty();
    if has_data {
        for d in &scope.dirs_to_remove {
            println!("  - remove dir  {}", d.display());
        }
        for f in &scope.files_to_remove {
            println!("  - remove file {}", f.display());
        }
    } else if daemons.is_empty() {
        println!("  (nothing to remove — install state is already clean)");
    }
    if opts.keep_secrets {
        println!(
            "  (preserving {} for future re-install)",
            paths::rantaiclaw_root().join(".secret_key").display()
        );
    }
    if opts.purge {
        match info.kind {
            InstallKind::Binary => {
                println!("  - remove binary {}", info.path.display());
            }
            InstallKind::Cargo => {
                println!(
                    "  - skip binary (cargo-installed at {}; use `cargo uninstall rantaiclaw`)",
                    info.path.display()
                );
            }
            InstallKind::Workspace => {
                println!(
                    "  - skip binary (workspace build at {})",
                    info.path.display()
                );
            }
        }
    }
    if opts.dry_run {
        println!("  (dry run — nothing will be touched)");
    }
}

fn confirm(scope: &Scope, daemons: &[RunningDaemon]) -> bool {
    if scope.dirs_to_remove.is_empty() && scope.files_to_remove.is_empty() && daemons.is_empty() {
        return true;
    }
    use std::io::{self, BufRead, Write};
    print!("Proceed? [y/N] ");
    let _ = io::stdout().flush();
    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn try_uninstall_daemon() {
    // Guard against re-entering a test harness: under `cargo test`,
    // `current_exe()` is the test binary, and spawning it with
    // `service uninstall` makes libtest re-run every test matching "uninstall"
    // — each of which spawns again: an unbounded fork bomb. The real binary is
    // never built with cfg(test), so production behavior is unchanged.
    if cfg!(test) {
        return;
    }
    // Best-effort: the daemon may not be installed at all. A failure here
    // shouldn't block the rest of the uninstall.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["service", "uninstall"]);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    if let Ok(status) = cmd.status() {
        if status.success() {
            println!("  daemon service unit removed");
        }
    }
}

fn self_delete_binary(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // On Unix, we can unlink the running binary (the kernel keeps the
        // inode alive until the process exits). Just remove and report.
        fs::remove_file(path).with_context(|| format!("remove binary {}", path.display()))?;
        println!("  removed binary {}", path.display());
    }
    #[cfg(windows)]
    {
        // On Windows, the running .exe is locked. Best we can do is rename
        // it to a sibling with a .delete-on-next-launch suffix and tell the
        // user to delete it manually after they exit.
        let mut sibling = path.to_path_buf();
        let stem = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        sibling.set_file_name(format!("{stem}.delete-on-next-launch"));
        fs::rename(path, &sibling)
            .with_context(|| format!("rename binary {} for deferred delete", path.display()))?;
        println!(
            "  binary moved to {}; delete it manually after this process exits",
            sibling.display()
        );
    }
    Ok(())
}

/// The exact marker the installer writes above its PATH line
/// (see `scripts/bootstrap.sh`: `printf '\n# Added by RantaiClaw installer\n%s\n'`).
const INSTALLER_MARKER: &str = "# Added by RantaiClaw installer";

fn clean_shell_rc_amendments() -> Result<()> {
    let home = paths::home_dir();
    // The rc files the installer's `detect_shell_rc` may target.
    let candidates = [
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".profile",
        ".config/fish/config.fish",
    ];
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut touched: Vec<PathBuf> = vec![];

    for rel in &candidates {
        let path = home.join(rel);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        let mut changed = false;
        // Comment out ONLY the single PATH line the installer wrote directly
        // beneath its marker. We deliberately do not touch arbitrary lines that
        // merely mention "rantaiclaw" — those may be the user's own aliases or
        // config, and clobbering them is destructive over-reach.
        for i in 0..lines.len() {
            if lines[i].trim() != INSTALLER_MARKER {
                continue;
            }
            if let Some(next) = lines.get(i + 1) {
                let trimmed = next.trim_start();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    lines[i + 1] = format!(
                        "# rantaiclaw: removed by uninstall on {date}: {}",
                        lines[i + 1]
                    );
                    changed = true;
                }
            }
        }

        if changed {
            let mut new = lines.join("\n");
            if content.ends_with('\n') {
                new.push('\n');
            }
            fs::write(&path, &new).with_context(|| format!("write {}", path.display()))?;
            touched.push(path);
        }
    }

    if !touched.is_empty() {
        println!("  commented out installer PATH amendment in:");
        for p in touched {
            println!("    {}", p.display());
        }
    }
    Ok(())
}

fn print_summary(scope: &Scope, opts: &UninstallOpts, info: &BinaryInfo) {
    println!();
    println!("✓ uninstall complete");
    if !opts.all && !opts.purge {
        let other = list_other_profiles();
        if !other.is_empty() {
            println!(
                "  remaining profiles ({}): {}",
                other.len(),
                other.join(", ")
            );
            println!("  use `rantaiclaw uninstall --all` to remove them too");
        }
    }
    if !scope.touch_global {
        return;
    }
    if opts.keep_secrets {
        println!(
            "  preserved {} (re-install will reuse it to decrypt prior secrets)",
            paths::rantaiclaw_root().join(".secret_key").display()
        );
    }

    // A full uninstall wipes data but the binary self-recreates a fresh
    // ~/.rantaiclaw on next launch — so without this note it looks like the
    // uninstall "did nothing". State plainly that the binary remains and how
    // to remove it. (--purge removes a plain Binary; cargo/workspace installs
    // are deferred, so the binary is still present in those cases.)
    let binary_removed = opts.purge && matches!(info.kind, InstallKind::Binary);
    if !binary_removed {
        println!();
        println!(
            "  note: the rantaiclaw binary is still installed at {}",
            info.path.display()
        );
        println!("  running `rantaiclaw` again will recreate a fresh ~/.rantaiclaw");
        match info.kind {
            InstallKind::Binary => {
                println!("  to remove it:  rm -f {}", info.path.display());
            }
            InstallKind::Cargo => {
                println!("  to remove it:  cargo uninstall rantaiclaw");
            }
            InstallKind::Workspace => {
                println!(
                    "  (local workspace build at {} — recreated by `cargo build`)",
                    info.path.display()
                );
            }
        }
    }
}

fn list_other_profiles() -> Vec<String> {
    let active = ProfileManager::resolve_active_name();
    ProfileManager::list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n != &active)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests mutate the process-global HOME/RANTAICLAW_PROFILE env; serialize
    // them and restore prior values so they can't clobber each other.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<F: FnOnce()>(f: F) {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("RANTAICLAW_PROFILE");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

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

    fn make_opts() -> UninstallOpts {
        UninstallOpts {
            all: false,
            purge: false,
            keep_secrets: false,
            yes: true,
            dry_run: false,
        }
    }

    #[test]
    fn dry_run_touches_nothing() {
        with_home(|| {
            let p = paths::profile_dir("default");
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("config.toml"), "x").unwrap();

            let opts = UninstallOpts {
                dry_run: true,
                ..make_opts()
            };
            run(opts).unwrap();
            assert!(p.exists(), "dry-run should not delete");
            assert!(p.join("config.toml").exists());
        });
    }

    #[test]
    fn default_removes_active_profile_only() {
        with_home(|| {
            fs::create_dir_all(paths::profile_dir("default")).unwrap();
            fs::create_dir_all(paths::profile_dir("work")).unwrap();
            fs::write(paths::active_profile_file(), "default").unwrap();

            run(make_opts()).unwrap();
            assert!(!paths::profile_dir("default").exists());
            assert!(paths::profile_dir("work").exists(), "non-active untouched");
            assert!(
                paths::rantaiclaw_root().exists(),
                "global root preserved without --all"
            );
        });
    }

    #[test]
    fn all_wipes_root() {
        with_home(|| {
            fs::create_dir_all(paths::profile_dir("default")).unwrap();
            fs::create_dir_all(paths::profile_dir("work")).unwrap();
            fs::write(paths::rantaiclaw_root().join(".secret_key"), "secret").unwrap();

            let opts = UninstallOpts {
                all: true,
                ..make_opts()
            };
            run(opts).unwrap();
            assert!(!paths::rantaiclaw_root().exists());
        });
    }

    #[test]
    fn keep_secrets_preserves_secret_key() {
        with_home(|| {
            fs::create_dir_all(paths::profile_dir("default")).unwrap();
            let key = paths::rantaiclaw_root().join(".secret_key");
            fs::write(&key, "secret").unwrap();

            let opts = UninstallOpts {
                all: true,
                keep_secrets: true,
                ..make_opts()
            };
            run(opts).unwrap();
            assert!(key.exists(), "secret key preserved with --keep-secrets");
            assert!(
                !paths::profile_dir("default").exists(),
                "profiles still removed"
            );
        });
    }

    #[test]
    fn shell_rc_only_touches_installer_marker_block() {
        with_home(|| {
            let bashrc = paths::home_dir().join(".bashrc");
            fs::write(
                &bashrc,
                // A user's own alias that mentions rantaiclaw (must survive),
                // an unrelated PATH line (must survive), then the installer's
                // marker block exactly as bootstrap.sh writes it.
                "alias rc='rantaiclaw chat'\n\
                 export PATH=\"$HOME/.local/bin:$PATH\"\n\
                 \n\
                 # Added by RantaiClaw installer\n\
                 export PATH=\"$HOME/.cargo/bin:$PATH\"\n",
            )
            .unwrap();

            clean_shell_rc_amendments().unwrap();
            let after = fs::read_to_string(&bashrc).unwrap();

            // The installer's PATH line (right after the marker) is commented.
            assert!(
                after.contains("# rantaiclaw: removed by uninstall on"),
                "installer PATH line should be commented:\n{after}"
            );
            // The user's own rantaiclaw alias is untouched — no over-reach.
            assert!(
                after.contains("alias rc='rantaiclaw chat'"),
                "user alias must survive:\n{after}"
            );
            // The unrelated PATH line is untouched.
            assert!(after.contains("export PATH=\"$HOME/.local/bin:$PATH\""));
            // The marker breadcrumb stays.
            assert!(after.contains("# Added by RantaiClaw installer"));
        });
    }

    #[test]
    fn shell_rc_marker_cleanup_is_idempotent() {
        with_home(|| {
            let bashrc = paths::home_dir().join(".bashrc");
            fs::write(
                &bashrc,
                "# Added by RantaiClaw installer\nexport PATH=\"$HOME/.cargo/bin:$PATH\"\n",
            )
            .unwrap();

            clean_shell_rc_amendments().unwrap();
            let first = fs::read_to_string(&bashrc).unwrap();
            clean_shell_rc_amendments().unwrap();
            let second = fs::read_to_string(&bashrc).unwrap();
            assert_eq!(first, second, "second run must not re-comment");
            // Exactly one removal breadcrumb.
            assert_eq!(second.matches("removed by uninstall on").count(), 1);
        });
    }

    #[test]
    fn running_daemons_keeps_live_sentinels_and_drops_dead() {
        with_home(|| {
            // A live foreground daemon: use our own PID (definitely alive).
            sentinel::write_sentinel(
                "alive",
                &sentinel::DaemonSentinel {
                    pid: std::process::id(),
                    unit: None,
                    started_at: None,
                },
            )
            .unwrap();
            // A stale sentinel pointing at a PID that isn't running.
            sentinel::write_sentinel(
                "stale",
                &sentinel::DaemonSentinel {
                    pid: 0x7fff_fffe,
                    unit: Some("rantaiclaw@stale.service".into()),
                    started_at: None,
                },
            )
            .unwrap();

            let found = running_daemons(&[
                "alive".to_string(),
                "stale".to_string(),
                "no-sentinel".to_string(),
            ]);
            let profiles: Vec<&str> = found.iter().map(|d| d.profile.as_str()).collect();
            assert_eq!(profiles, vec!["alive"], "only the live sentinel is kept");
            assert_eq!(found[0].pid, std::process::id());
            assert!(found[0].unit.is_none(), "foreground daemon has no unit");
        });
    }
}
