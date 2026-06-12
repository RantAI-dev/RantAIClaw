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
    let s = path.to_string_lossy();
    if s.contains("/.cargo/bin/") || s.contains("\\.cargo\\bin\\") {
        return InstallKind::Cargo;
    }
    if s.contains("/target/debug/")
        || s.contains("/target/release/")
        || s.contains("\\target\\debug\\")
        || s.contains("\\target\\release\\")
    {
        return InstallKind::Workspace;
    }
    InstallKind::Binary
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
    fn classify_cargo() {
        let p = PathBuf::from("/home/u/.cargo/bin/rantaiclaw");
        assert_eq!(classify(&p), InstallKind::Cargo);
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
