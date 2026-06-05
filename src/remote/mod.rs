//! Remote-install transport layer.
//!
//! Provides the shared SSH session registry (russh) and pure helpers used by
//! the `ssh` and `pty` tools to pilot installers on remote hosts over tmux.
//!
//! Gated behind the `remote-install` cargo feature.

pub mod keys;
pub mod registry;
pub mod session;
