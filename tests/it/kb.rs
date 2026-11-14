//! The KB integration test module.
//!
//! Every phase adds a new `mod <thing>_test;` declaration. Keeping the KB
//! integration tests under `tests/it/` (instead of one binary per file at the
//! top of `tests/`) avoids polluting the global test target list and groups
//! related fixtures under `tests/it/kb/`.
//!
//! The KB modules are declared here directly, so each one lives in
//! `tests/it/kb/<name>.rs` — the directory a module file's own children
//! resolve to. There is deliberately no inline `mod kb { … }` wrapper: an
//! inline module's children resolve to `<parent file's directory>/<module
//! name>/`, which would push them one level deeper (`tests/it/kb/kb/`) and
//! give the inner module the same name as the one containing it — the shape
//! `clippy::module_inception` rejects.
//!
//! The printed test names are unchanged. Before the merge into one binary,
//! `tests/kb.rs` was a binary of its own and nested the very same modules in
//! an inline `mod kb { … }`, so a test printed as
//! `kb::agent_integration_test::test`. This file is now the module `kb`, and
//! the modules sit directly in it, so the same test still prints as
//! `kb::agent_integration_test::test` — byte for byte the same name.

#![cfg(feature = "kb")]

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
