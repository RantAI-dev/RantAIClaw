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
//!
//! The bytes themselves are parsed in a **child process** (see
//! [`crate::kb::extract::child_process`]): a parser that panics, aborts,
//! loops or allocates takes the child down, not the agent. This module owns
//! the slot that keeps one extraction at a time and hands the work over.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::kb::extract::child_process;
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

impl InboundDocumentKind {
    /// The name the child process knows this kind by.
    pub(crate) fn env_value(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Text => "text",
        }
    }
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
/// A hostile file can stall its parser, and a parser that cannot be
/// cancelled would block every later document. The child process is killed
/// at the deadline, so the slot is released when the extraction ends — not
/// when the parser would have finished, had it ever finished.
static EXTRACTION_SLOT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Extract the text of an inbound document.
///
/// The bytes never reach the disk. The extraction runs in a child process
/// under a single deadline, and only one runs at a time.
pub async fn extract_inbound_document(
    kind: InboundDocumentKind,
    bytes: Vec<u8>,
    limits: InboundLimits,
) -> KbResult<InboundExtraction> {
    extract_with_slot(kind.env_value(), &[], bytes, limits).await
}

/// Extract a document of a test-only kind, under the same slot and the same
/// deadline as a real one.
///
/// The kinds exist in a test build only; they are how the process boundary
/// is tested without a hostile file.
#[cfg(test)]
pub(crate) async fn extract_test_document(
    kind: &str,
    extra_env: &[(&str, &str)],
    bytes: Vec<u8>,
    limits: InboundLimits,
) -> KbResult<InboundExtraction> {
    extract_with_slot(kind, extra_env, bytes, limits).await
}

/// Take the slot, then hand the document to the child process.
async fn extract_with_slot(
    kind: &str,
    extra_env: &[(&str, &str)],
    bytes: Vec<u8>,
    limits: InboundLimits,
) -> KbResult<InboundExtraction> {
    let deadline = tokio::time::Instant::now() + limits.timeout;

    // The slot is taken before the work starts and released when the
    // extraction ends.
    let permit = tokio::time::timeout_at(deadline, EXTRACTION_SLOT.clone().acquire_owned())
        .await
        .map_err(|_| {
            KbError::Other(format!(
                "extraction timed out after {} seconds waiting for the extraction slot",
                limits.timeout.as_secs()
            ))
        })?
        .map_err(|_| KbError::Other("the extraction slot is closed".into()))?;

    // The permit is held for the whole child run. The child is killed at the
    // deadline, so the slot is always released: a parser that never returns
    // cannot block the next document.
    let _permit = permit;
    child_process::run_child(kind, extra_env, bytes, limits, deadline).await
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
    build_fixture_pdf(page_count, text, None)
}

/// A PDF whose font names an encoding no parser knows.
///
/// `pdf-extract` panics on it (`encoding_to_unicode_table`), which is the
/// crash the child process exists to contain.
#[cfg(test)]
pub(crate) fn hostile_fixture_pdf(page_count: usize, text: &str) -> Vec<u8> {
    build_fixture_pdf(page_count, text, Some("Foo"))
}

/// The fixture PDF, with an optional font encoding.
#[cfg(test)]
fn build_fixture_pdf(page_count: usize, text: &str, encoding: Option<&str>) -> Vec<u8> {
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
    let font = match encoding {
        Some(name) => {
            format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /{name} >>")
        }
        None => "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    };
    bodies.push(font);
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
}
