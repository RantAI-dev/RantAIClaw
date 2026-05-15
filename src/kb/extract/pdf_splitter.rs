//! PDF page splitter shell — real implementation lands in task 5.3.

use crate::kb::{KbError, KbResult};

pub async fn split_pdf_by_page_count(
    _pdf_bytes: &[u8],
    _pages_per_segment: u32,
) -> KbResult<Vec<Vec<u8>>> {
    Err(KbError::Other(
        "pdf_splitter not implemented yet (task 5.3)".into(),
    ))
}

pub async fn get_page_count(_pdf_bytes: &[u8]) -> KbResult<u32> {
    Err(KbError::Other(
        "pdf_splitter not implemented yet (task 5.3)".into(),
    ))
}
