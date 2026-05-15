//! Markdown file processor — port of `processMarkdown` in
//! `file-processor.ts:100-102`. There's no parsing or rewriting; the KB
//! chunker downstream consumes Markdown directly.

use std::path::Path;

use crate::kb::KbResult;

/// Read a Markdown file as UTF-8 text. Errors bubble up as
/// [`crate::kb::KbError::Io`].
pub async fn process_markdown(path: &Path) -> KbResult<String> {
    Ok(tokio::fs::read_to_string(path).await?)
}
