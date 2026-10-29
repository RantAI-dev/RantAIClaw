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
//! `--keep-secrets` preserves every secret key so a future re-install can
//! decrypt prior encrypted credentials. The key lives next to the config it
//! protects — `<profile>/.secret_key` for the profile layout — and the report
//! names only the keys that exist and that the scope really keeps.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::lifecycle::binary_path::{BinaryInfo, InstallKind};
use crate::profile::{paths, sentinel, ProfileManager};

#[allow(
    clippy::struct_excessive_bools,
    reason = "all five booleans are independent CLI flags (`--all`, `--purge`, `--keep-secrets`, `--yes`, `--dry-run`); grouping them under a sub-struct would add indirection without clarifying any decision the field already makes"
)]
#[derive(Debug, Clone)]
pub struct UninstallOpts {
    /// Remove every profile + the global root, not just the active profile.
    pub all: bool,
    /// `--all` plus self-delete the binary.
    pub purge: bool,
    /// Preserve every secret key so re-install can decrypt.
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

    // The web console (`ui start`) is tracked in ~/.rantaiclaw/ui/.run, not by
    // the daemon sentinel, so the stop above misses it. When --all/--purge wipes
    // the whole tree, its .run goes with it — stop the console first, else it's
    // orphaned (keeps holding the port, untrackable by `ui stop`).
    if opts.all || opts.purge {
        stop_ui_console();
    }

    // 2. Remove profile data. Every path is judged by itself
    //    (`symlink_metadata` never follows a link), so a symlink is removed
    //    as itself and a dangling one is not skipped.
    for path in &scope.dirs_to_remove {
        if fs::symlink_metadata(path).is_ok() {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
            println!("  removed {}", path.display());
        }
    }
    for path in &scope.files_to_remove {
        if fs::symlink_metadata(path).is_ok() {
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            println!("  removed {}", path.display());
        }
    }

    // 3. Self-delete the binary (purge only). A binary this account did not
    //    install belongs to whoever put it there: it is left alone, and the
    //    `rm` command is printed instead of deleting someone else's file.
    if opts.purge {
        match info.kind {
            InstallKind::Cargo => {
                eprintln!(
                    "⚠ skipping --purge: cargo-installed binary at {}.\n  \
                     Run `cargo uninstall rantaiclaw` to remove.",
                    info.path.display()
                );
            }
            InstallKind::Workspace => {
                eprintln!(
                    "⚠ skipping --purge: binary is running from a cargo workspace at {}.",
                    info.path.display()
                );
            }
            InstallKind::Binary if info.owned => {
                self_delete_binary(&info.path)?;
            }
            InstallKind::Binary => {
                eprintln!(
                    "⚠ skipping --purge: this binary at {} was not installed by this account.\n  \
                     Remove it with `rm -f {}` if that is what you want.",
                    info.path.display(),
                    info.path.display()
                );
            }
        }
    }

    // 4. Best-effort PATH amendment cleanup.
    if scope.touch_global {
        let _ = clean_shell_rc_amendments();
    }

    print_summary(&scope, &opts, &info);
    Ok(())
}

/// Stop the web console started by `ui start`, tracked in `~/.rantaiclaw/ui/.run`
/// (a separate mechanism from the daemon sentinel). Called before a full-tree
/// wipe so removing that `.run` file doesn't orphan the still-running process.
fn stop_ui_console() {
    let run_file = paths::rantaiclaw_root().join("ui").join(".run");
    let Ok(state) = fs::read_to_string(&run_file) else {
        return;
    };
    for (name, pid) in parse_run_pids(&state) {
        if crate::lifecycle::process::stop_process_graceful(pid) {
            println!("  stopped web console {name} (pid {pid})");
        }
    }
}

/// Parse a console `.run` file (`name=pid` per line) into `(name, pid)` pairs,
/// skipping malformed lines.
fn parse_run_pids(state: &str) -> Vec<(&str, u32)> {
    state
        .lines()
        .filter_map(|line| {
            let (name, pid) = line.split_once('=')?;
            Some((name.trim(), pid.trim().parse::<u32>().ok()?))
        })
        .collect()
}

#[derive(Debug)]
struct Scope {
    dirs_to_remove: Vec<PathBuf>,
    files_to_remove: Vec<PathBuf>,
    touch_global: bool,
    /// Profiles whose data is being removed — used to find daemons bound to
    /// them so they can be stopped before their profile dir is deleted.
    profiles: Vec<String>,
    /// Secret key files this scope keeps: they survive the uninstall.
    kept_secret_keys: Vec<PathBuf>,
    /// Secret key files this scope removes with the profile that holds them.
    removed_secret_keys: Vec<PathBuf>,
}

impl Scope {
    fn resolve(opts: &UninstallOpts) -> Self {
        if opts.all || opts.purge {
            // Wipe the entire ~/.rantaiclaw tree (minus every secret key when
            // --keep-secrets is set).
            let root = paths::rantaiclaw_root();
            let (dirs, files, kept_secret_keys) = if root.exists() {
                if opts.keep_secrets {
                    removal_plan_keeping_secrets(&root)
                } else {
                    (vec![root], vec![], vec![])
                }
            } else {
                (vec![], vec![], vec![])
            };
            Self {
                dirs_to_remove: dirs,
                files_to_remove: files,
                touch_global: true,
                profiles: ProfileManager::list().unwrap_or_default(),
                kept_secret_keys,
                removed_secret_keys: vec![],
            }
        } else {
            // Default: only the active profile directory.
            let active = ProfileManager::resolve_active_name();
            let dir = paths::profile_dir(&active);
            // The profile holds its own secret key; it goes with the profile.
            let key = dir.join(".secret_key");
            let removed_secret_keys = if key.exists() { vec![key] } else { vec![] };
            let dirs = if dir.exists() { vec![dir] } else { vec![] };
            Self {
                dirs_to_remove: dirs,
                files_to_remove: vec![],
                touch_global: false,
                profiles: vec![active],
                kept_secret_keys: vec![],
                removed_secret_keys,
            }
        }
    }
}

/// The paths to remove when `--keep-secrets` is set, and the secret keys that
/// survive.
///
/// The key lives next to the config it protects — `<profile>/.secret_key` for
/// the profile layout, `<root>/.secret_key` for an install that predates it.
/// A directory that holds a key somewhere below stays (it ends up holding
/// only that key); every other directory is removed whole.
fn removal_plan_keeping_secrets(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = vec![];
    let mut files = vec![];
    let mut kept = vec![];
    collect_keeping_secrets(root, &mut dirs, &mut files, &mut kept);
    (dirs, files, kept)
}

/// Remove everything under `dir` except the secret keys it holds.
///
/// A symlink is an entry of the tree, never a doorway into it: it is
/// classified by its own type, so the walk can neither step out of the tree
/// nor chase a cycle back into it.
fn collect_keeping_secrets(
    dir: &Path,
    dirs: &mut Vec<PathBuf>,
    files: &mut Vec<PathBuf>,
    kept: &mut Vec<PathBuf>,
) {
    if !subtree_holds_a_secret_key(dir) {
        dirs.push(dir.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry_is_real_dir(&entry) {
            collect_keeping_secrets(&path, dirs, files, kept);
        } else if path.file_name().is_some_and(|name| name == ".secret_key") {
            kept.push(path);
        } else {
            files.push(path);
        }
    }
}

/// Whether the entry is a real directory. A symlink never is, whatever it
/// points at: [`fs::DirEntry::file_type`] reports the entry's own type (from
/// `d_type`, or `lstat` when the filesystem does not report it) and never
/// follows the link.
fn entry_is_real_dir(entry: &fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|file_type| file_type.is_dir())
}

/// Whether a secret key is anywhere below `dir`.
fn subtree_holds_a_secret_key(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if entry_is_real_dir(&entry) {
            subtree_holds_a_secret_key(&path)
        } else {
            path.file_name().is_some_and(|name| name == ".secret_key")
        }
    })
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
    for line in plan_lines(scope, opts, info, daemons) {
        println!("{line}");
    }
}

/// The lines of the uninstall plan.
///
/// A pure function, so the report can be checked: a "preserving" line names a
/// file that exists and that this scope really keeps, and a "remove binary"
/// line names a binary this account really installed.
fn plan_lines(
    scope: &Scope,
    opts: &UninstallOpts,
    info: &BinaryInfo,
    daemons: &[RunningDaemon],
) -> Vec<String> {
    let mut lines = vec!["rantaiclaw uninstall plan:".to_string()];
    for d in daemons {
        lines.push(match &d.unit {
            Some(unit) => format!(
                "  - stop service unit {} (pid {}, profile {})",
                unit, d.pid, d.profile
            ),
            None => format!("  - stop daemon pid {} (profile {})", d.pid, d.profile),
        });
    }
    let has_data = !scope.dirs_to_remove.is_empty() || !scope.files_to_remove.is_empty();
    if has_data {
        for d in &scope.dirs_to_remove {
            lines.push(format!("  - remove dir  {}", d.display()));
        }
        for f in &scope.files_to_remove {
            lines.push(format!("  - remove file {}", f.display()));
        }
    } else if daemons.is_empty() {
        lines.push("  (nothing to remove — install state is already clean)".to_string());
    }
    for key in &scope.kept_secret_keys {
        lines.push(format!(
            "  (preserving {} for future re-install)",
            key.display()
        ));
    }
    for key in &scope.removed_secret_keys {
        lines.push(format!(
            "  (the profile's secret key {} is removed with it)",
            key.display()
        ));
    }
    if opts.purge {
        lines.push(binary_plan_line(info));
    }
    if opts.dry_run {
        lines.push("  (dry run — nothing will be touched)".to_string());
    }
    lines
}

/// The plan line for the binary under `--purge`.
fn binary_plan_line(info: &BinaryInfo) -> String {
    match info.kind {
        InstallKind::Binary if info.owned => {
            format!("  - remove binary {}", info.path.display())
        }
        InstallKind::Binary => format!(
            "  - skip binary (not installed by this account at {}; remove it with `rm -f {}`)",
            info.path.display(),
            info.path.display()
        ),
        InstallKind::Cargo => format!(
            "  - skip binary (cargo-installed at {}; use `cargo uninstall rantaiclaw`)",
            info.path.display()
        ),
        InstallKind::Workspace => {
            format!(
                "  - skip binary (workspace build at {})",
                info.path.display()
            )
        }
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
    for line in summary_lines(scope, opts, info) {
        println!("{line}");
    }
}

/// The lines of the uninstall summary.
///
/// A pure function, so the report can be checked: a "preserved" line names a
/// file that exists and that this scope really kept.
fn summary_lines(scope: &Scope, opts: &UninstallOpts, info: &BinaryInfo) -> Vec<String> {
    let mut lines = vec![String::new(), "✓ uninstall complete".to_string()];
    if !opts.all && !opts.purge {
        let other = list_other_profiles();
        if !other.is_empty() {
            lines.push(format!(
                "  remaining profiles ({}): {}",
                other.len(),
                other.join(", ")
            ));
            lines.push("  use `rantaiclaw uninstall --all` to remove them too".to_string());
        }
    }
    if !scope.touch_global {
        return lines;
    }
    for key in &scope.kept_secret_keys {
        lines.push(format!(
            "  preserved {} (re-install will reuse it to decrypt prior secrets)",
            key.display()
        ));
    }

    // A full uninstall wipes data but the binary self-recreates a fresh
    // ~/.rantaiclaw on next launch — so without this note it looks like the
    // uninstall "did nothing". State plainly that the binary remains and how
    // to remove it. (--purge removes a plain Binary this account installed;
    // cargo/workspace installs are deferred, and a binary this account did
    // not install is left to its owner, so the binary is still present in
    // those cases.)
    let binary_removed = opts.purge && matches!(info.kind, InstallKind::Binary) && info.owned;
    if !binary_removed {
        lines.push(String::new());
        lines.push(format!(
            "  note: the rantaiclaw binary is still installed at {}",
            info.path.display()
        ));
        lines.push("  running `rantaiclaw` again will recreate a fresh ~/.rantaiclaw".to_string());
        lines.push(match info.kind {
            InstallKind::Binary => format!("  to remove it:  rm -f {}", info.path.display()),
            InstallKind::Cargo => "  to remove it:  cargo uninstall rantaiclaw".to_string(),
            InstallKind::Workspace => format!(
                "  (local workspace build at {} — recreated by `cargo build`)",
                info.path.display()
            ),
        });
    }
    lines
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

    // Tests mutate the process-global HOME/RANTAICLAW_PROFILE env; serialize
    // them against the crate-shared ENV_LOCK so they can't clobber other
    // tests (e.g. in channels/config) that mutate the same process-global env.
    fn with_home<F: FnOnce()>(f: F) {
        let _g = crate::test_env::ENV_LOCK.blocking_lock();
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
    fn parse_run_pids_extracts_named_pids_and_skips_malformed() {
        let pids = parse_run_pids("gateway=1234\nui=5678\ngarbage\nempty=\nui = 42\n");
        assert_eq!(pids, vec![("gateway", 1234), ("ui", 5678), ("ui", 42)]);
    }

    #[test]
    fn stop_ui_console_is_a_noop_when_no_run_file_exists() {
        // No ~/.rantaiclaw/ui/.run under the temp HOME → must return quietly,
        // never panic. (The actual PID-stopping path composes the unit-tested
        // parse_run_pids with lifecycle::process::stop_process_graceful.)
        with_home(|| {
            stop_ui_console();
        });
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

    fn binary_info(path: &str, kind: InstallKind, owned: bool) -> BinaryInfo {
        BinaryInfo {
            path: PathBuf::from(path),
            kind,
            owned,
        }
    }

    fn empty_scope() -> Scope {
        Scope {
            dirs_to_remove: vec![],
            files_to_remove: vec![],
            touch_global: false,
            profiles: vec![],
            kept_secret_keys: vec![],
            removed_secret_keys: vec![],
        }
    }

    #[test]
    fn keep_secrets_with_no_secret_key_prints_no_preserved_line() {
        with_home(|| {
            fs::create_dir_all(paths::profile_dir("default")).unwrap();

            let opts = UninstallOpts {
                all: true,
                keep_secrets: true,
                dry_run: true,
                ..make_opts()
            };
            let scope = Scope::resolve(&opts);
            assert!(scope.kept_secret_keys.is_empty());

            let info = binary_info("/home/u/.local/bin/rantaiclaw", InstallKind::Binary, true);
            let lines = plan_lines(&scope, &opts, &info, &[]);
            assert!(
                !lines.iter().any(|line| line.contains("preserving")),
                "{lines:?}"
            );
            let summary = summary_lines(&scope, &opts, &info);
            assert!(
                !summary.iter().any(|line| line.contains("preserved")),
                "{summary:?}"
            );
        });
    }

    #[test]
    fn keep_secrets_keeps_the_profile_secret_key_and_names_it() {
        with_home(|| {
            let profile = paths::profile_dir("default");
            fs::create_dir_all(&profile).unwrap();
            let key = profile.join(".secret_key");
            fs::write(&key, "secret").unwrap();

            let opts = UninstallOpts {
                all: true,
                keep_secrets: true,
                dry_run: true,
                ..make_opts()
            };
            let scope = Scope::resolve(&opts);
            assert_eq!(scope.kept_secret_keys, vec![key.clone()]);

            let info = binary_info("/home/u/.local/bin/rantaiclaw", InstallKind::Binary, true);
            let lines = plan_lines(&scope, &opts, &info, &[]);
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains("preserving")
                        && line.contains(&key.display().to_string())),
                "{lines:?}"
            );
            let summary = summary_lines(&scope, &opts, &info);
            assert!(
                summary
                    .iter()
                    .any(|line| line.contains("preserved")
                        && line.contains(&key.display().to_string())),
                "{summary:?}"
            );
        });
    }

    #[test]
    fn the_default_scope_says_the_profile_secret_key_goes_with_the_profile() {
        with_home(|| {
            let profile = paths::profile_dir("default");
            fs::create_dir_all(&profile).unwrap();
            let key = profile.join(".secret_key");
            fs::write(&key, "secret").unwrap();

            let opts = UninstallOpts {
                dry_run: true,
                ..make_opts()
            };
            let scope = Scope::resolve(&opts);
            assert!(scope.kept_secret_keys.is_empty());
            assert_eq!(scope.removed_secret_keys, vec![key.clone()]);

            let info = binary_info("/home/u/.local/bin/rantaiclaw", InstallKind::Binary, true);
            let lines = plan_lines(&scope, &opts, &info, &[]);
            assert!(
                lines.iter().any(|line| line.contains("removed with it")
                    && line.contains(&key.display().to_string())),
                "{lines:?}"
            );
            assert!(
                !lines.iter().any(|line| line.contains("preserving")),
                "{lines:?}"
            );
        });
    }

    #[test]
    fn a_binary_this_account_did_not_install_is_not_planned_for_removal() {
        let opts = UninstallOpts {
            purge: true,
            dry_run: true,
            ..make_opts()
        };
        let info = binary_info("/x/.cargo/bin/rantaiclaw", InstallKind::Binary, false);
        let lines = plan_lines(&empty_scope(), &opts, &info, &[]);
        assert!(
            !lines.iter().any(|line| line.contains("remove binary")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("rm -f")), "{lines:?}");
    }

    #[test]
    fn a_binary_this_account_installed_is_planned_for_removal() {
        let opts = UninstallOpts {
            purge: true,
            dry_run: true,
            ..make_opts()
        };
        let info = binary_info("/home/u/.local/bin/rantaiclaw", InstallKind::Binary, true);
        let lines = plan_lines(&empty_scope(), &opts, &info, &[]);
        assert!(
            lines.iter().any(|line| line.contains("remove binary")),
            "{lines:?}"
        );
    }

    #[test]
    fn keep_secrets_preserves_the_profile_secret_key() {
        with_home(|| {
            let profile = paths::profile_dir("default");
            fs::create_dir_all(&profile).unwrap();
            let key = profile.join(".secret_key");
            fs::write(&key, "secret").unwrap();
            fs::write(profile.join("config.toml"), "x").unwrap();

            let opts = UninstallOpts {
                all: true,
                keep_secrets: true,
                ..make_opts()
            };
            run(opts).unwrap();
            assert!(key.exists(), "the profile's secret key is preserved");
            assert!(
                !profile.join("config.toml").exists(),
                "the rest of the profile is removed"
            );
        });
    }

    /// A tree whose profile holds a secret key, plus a symlink inside the
    /// tree pointing at `outside` — a directory outside the tree that holds a
    /// file and a secret key below it.
    ///
    /// Returns `(root, outside, link)`.
    #[cfg(unix)]
    fn tree_with_symlink_to_outside() -> (PathBuf, PathBuf, PathBuf) {
        let root = paths::rantaiclaw_root();
        let outside = paths::home_dir().join("outside");
        fs::create_dir_all(outside.join("keys")).unwrap();
        fs::write(outside.join("document.txt"), "outside").unwrap();
        fs::write(outside.join("keys").join(".secret_key"), "secret").unwrap();

        let profile = paths::profile_dir("default");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join(".secret_key"), "secret").unwrap();
        fs::write(profile.join("config.toml"), "x").unwrap();

        let link = root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        (root, outside, link)
    }

    /// The plan of `--all --keep-secrets` for a tree that holds a symlink.
    #[cfg(unix)]
    fn keep_secrets_plan() -> Scope {
        let opts = UninstallOpts {
            all: true,
            keep_secrets: true,
            dry_run: true,
            ..make_opts()
        };
        Scope::resolve(&opts)
    }

    #[cfg(unix)]
    #[test]
    fn keep_secrets_does_not_descend_into_a_symlinked_directory_outside_the_tree() {
        with_home(|| {
            let (_, outside, link) = tree_with_symlink_to_outside();
            let outside_file = outside.join("document.txt");

            let scope = keep_secrets_plan();

            // The symlink is an entry of the tree: it is planned as a file,
            // and nothing below it is planned at all.
            assert!(
                scope.files_to_remove.contains(&link),
                "the symlink is planned as a file, got {:?}",
                scope.files_to_remove
            );
            assert!(
                !scope.dirs_to_remove.contains(&link),
                "a symlink is never a directory, got {:?}",
                scope.dirs_to_remove
            );
            for planned in scope
                .files_to_remove
                .iter()
                .chain(scope.dirs_to_remove.iter())
            {
                assert!(
                    !(planned.starts_with(&link) && planned != &link),
                    "nothing below the symlink may be planned for removal: {}",
                    planned.display()
                );
            }

            // The file outside the tree is not part of the plan...
            assert!(
                !scope.files_to_remove.contains(&outside_file)
                    && !scope.dirs_to_remove.contains(&outside_file),
                "a file outside the tree must never be planned"
            );
            // ...while the tree's own files are.
            let profile = paths::profile_dir("default");
            assert!(
                scope.files_to_remove.contains(&profile.join("config.toml")),
                "the tree's own file is planned, got {:?}",
                scope.files_to_remove
            );
            assert!(
                scope
                    .kept_secret_keys
                    .contains(&profile.join(".secret_key")),
                "the tree's own key is kept, got {:?}",
                scope.kept_secret_keys
            );

            // The outside file is untouched.
            assert!(
                outside_file.exists(),
                "the file outside the tree must survive the plan"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn keep_secrets_removes_a_symlink_in_the_tree_as_itself() {
        with_home(|| {
            let (_, outside, link) = tree_with_symlink_to_outside();
            let outside_file = outside.join("document.txt");

            let opts = UninstallOpts {
                all: true,
                keep_secrets: true,
                ..make_opts()
            };
            run(opts).unwrap();

            // The link itself is gone...
            assert!(
                fs::symlink_metadata(&link).is_err(),
                "the symlink must be removed as itself"
            );
            // ...but nothing it pointed at was.
            assert!(
                outside_file.exists(),
                "the file outside the tree must survive the uninstall"
            );
            assert!(
                outside.join("keys").join(".secret_key").exists(),
                "the key outside the tree must survive the uninstall"
            );

            // The tree's own files are gone; its key is kept.
            let profile = paths::profile_dir("default");
            assert!(!profile.join("config.toml").exists());
            assert!(profile.join(".secret_key").exists());
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_in_the_tree_is_removed_as_itself() {
        with_home(|| {
            let root = paths::rantaiclaw_root();
            let profile = paths::profile_dir("default");
            fs::create_dir_all(&profile).unwrap();
            fs::write(profile.join(".secret_key"), "secret").unwrap();
            fs::write(profile.join("config.toml"), "x").unwrap();
            let dangling = root.join("dangling");
            std::os::unix::fs::symlink(root.join("gone"), &dangling).unwrap();

            let scope = keep_secrets_plan();
            assert!(
                scope.files_to_remove.contains(&dangling),
                "a dangling symlink is still an entry of the tree, got {:?}",
                scope.files_to_remove
            );

            let opts = UninstallOpts {
                all: true,
                keep_secrets: true,
                ..make_opts()
            };
            run(opts).unwrap();
            assert!(
                fs::symlink_metadata(&dangling).is_err(),
                "a dangling symlink must be removed, not skipped"
            );
        });
    }

    /// Marks the child half of `a_symlink_cycle_does_not_hang`.
    const CYCLE_CHILD_ENV: &str = "RANTAICLAW_UNINSTALL_CYCLE_CHILD";
    const CYCLE_MARKER_ENV: &str = "RANTAICLAW_UNINSTALL_CYCLE_MARKER";

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_does_not_hang() {
        // Child half: HOME is the tree the parent built. Compute the plan on
        // it and leave a marker behind, so the parent can tell that the plan
        // was really computed (and not that the filter matched no test).
        if std::env::var_os(CYCLE_CHILD_ENV).is_some() {
            let scope = keep_secrets_plan();
            let cycle = paths::rantaiclaw_root().join("self");
            assert!(
                scope.files_to_remove.contains(&cycle),
                "the cycle link is planned as a file, not walked into: {:?}",
                scope.files_to_remove
            );
            let marker = std::env::var_os(CYCLE_MARKER_ENV).expect("marker path");
            fs::write(marker, "planned").unwrap();
            return;
        }

        with_home(|| {
            let root = paths::rantaiclaw_root();
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join(".secret_key"), "secret").unwrap();
            // A link that points back at the tree it lives in.
            let cycle = root.join("self");
            std::os::unix::fs::symlink(&root, &cycle).unwrap();

            // The plan must be computed, not chased forever. The child does
            // the work; a walk that follows the link never returns, so the
            // parent gives up on it rather than hanging the suite.
            let marker = paths::home_dir().join("cycle-plan-computed");
            let exe = std::env::current_exe().expect("the test binary");
            let mut cmd = std::process::Command::new(exe);
            cmd.args([
                "--exact",
                "lifecycle::uninstall::tests::a_symlink_cycle_does_not_hang",
            ])
            .env(CYCLE_CHILD_ENV, "1")
            .env(CYCLE_MARKER_ENV, &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("spawn the child");

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            panic!(
                                "the uninstall plan never finished: the walk followed the symlink cycle"
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(err) => panic!("waiting for the child failed: {err}"),
                }
            };
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stderr);
            }
            assert!(status.success(), "the child failed: {stderr}");
            assert!(
                marker.exists(),
                "the child never computed the plan: {stderr}"
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
                    profile_pinned: false,
                },
            )
            .unwrap();
            // A stale sentinel pointing at a PID that isn't running.
            sentinel::write_sentinel(
                "stale",
                &sentinel::DaemonSentinel {
                    pid: 0x7fff_fffe,
                    unit: Some("rantaiclaw.service".into()),
                    started_at: None,
                    profile_pinned: false,
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
