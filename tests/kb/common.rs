//! Cross-file test helpers for the KB integration binary.
//!
//! `ENV_LOCK` serializes any test that mutates process-wide env vars
//! (`KB_*`, `OPENROUTER_API_KEY`, etc.). All four KB test modules
//! (`config_test`, `embed_test`, `retrieve_test`, plus future additions)
//! compile into the SAME test binary (`tests/kb.rs`) — a per-file static
//! mutex only serializes within its own module, so cross-module tests
//! would race each other on shared env state. Keeping the lock here
//! makes the shared contract explicit.

use std::sync::Mutex;

pub static ENV_LOCK: Mutex<()> = Mutex::new(());
