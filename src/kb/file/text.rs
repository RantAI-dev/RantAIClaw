//! Plain-text / structured-text file processor. Covers code, csv/tsv, json,
//! yaml, toml, html/xml, log, etc. — see `TEXT_EXTENSIONS` in
//! `super::mod`. No transformation: the chunker downstream is content-aware
//! and the upstream TS source likewise just hands the buffer through to
//! `extractTextFromBuffer`, which for text MIME types is identity.

use std::path::Path;

use crate::kb::KbResult;

/// Read a text file as UTF-8. For binary content masquerading as text the
/// caller will see [`crate::kb::KbError::Io`] from `read_to_string`'s
/// underlying `InvalidData` rejection.
pub async fn process_text(path: &Path) -> KbResult<String> {
    Ok(tokio::fs::read_to_string(path).await?)
}
