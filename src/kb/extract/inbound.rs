//! Inbound document extraction: bytes in, text out, inside fixed limits.
//!
//! The knowledge base's own extractors read a file from **disk**. An inbound
//! document arrives as bytes in the operator's memory and must never touch
//! the disk, so this module takes bytes directly.
//!
//! Every axis an attacker can push on is bounded here: the page count, the
//! number of characters that leave the extractor, the wall-clock time, and the
//! number of extractions running at once. The limits themselves are the
//! policy's, and the caller passes them in — the numbers live in
//! `channels::media` so there is one set of them and not two.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::kb::extract::pdf_splitter;
use crate::kb::{KbError, KbResult};

/// The limits one inbound extraction runs under.
#[derive(Debug, Clone, Copy)]
pub struct InboundLimits {
    /// The most pages a PDF may have. Counted before the text is extracted.
    pub max_pages: u32,
    /// The most characters of text that leave the extractor.
    pub max_chars: usize,
    /// The wall-clock budget for the whole extraction, waiting for the slot
    /// included.
    pub timeout: Duration,
}

/// The document types an inbound extraction understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundDocumentKind {
    /// A PDF, read through the same text layer the KB ingestion uses.
    Pdf,
    /// A DOCX, read through the office processor.
    Docx,
    /// A plain text file, read as UTF-8.
    Text,
}

/// What an extraction produced.
#[derive(Debug, Clone)]
pub struct InboundExtraction {
    /// The text, cut at `InboundLimits::max_chars`.
    pub text: String,
    /// The page count, for the types that have one.
    pub pages: Option<u32>,
    /// Whether the text was cut at `InboundLimits::max_chars`.
    pub truncated: bool,
}

/// One extraction at a time per process.
///
/// A hostile file can stall its parser, and a parser thread cannot be
/// cancelled. Letting several run at once would turn one bad file into many,
/// so the slot is held by the blocking task itself: it is released when the
/// task ends, not when the awaiting future gives up.
static EXTRACTION_SLOT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Extract the text of an inbound document.
///
/// The bytes never reach the disk. The extraction runs in a blocking task
/// under a single deadline, and only one runs at a time.
pub async fn extract_inbound_document(
    kind: InboundDocumentKind,
    bytes: Vec<u8>,
    limits: InboundLimits,
) -> KbResult<InboundExtraction> {
    let deadline = tokio::time::Instant::now() + limits.timeout;

    // The slot is taken before the work starts and released when the blocking
    // task ends — including when the task is abandoned by the deadline.
    let permit = tokio::time::timeout_at(deadline, EXTRACTION_SLOT.clone().acquire_owned())
        .await
        .map_err(|_| {
            KbError::Other(format!(
                "extraction timed out after {} seconds waiting for the extraction slot",
                limits.timeout.as_secs()
            ))
        })?
        .map_err(|_| KbError::Other("the extraction slot is closed".into()))?;

    let task = tokio::task::spawn_blocking(move || {
        // The permit lives here, so a timed-out extraction keeps the slot
        // until its parser actually returns.
        let _permit = permit;
        extract_blocking(kind, &bytes, limits)
    });

    match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(KbError::Other(format!("extraction task failed: {join}"))),
        Err(_) => Err(KbError::Other(format!(
            "extraction timed out after {} seconds",
            limits.timeout.as_secs()
        ))),
    }
}

/// The extraction itself, on the blocking thread.
fn extract_blocking(
    kind: InboundDocumentKind,
    bytes: &[u8],
    limits: InboundLimits,
) -> KbResult<InboundExtraction> {
    let (text, pages) = match kind {
        InboundDocumentKind::Pdf => extract_pdf(bytes, limits)?,
        InboundDocumentKind::Docx => (extract_docx(bytes)?, None),
        InboundDocumentKind::Text => (extract_text(bytes)?, None),
    };
    let (text, truncated) = cut_text(text, limits.max_chars);
    Ok(InboundExtraction {
        text,
        pages,
        truncated,
    })
}

/// A PDF's text, with the page count checked **before** the text is read.
///
/// The count comes from the bytes, so a 500-page file is refused without its
/// text ever being parsed.
fn extract_pdf(bytes: &[u8], limits: InboundLimits) -> KbResult<(String, Option<u32>)> {
    let pages = pdf_splitter::page_count(bytes)?;
    if pages > limits.max_pages {
        return Err(KbError::Extraction {
            extractor: "inbound-pdf".into(),
            message: format!(
                "the document has {pages} pages, over the {}-page limit",
                limits.max_pages
            ),
        });
    }
    let text = pdf_extract::extract_text_from_mem(bytes).map_err(|e| KbError::Extraction {
        extractor: "inbound-pdf".into(),
        message: format!("the PDF text layer could not be read: {e}"),
    })?;
    Ok((text, Some(pages)))
}

/// A DOCX's text.
///
/// The office processor lives behind the `kb-office` feature. A build without
/// it says so, rather than dropping the document.
#[cfg(feature = "kb-office")]
fn extract_docx(bytes: &[u8]) -> KbResult<String> {
    // The display name is fixed: the sender chose the file name, and it does
    // not belong in an error message.
    crate::kb::file::office::process_docx(bytes, "inbound document")
}

/// A DOCX's text, in a build without the office processor.
#[cfg(not(feature = "kb-office"))]
fn extract_docx(_bytes: &[u8]) -> KbResult<String> {
    Err(KbError::UnsupportedFileType(
        "reading DOCX documents needs the kb-office feature".into(),
    ))
}

/// A plain text file's text: UTF-8, and nothing that is not text.
fn extract_text(bytes: &[u8]) -> KbResult<String> {
    let text = std::str::from_utf8(bytes).map_err(|e| KbError::Extraction {
        extractor: "inbound-text".into(),
        message: format!("the file is not valid UTF-8: {e}"),
    })?;
    if text.contains('\0') {
        return Err(KbError::Extraction {
            extractor: "inbound-text".into(),
            message: "the file contains NUL bytes and is not a text document".into(),
        });
    }
    Ok(text.to_string())
}

/// The text, cut at the limit, and whether it was cut.
///
/// A cut is never silent: the caller turns the flag into a note the sender
/// sees, so the model is never handed a document that looks complete and is
/// not.
fn cut_text(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    (text.chars().take(max_chars).collect(), true)
}

/// A PDF with `page_count` pages, each carrying `text`.
///
/// Built by hand rather than kept as a fixture file: the cross-reference
/// offsets have to match the objects, and a test should show what it feeds
/// the parser instead of pointing at an opaque blob.
///
/// Shared with the channel policy's own tests: they need the same bytes to
/// prove that the **production** limits — not a hand-built limit — refuse a
/// PDF that is too long.
#[cfg(test)]
pub(crate) fn fixture_pdf(page_count: usize, text: &str) -> Vec<u8> {
    let first_page = 4;
    let first_content = first_page + page_count;
    let object_count = first_content + page_count - 1;

    let mut bodies: Vec<String> = Vec::new();
    bodies.push("<< /Type /Catalog /Pages 2 0 R >>".to_string());
    let kids: Vec<String> = (0..page_count)
        .map(|index| format!("{} 0 R", first_page + index))
        .collect();
    bodies.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {page_count} >>",
        kids.join(" ")
    ));
    bodies.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string());
    for index in 0..page_count {
        bodies.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] \
             /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> >> >>",
            first_content + index
        ));
    }
    for _ in 0..page_count {
        let stream = format!("BT /F1 12 Tf 20 150 Td ({text}) Tj ET");
        bodies.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }

    use std::fmt::Write as _;

    let mut out = String::from("%PDF-1.4\n");
    let mut offsets: Vec<usize> = Vec::new();
    for (index, body) in bodies.iter().enumerate() {
        offsets.push(out.len());
        let _ = writeln!(out, "{} 0 obj\n{body}\nendobj", index + 1);
    }
    let xref_offset = out.len();
    let _ = writeln!(out, "xref\n0 {}", object_count + 1);
    out.push_str("0000000000 65535 f \n");
    for offset in &offsets {
        let _ = writeln!(out, "{offset:010} 00000 n ");
    }
    let _ = writeln!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF",
        object_count + 1
    );
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_over_the_limit_is_cut_and_says_so() {
        let (text, truncated) = cut_text("abcdef".repeat(10), 25);
        assert_eq!(text.chars().count(), 25);
        assert!(truncated);
    }

    #[test]
    fn text_under_the_limit_is_left_alone() {
        let (text, truncated) = cut_text("hello".to_string(), 25);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    // ── The limits the tests run under ─────────────────────────────────────

    fn limits(max_pages: u32, max_chars: usize) -> InboundLimits {
        InboundLimits {
            max_pages,
            max_chars,
            timeout: Duration::from_secs(20),
        }
    }

    // ── Reading ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_text_document_is_read_as_it_is() {
        let extraction = extract_inbound_document(
            InboundDocumentKind::Text,
            b"hello from a document".to_vec(),
            limits(50, 50_000),
        )
        .await
        .expect("the text is read");
        assert_eq!(extraction.text, "hello from a document");
        assert!(!extraction.truncated);
        assert_eq!(extraction.pages, None);
    }

    #[tokio::test]
    async fn a_pdf_is_read_with_its_page_count() {
        let extraction = extract_inbound_document(
            InboundDocumentKind::Pdf,
            fixture_pdf(1, "Hello from a PDF"),
            limits(50, 50_000),
        )
        .await
        .expect("the PDF is read");
        assert_eq!(extraction.pages, Some(1));
        assert!(
            extraction.text.contains("Hello from a PDF"),
            "the text layer must come out: {}",
            extraction.text
        );
        assert!(!extraction.truncated);
    }

    // ── The limits ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_pdf_over_the_page_limit_is_refused_before_its_text_is_read() {
        let err = extract_inbound_document(
            InboundDocumentKind::Pdf,
            fixture_pdf(51, "page"),
            limits(50, 50_000),
        )
        .await
        .expect_err("51 pages is over the limit");
        assert!(err.to_string().contains("51 pages"), "{err}");
        assert!(err.to_string().contains("50-page limit"), "{err}");
    }

    #[tokio::test]
    async fn a_pdf_at_the_page_limit_is_read() {
        let extraction = extract_inbound_document(
            InboundDocumentKind::Pdf,
            fixture_pdf(50, "page"),
            limits(50, 50_000),
        )
        .await
        .expect("50 pages is within the limit");
        assert_eq!(extraction.pages, Some(50));
    }

    #[tokio::test]
    async fn a_long_document_is_cut_and_says_so() {
        let extraction = extract_inbound_document(
            InboundDocumentKind::Text,
            "x".repeat(100).into_bytes(),
            limits(50, 25),
        )
        .await
        .expect("the text is read");
        assert_eq!(extraction.text.chars().count(), 25);
        assert!(extraction.truncated, "a cut must be visible");
    }

    #[test]
    fn a_file_that_is_not_utf8_is_refused() {
        let err = extract_text(&[0xff, 0xfe, 0x00]).expect_err("not text");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn a_file_with_a_nul_byte_is_refused() {
        let err = extract_text(b"hello\0world").expect_err("not text");
        assert!(err.to_string().contains("NUL"), "{err}");
    }
}
