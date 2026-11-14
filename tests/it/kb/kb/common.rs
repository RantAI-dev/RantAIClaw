//! Cross-file test helpers for the KB test module.
//!
//! The KB tests mutate process-wide env vars (`KB_*`, `OPENROUTER_API_KEY`,
//! etc.). They all live in ONE test binary now (`tests/it/main.rs`), so a
//! per-file static mutex would no longer serialize anything: two files that
//! mutate different variables still clobber each other's, because the
//! variables are process-global. The binary's one shared lock
//! ([`crate::common::ENV_LOCK`]) is what serializes them, and re-exporting it
//! here keeps the KB call sites reading `super::common::ENV_LOCK` /
//! `crate::kb::kb::common::ENV_LOCK`.
//!
//! The lock is a `tokio::sync::Mutex`, so an async test holds the guard
//! across an `.await` with `.lock().await` and a sync test takes it with
//! `.blocking_lock()`.

pub use crate::common::ENV_LOCK;
