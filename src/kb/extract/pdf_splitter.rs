//! PDF page splitter — used by the vision-LLM extractor to chunk large PDFs
//! into N-page segments that fit inside a single model output budget.
//!
//! Port of `extractors/pdf-splitter.ts`. Built on `lopdf` (the JS source uses
//! `pdf-lib`).

use crate::kb::{KbError, KbResult};
use lopdf::Document;

/// Split `pdf_bytes` into `pages_per_segment`-sized segments.
///
/// If the total page count is `<= pages_per_segment`, returns `vec![pdf_bytes]`
/// unchanged (the caller can avoid an extra copy by also short-circuiting,
/// but this keeps the call site uniform).
///
/// Mirrors `splitPdfByPageCount` from the TS source.
pub async fn split_pdf_by_page_count(
    pdf_bytes: &[u8],
    pages_per_segment: u32,
) -> KbResult<Vec<Vec<u8>>> {
    if pages_per_segment < 1 {
        return Err(KbError::Config("pagesPerSegment must be >= 1".into()));
    }
    // `lopdf` is synchronous and CPU-bound on large PDFs; offload.
    let bytes = pdf_bytes.to_vec();
    let pages_per_segment = pages_per_segment as usize;
    tokio::task::spawn_blocking(move || split_blocking(&bytes, pages_per_segment))
        .await
        .map_err(|e| KbError::Other(format!("pdf_splitter join error: {e}")))?
}

fn split_blocking(pdf_bytes: &[u8], pages_per_segment: usize) -> KbResult<Vec<Vec<u8>>> {
    let source = Document::load_mem(pdf_bytes).map_err(|e| KbError::Extraction {
        extractor: "pdf_splitter".into(),
        message: format!("load failed: {e}"),
    })?;
    let pages: Vec<u32> = source.get_pages().keys().copied().collect();
    let total_pages = pages.len();
    if total_pages <= pages_per_segment {
        return Ok(vec![pdf_bytes.to_vec()]);
    }

    let mut segments = Vec::new();
    let mut start = 0usize;
    let total_pages_u32 = u32::try_from(total_pages).map_err(|_| KbError::Extraction {
        extractor: "pdf_splitter".into(),
        message: format!("page count {total_pages} exceeds u32"),
    })?;
    while start < total_pages {
        let end = (start + pages_per_segment).min(total_pages);
        let start_u32 = u32::try_from(start).unwrap_or(u32::MAX);
        let end_u32 = u32::try_from(end).unwrap_or(u32::MAX);
        // 1-based page numbers per lopdf convention; the keys in get_pages()
        // are already the document's page numbers but we delete by 1-based
        // index against a fresh clone to keep the math straightforward.
        let mut segment = source.clone();
        let to_delete: Vec<u32> = (1..=total_pages_u32)
            .filter(|p| !(*p > start_u32 && *p <= end_u32))
            .collect();
        segment.delete_pages(&to_delete);
        let mut buf: Vec<u8> = Vec::new();
        segment.save_to(&mut buf).map_err(|e| KbError::Extraction {
            extractor: "pdf_splitter".into(),
            message: format!("save failed: {e}"),
        })?;
        segments.push(buf);
        start = end;
    }
    Ok(segments)
}

/// Return the page count without serializing or splitting. Mirrors
/// `getPdfPageCount` from the TS source.
pub async fn get_page_count(pdf_bytes: &[u8]) -> KbResult<u32> {
    let bytes = pdf_bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        let doc = Document::load_mem(&bytes).map_err(|e| KbError::Extraction {
            extractor: "pdf_splitter".into(),
            message: format!("load failed: {e}"),
        })?;
        u32::try_from(doc.get_pages().len()).map_err(|_| KbError::Extraction {
            extractor: "pdf_splitter".into(),
            message: "page count exceeds u32".into(),
        })
    })
    .await
    .map_err(|e| KbError::Other(format!("pdf_splitter join error: {e}")))?
}
