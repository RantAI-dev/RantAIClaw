//! Resolve where the running `rantaiclaw` binary lives on disk and whether
//! it is safe to self-modify.
//!
//! Used by both `uninstall --purge` and `update`. The single source of truth
//! is `std::env::current_exe()`; we additionally classify the path so callers
//! can handle each install kind appropriately (`update` swaps binary and cargo
//! installs in place and refuses only workspace runs; `uninstall` defers cargo
//! removals to `cargo uninstall`).

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
}

impl BinaryInfo {
    pub fn detect() -> Result<Self> {
        let path = std::env::current_exe().context("resolve current_exe")?;
        let path = path.canonicalize().unwrap_or(path);
        let kind = classify(&path);
        Ok(Self { path, kind })
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
/// Cargo tracks installs in `$CARGO_HOME/.crates2.json` (modern) and
/// `$CARGO_HOME/.crates.toml` (legacy v1). If neither lists the binary,
/// `cargo uninstall` cannot remove it, so the file must be treated as a plain
/// binary the caller can delete directly.
fn cargo_tracks_binary(path: &Path) -> bool {
    let Some(bin) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let home = cargo_home();
    crates2_lists_bin(&home.join(".crates2.json"), bin)
        || crates_v1_lists_bin(&home.join(".crates.toml"), bin)
}

/// `$CARGO_HOME`, falling back to `~/.cargo`.
fn cargo_home() -> PathBuf {
    if let Some(h) = std::env::var_os("CARGO_HOME") {
        return PathBuf::from(h);
    }
    crate::profile::paths::home_dir().join(".cargo")
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
    fn require_self_modifiable_allows_cargo() {
        // `update` swaps the binary in place; a cargo-installed binary under
        // ~/.cargo/bin is user-owned and safe to replace (the only cost is
        // stale cargo registry metadata). rantaiclaw isn't on crates.io, so
        // there is no `cargo install rantaiclaw --force` path to defer to.
        let path = PathBuf::from("/home/u/.cargo/bin/rantaiclaw");
        let info = BinaryInfo {
            path: path.clone(),
            kind: InstallKind::Cargo,
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
        };
        assert!(require_self_modifiable(&info, "update").is_err());
    }

    #[test]
    fn require_self_modifiable_allows_binary() {
        let info = BinaryInfo {
            path: PathBuf::from("/usr/local/bin/rantaiclaw"),
            kind: InstallKind::Binary,
        };
        assert!(require_self_modifiable(&info, "update").is_ok());
    }
}
