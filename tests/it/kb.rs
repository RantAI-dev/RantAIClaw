//! The KB integration test module.
//!
//! Every phase adds a new `mod <thing>_test;` declaration. Keeping the KB
//! integration tests under `tests/it/` (instead of one binary per file at the
//! top of `tests/`) avoids polluting the global test target list and groups
//! related fixtures under `tests/it/kb/`.
//!
//! The inline `mod kb { … }` below is what the KB tests were always nested
//! in (`tests/kb.rs` declared `mod kb { … }` when it was its own binary).
//! Keeping it keeps every KB test's name exactly as it was — `kb::kb::x`
//! before, `it::kb::kb::x` now — so no test name moves. Rust resolves the
//! children of an inline module to `<parent file's directory>/<module name>/`,
//! which is why they live in `tests/it/kb/kb/` and not in `tests/it/kb/`.

#![cfg(feature = "kb")]

mod kb {
    pub mod agent_integration_test;
    pub mod api_test;
    pub mod chunk_test;
    pub mod cli_test;
    pub mod common;
    pub mod config_test;
    pub mod embed_test;
    pub mod extract_test;
    pub mod file_test;
    pub mod intelligence_test;
    pub mod maintenance_test;
    pub mod parity_helpers;
    pub mod parity_test;
    pub mod rerank_test;
    pub mod retrieve_test;
    pub mod store_sqlite_test;
}
