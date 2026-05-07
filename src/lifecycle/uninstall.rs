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
use crate::profile::{paths, ProfileManager};

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
    print_plan(&scope, &opts, &info);

    if !opts.yes && !opts.dry_run && !confirm(&scope) {
        println!("aborted");
        return Ok(());
    }

    if opts.dry_run {
        return Ok(());
    }

    // 1. Daemon teardown — best effort. `service uninstall` is the cleanest
    //    surface but it requires a Config; on a partial install it may not
    //    load. Fall back to printing a hint if it fails.
    if scope.touch_global {
        try_uninstall_daemon();
    }

    // 2. Remove profile data.
    for path in &scope.dirs_to_remove {
        if path.exists() {
            fs::remove_dir_all(path)
                .with_context(|| format!("remove {}", path.display()))?;
            println!("  removed {}", path.display());
        }
    }
    for path in &scope.files_to_remove {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("remove {}", path.display()))?;
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

    print_summary(&scope, &opts);
    Ok(())
}

#[derive(Debug)]
struct Scope {
    dirs_to_remove: Vec<PathBuf>,
    files_to_remove: Vec<PathBuf>,
    touch_global: bool,
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
            }
        }
    }
}

fn print_plan(scope: &Scope, opts: &UninstallOpts, info: &BinaryInfo) {
    println!("rantaiclaw uninstall plan:");
    if scope.dirs_to_remove.is_empty() && scope.files_to_remove.is_empty() {
        println!("  (nothing to remove — install state is already clean)");
    } else {
        for d in &scope.dirs_to_remove {
            println!("  - remove dir  {}", d.display());
        }
        for f in &scope.files_to_remove {
            println!("  - remove file {}", f.display());
        }
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

fn confirm(scope: &Scope) -> bool {
    if scope.dirs_to_remove.is_empty() && scope.files_to_remove.is_empty() {
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
        let stem = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        sibling.set_file_name(format!("{stem}.delete-on-next-launch"));
        fs::rename(path, &sibling).with_context(|| {
            format!("rename binary {} for deferred delete", path.display())
        })?;
        println!(
            "  binary moved to {}; delete it manually after this process exits",
            sibling.display()
        );
    }
    Ok(())
}

fn clean_shell_rc_amendments() -> Result<()> {
    let home = paths::home_dir();
    let candidates = [".bashrc", ".zshrc", ".profile", ".config/fish/config.fish"];
    let marker = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut touched: Vec<PathBuf> = vec![];

    for rel in &candidates {
        let path = home.join(rel);
        if !path.exists() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut changed = false;
        let new = content
            .lines()
            .map(|line| {
                if line.contains("rantaiclaw") && !line.trim_start().starts_with('#') {
                    changed = true;
                    format!("# rantaiclaw: removed by uninstall on {marker}: {line}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if changed {
            // Preserve trailing newline.
            let mut new = new;
            if content.ends_with('\n') && !new.ends_with('\n') {
                new.push('\n');
            }
            fs::write(&path, new).with_context(|| format!("write {}", path.display()))?;
            touched.push(path);
        }
    }

    if !touched.is_empty() {
        println!("  commented out PATH amendments in:");
        for p in touched {
            println!("    {}", p.display());
        }
    }
    Ok(())
}

fn print_summary(scope: &Scope, opts: &UninstallOpts) {
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
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        // Create some fake profile state.
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
    }

    #[test]
    fn default_removes_active_profile_only() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("RANTAICLAW_PROFILE");
        // Two profiles.
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
    }

    #[test]
    fn all_wipes_root() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        fs::create_dir_all(paths::profile_dir("default")).unwrap();
        fs::create_dir_all(paths::profile_dir("work")).unwrap();
        fs::write(paths::rantaiclaw_root().join(".secret_key"), "secret").unwrap();

        let opts = UninstallOpts {
            all: true,
            ..make_opts()
        };
        run(opts).unwrap();
        assert!(!paths::rantaiclaw_root().exists());
    }

    #[test]
    fn keep_secrets_preserves_secret_key() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
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
    }

    #[test]
    fn shell_rc_amendments_are_commented_out() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let bashrc = dir.path().join(".bashrc");
        fs::write(
            &bashrc,
            "export PATH=\"$HOME/.local/bin:$PATH\"\n# added by rantaiclaw bootstrap\nexport RANTAICLAW_HOME=\"$HOME/.rantaiclaw\"\n",
        )
        .unwrap();

        clean_shell_rc_amendments().unwrap();
        let after = fs::read_to_string(&bashrc).unwrap();
        // The line containing "rantaiclaw" got commented out.
        assert!(
            after.contains("# rantaiclaw: removed by uninstall"),
            "expected marker in:\n{after}"
        );
        // The unrelated PATH line is left alone.
        assert!(after.contains("export PATH=\"$HOME/.local/bin:$PATH\""));
    }
}
