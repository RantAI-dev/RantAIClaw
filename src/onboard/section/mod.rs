//! Setup-section root. Wave 3 replaces the contents of this file with
//! the real `SetupSection` trait + 13 section module declarations; until
//! then we expose only the `_stub` trait + the leaf modules that
//! Waves 2C/2D/2E ship.
//!
//! Race rule (per plan §"Cross-wave coordination — _stub.rs"): each
//! Wave-2 leaf either creates this file (first to push) or appends its
//! own `pub mod <leaf>;` line (later pushes). `_stub.rs` is shared.

pub mod _stub;

pub mod mcp;
pub mod persona;
pub mod skills;
