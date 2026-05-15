//! Lifecycle commands — `rantaiclaw uninstall` + `rantaiclaw update`.
//!
//! These are the missing pieces between "binary is installed" and "binary is
//! gone" that every modern CLI runtime ships (e.g. `rustup self uninstall`,
//! `gh extension upgrade`). v0.6.0 — Product Completeness Beta gap close.
//!
//! - `uninstall` removes profile data, optionally the binary itself, and
//!   coordinates with the daemon service unit if installed.
//! - `update` self-replaces the binary atomically against a published GitHub
//!   release, with SHA256 verification and rollback on any failure.
//!
//! Both commands intentionally do *not* require a loaded `Config` — they
//! operate on `~/.rantaiclaw/` and the running binary path directly, so they
//! still work after a partial install or a corrupted config.

pub mod binary_path;
pub mod uninstall;
pub mod update;
pub mod update_service_restart;
pub mod update_snapshot;
