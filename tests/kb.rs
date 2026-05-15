//! Integration test binary for the KB subsystem.
//!
//! Each phase adds a new `mod <thing>_test;` declaration. Keeping the KB
//! integration tests in a single test binary (instead of one per file at
//! the top of `tests/`) avoids polluting the global test target list and
//! groups related fixtures under `tests/kb/`.

#![cfg(feature = "kb")]

mod kb {
    pub mod common;
    pub mod chunk_test;
    pub mod config_test;
    pub mod embed_test;
    pub mod extract_test;
    pub mod file_test;
    pub mod retrieve_test;
    pub mod store_sqlite_test;
}
