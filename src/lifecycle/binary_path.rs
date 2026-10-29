//! Resolve where the running `rantaiclaw` binary lives on disk and whether
//! it is safe to self-modify.
//!
//! Used by both `uninstall --purge` and `update`. The single source of truth
//! is `std::env::current_exe()`; we additionally classify the path so callers
//! can handle each install kind appropriately (`update` swaps binary and cargo
//! installs in place and refuses only workspace runs; `uninstall` defers cargo
//! removals to `cargo uninstall`).
//!
//! The cargo registry is looked up where the binary itself lives, never under
//! `$HOME`: the question this file answers — "did the installer of this
//! account put this binary here?" — must not change when the process runs
//! with a different `$HOME`.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// How the running binary was installed. Determines whether self-replacement
/// or self-deletion is appropriate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// Pre-built binary in `~/.local/bin`, `/usr/local/bin`, custom prefix
    /// — anything writable that isn't cargo-managed.
    Binary,
    /// Cargo-installed under `~/.cargo/bin/` — caller should defer to cargo.
    Cargo,
    /// Built locally via `cargo build` and run from the workspace.
    Workspace,
}

/// Resolved info about the running binary.
#[derive(Debug, Clone)]
pub struct BinaryInfo {
    pub path: PathBuf,
    pub kind: InstallKind,
    /// Whether the installer of this account placed the binary where it is.
    ///
    /// `uninstall --purge` removes only a binary it can prove it owns. A
    /// binary this account did not install belongs to whoever put it there:
    /// it is left alone and the uninstall prints the `rm` command instead.
    pub owned: bool,
}

impl BinaryInfo {
    pub fn detect() -> Result<Self> {
        let path = std::env::current_exe().context("resolve current_exe")?;
        let path = path.canonicalize().unwrap_or(path);
        let kind = classify(&path);
        let owned = kind != InstallKind::Workspace
            && owned_by_this_account(
                &path,
                &crate::profile::paths::home_dir(),
                cargo_tracks_binary,
            );
        Ok(Self { path, kind, owned })
    }
}

fn classify(path: &Path) -> InstallKind {
    classify_with(path, cargo_tracks_binary)
}

/// Path-based classification with an injectable cargo-ownership check so the
/// logic is deterministically unit-testable without touching the real cargo
/// registry.
fn classify_with(path: &Path, cargo_tracks: impl Fn(&Path) -> bool) -> InstallKind {
    let s = path.to_string_lossy();
    // Workspace builds win first: a binary under target/{debug,release} is a
    // build artifact regardless of any other path segment.
    if s.contains("/target/debug/")
        || s.contains("/target/release/")
        || s.contains("\\target\\debug\\")
        || s.contains("\\target\\release\\")
    {
        return InstallKind::Workspace;
    }
    if s.contains("/.cargo/bin/") || s.contains("\\.cargo\\bin\\") {
        // Living in ~/.cargo/bin is NOT sufficient to call this a cargo
        // install. Installer scripts (see scripts/bootstrap.sh) also *copy*
        // prebuilt binaries there because it's a common PATH dir — and for
        // those, `cargo uninstall` fails with "did not match any packages".
        // Only defer to cargo when cargo's own registry records the binary.
        return if cargo_tracks(path) {
            InstallKind::Cargo
        } else {
            InstallKind::Binary
        };
    }
    InstallKind::Binary
}

/// True when cargo's install registry records a binary with this file name.
///
/// Cargo tracks installs in `<registry>/.crates2.json` (modern) and
/// `<registry>/.crates.toml` (legacy v1). If neither lists the binary,
/// `cargo uninstall` cannot remove it, so the file must be treated as a plain
/// binary the caller can delete directly — when this account owns it.
fn cargo_tracks_binary(path: &Path) -> bool {
    let Some(bin) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(registry) = registry_for(path) else {
        return false;
    };
    crates2_lists_bin(&registry.join(".crates2.json"), bin)
        || crates_v1_lists_bin(&registry.join(".crates.toml"), bin)
}

/// The cargo registry that could have installed `path`.
///
/// `$CARGO_HOME/bin/<bin>` gives `$CARGO_HOME`; `<dir>/.cargo/bin/<bin>`
/// gives `<dir>/.cargo`. Nothing else is a cargo location. The registry is
/// derived from where the binary itself lives and never from `$HOME`, so a
/// sandbox or another account's `$HOME` cannot change the answer.
fn registry_for(path: &Path) -> Option<PathBuf> {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        let cargo_home = PathBuf::from(cargo_home);
        if path.starts_with(cargo_home.join("bin")) {
            return Some(cargo_home);
        }
    }
    let bin_dir = path.parent()?;
    let registry = bin_dir.parent()?;
    if registry.file_name().is_some_and(|name| name == ".cargo") {
        return Some(registry.to_path_buf());
    }
    None
}

/// Whether the installer of this account placed the binary where it is.
///
/// Cargo's registry records the install, and the registry is looked up where
/// the binary itself lives, so a different `$HOME` cannot hide it. Any
/// other binary is owned only when it lives under this account's home
/// directory; a binary outside it belongs to whoever put it there.
fn owned_by_this_account(path: &Path, home: &Path, cargo_tracks: impl Fn(&Path) -> bool) -> bool {
    cargo_tracks(path) || path.starts_with(home)
}

/// Does `.crates2.json` record an install whose `bins` array contains `bin`?
fn crates2_lists_bin(path: &Path, bin: &str) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    json.get("installs")
        .and_then(|v| v.as_object())
        .is_some_and(|installs| {
            installs.values().any(|entry| {
                entry
                    .get("bins")
                    .and_then(|b| b.as_array())
                    .is_some_and(|bins| bins.iter().any(|x| x.as_str() == Some(bin)))
            })
        })
}

/// Does legacy `.crates.toml` (`[v1]` table of pkgid -> [bins]) list `bin`?
fn crates_v1_lists_bin(path: &Path, bin: &str) -> bool {
    // Typed deserialize (the same path `profile::sentinel` uses) rather than
    // `toml::Value` traversal — the latter's shape shifted across toml crate
    // versions; this stays correct regardless.
    #[derive(serde::Deserialize)]
    struct CratesToml {
        #[serde(default)]
        v1: std::collections::HashMap<String, Vec<String>>,
    }
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(parsed) = toml::from_str::<CratesToml>(&body) else {
        return false;
    };
    parsed.v1.values().any(|bins| bins.iter().any(|b| b == bin))
}

/// Refuse self-modifying ops on cargo/workspace installs. Returns the binary
/// path on success.
pub fn require_self_modifiable<'a>(info: &'a BinaryInfo, op_label: &str) -> Result<&'a Path> {
    match info.kind {
        // Both are user-owned files we can swap in place. A cargo install
        // (~/.cargo/bin) is allowed because rantaiclaw isn't published to
        // crates.io — there is no `cargo install rantaiclaw --force` to defer
        // to — so swapping the binary is the only working update path. The
        // caller warns that cargo's registry metadata will go stale.
        InstallKind::Binary | InstallKind::Cargo => Ok(&info.path),
        InstallKind::Workspace => Err(anyhow!(
            "{op_label} refused: this binary is running from a cargo workspace \
             build (target/debug or target/release). Run a release binary."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_cargo_bin_is_cargo_only_when_tracked() {
        // A binary under ~/.cargo/bin that cargo actually installed.
        let p = PathBuf::from("/home/u/.cargo/bin/rantaiclaw");
        assert_eq!(classify_with(&p, |_| true), InstallKind::Cargo);
    }

    #[test]
    fn classify_cargo_bin_copied_by_installer_is_plain_binary() {
        // Same path, but cargo has no registry entry — a bootstrap-copied
        // binary. `cargo uninstall` would fail, so it must be removable as a
        // plain Binary. Regression guard for the uninstall dead-end.
        let p = PathBuf::from("/home/u/.cargo/bin/rantaiclaw");
        assert_eq!(classify_with(&p, |_| false), InstallKind::Binary);
    }

    #[test]
    fn classify_cargo_workspace_wins_over_cargo_bin() {
        // Defense-in-depth: a target/ path is a Workspace build even if the
        // ownership probe would say "tracked".
        let p = PathBuf::from("/repo/target/release/rantaiclaw");
        assert_eq!(classify_with(&p, |_| true), InstallKind::Workspace);
    }

    #[test]
    fn crates2_detects_and_rejects_bins() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(".crates2.json");
        std::fs::write(
            &f,
            r#"{"installs":{"rantaiclaw 0.6.0 (path+file:///x)":{"bins":["rantaiclaw"]}}}"#,
        )
        .unwrap();
        assert!(crates2_lists_bin(&f, "rantaiclaw"));
        assert!(!crates2_lists_bin(&f, "somethingelse"));
        // Missing file -> not tracked, never panics.
        assert!(!crates2_lists_bin(
            &dir.path().join("nope.json"),
            "rantaiclaw"
        ));
    }

    #[test]
    fn crates_v1_detects_and_rejects_bins() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(".crates.toml");
        std::fs::write(
            &f,
            "[v1]\n\"rantaiclaw 0.6.0 (path+file:///x)\" = [\"rantaiclaw\"]\n",
        )
        .unwrap();
        assert!(crates_v1_lists_bin(&f, "rantaiclaw"));
        assert!(!crates_v1_lists_bin(&f, "somethingelse"));
        assert!(!crates_v1_lists_bin(
            &dir.path().join("nope.toml"),
            "rantaiclaw"
        ));
    }

    #[test]
    fn classify_workspace_release() {
        let p = PathBuf::from("/repo/target/release/rantaiclaw");
        assert_eq!(classify(&p), InstallKind::Workspace);
    }

    #[test]
    fn classify_workspace_debug() {
        let p = PathBuf::from("/repo/target/debug/rantaiclaw");
        assert_eq!(classify(&p), InstallKind::Workspace);
    }

    #[test]
    fn classify_binary_local_bin() {
        let p = PathBuf::from("/home/u/.local/bin/rantaiclaw");
        assert_eq!(classify(&p), InstallKind::Binary);
    }

    #[test]
    fn classify_binary_usr_local() {
        let p = PathBuf::from("/usr/local/bin/rantaiclaw");
        assert_eq!(classify(&p), InstallKind::Binary);
    }

    #[test]
    fn the_registry_is_looked_up_where_the_binary_lives() {
        // A registry under the binary's own `.cargo` records it — whatever
        // `$HOME` the process runs with.
        let dir = tempfile::tempdir().unwrap();
        let cargo_home = dir.path().join(".cargo");
        let bin = cargo_home.join("bin/rantaiclaw");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "x").unwrap();
        // No registry yet: cargo does not record this install.
        assert!(!cargo_tracks_binary(&bin));
        std::fs::write(
            cargo_home.join(".crates2.json"),
            r#"{"installs":{"rantaiclaw 0.6.0 (path+file:///x)":{"bins":["rantaiclaw"]}}}"#,
        )
        .unwrap();
        assert!(cargo_tracks_binary(&bin));
    }

    #[test]
    fn a_plain_install_directory_has_no_cargo_registry() {
        assert_eq!(
            registry_for(Path::new("/home/u/.local/bin/rantaiclaw")),
            None
        );
    }

    #[test]
    fn a_binary_outside_this_home_is_not_owned() {
        // The operator's `~/.cargo/bin/rantaiclaw`, seen from a sandbox whose
        // `$HOME` is elsewhere. Cargo's registry does not record it, so this
        // account did not install it and must not delete it.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".cargo/bin/rantaiclaw");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "x").unwrap();
        assert_eq!(classify_with(&bin, |_| false), InstallKind::Binary);
        assert!(!owned_by_this_account(&bin, Path::new("/sandbox"), |_| {
            false
        }));
    }

    #[test]
    fn a_cargo_install_is_owned_even_outside_this_home() {
        // A readable registry that lists the binary proves the install, so
        // the binary is this account's — and it is a cargo install, which the
        // uninstall defers to `cargo uninstall`.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".cargo/bin/rantaiclaw");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "x").unwrap();
        assert_eq!(classify_with(&bin, |_| true), InstallKind::Cargo);
        assert!(owned_by_this_account(&bin, Path::new("/sandbox"), |_| true));
    }

    #[test]
    fn a_binary_inside_this_home_is_owned() {
        let bin = PathBuf::from("/home/u/.local/bin/rantaiclaw");
        assert!(owned_by_this_account(&bin, Path::new("/home/u"), |_| false));
    }

    #[test]
    fn require_self_modifiable_allows_cargo() {
        // `update` swaps the binary in place; a cargo-installed binary under
        // ~/.cargo/bin is user-owned and safe to replace (the only cost is
        // stale cargo registry metadata). rantaiclaw isn't on crates.io, so
        // there is no `cargo install rantaiclaw --force` path to defer to.
        let path = PathBuf::from("/home/u/.cargo/bin/rantaiclaw");
        let info = BinaryInfo {
            path: path.clone(),
            kind: InstallKind::Cargo,
            owned: true,
        };
        let resolved = require_self_modifiable(&info, "update").expect("cargo must be allowed");
        assert_eq!(resolved, path);
    }

    #[test]
    fn require_self_modifiable_blocks_workspace() {
        // Running from target/{debug,release} — swapping a build artifact is
        // pointless; the next `cargo build` overwrites it.
        let info = BinaryInfo {
            path: PathBuf::from("/repo/target/release/rantaiclaw"),
            kind: InstallKind::Workspace,
            owned: false,
        };
        assert!(require_self_modifiable(&info, "update").is_err());
    }

    #[test]
    fn require_self_modifiable_allows_binary() {
        let info = BinaryInfo {
            path: PathBuf::from("/usr/local/bin/rantaiclaw"),
            kind: InstallKind::Binary,
            owned: false,
        };
        assert!(require_self_modifiable(&info, "update").is_ok());
    }
}
