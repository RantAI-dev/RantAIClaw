//! Maintenance operations for the Knowledge Base.
//!
//! These are explicit operator actions — never auto-triggered. The two
//! supported flows today:
//!
//! - [`check_drift`] — read-only report of chunks embedded with a model
//!   other than the currently-configured one.
//! - `bulk_re_embed` (Task 9.2) — the corrective action when drift > 0.

pub mod drift;

pub use drift::{check_drift, DriftReport};
