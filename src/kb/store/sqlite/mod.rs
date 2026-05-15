//! sqlite-vec + FTS5 backend for [`super::KbStore`].
//!
//! Wired in incrementally across tasks 2.2–2.7. Each submodule keeps a single
//! responsibility (schema, document CRUD, chunk insert/search, BM25, drift).
