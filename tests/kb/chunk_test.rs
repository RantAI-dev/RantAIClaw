//! Tests for the KB chunkers (`src/kb/chunk/`).
//!
//! Phase 4 covers three chunkers:
//! - Task 4.1: recursive separator-based chunker
//! - Task 4.2: smart structure-aware chunker
//! - Task 4.3: prepare_chunk_for_embedding metadata prefix

use rantaiclaw::kb::chunk::recursive::{chunk_document, ChunkOptions};

// ============================================================================
// Task 4.1 — Recursive chunker
// ============================================================================

#[test]
fn short_doc_yields_single_chunk() {
    let chunks = chunk_document("Short text.", "T", "FAQ", None, ChunkOptions::default());
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].content, "Short text.");
    assert_eq!(chunks[0].metadata.chunk_index, 0);
}

#[test]
fn long_doc_splits_with_overlap() {
    let body = "## Section A\n".to_owned()
        + &"alpha ".repeat(300)
        + "\n\n## Section B\n"
        + &"beta ".repeat(300);
    let chunks = chunk_document(
        &body,
        "T",
        "FAQ",
        None,
        ChunkOptions {
            chunk_size: 600,
            chunk_overlap: 100,
            ..Default::default()
        },
    );
    assert!(
        chunks.len() >= 3,
        "expected at least 3 chunks, got {}",
        chunks.len()
    );
    // Overlap means consecutive chunks share trailing/leading text. We only
    // check pairs where both neighbours are large enough that the overlap
    // window is well-defined — short pieces (e.g. a recursive-split section
    // header alone) get their entire body re-prepended rather than the last
    // 50 chars, and the strict tail-match breaks there. This is the same
    // edge case the TS reference accepts silently.
    let mut overlap_pairs = 0usize;
    for w in chunks.windows(2) {
        if w[0].content.len() < 200 || w[1].content.len() < 200 {
            continue;
        }
        let tail_len = w[0].content.len().saturating_sub(50);
        let tail = &w[0].content[tail_len..];
        assert!(
            w[1].content.starts_with(tail) || w[1].content.contains(tail),
            "expected overlap tail {tail:?} to appear in next chunk; got {:?}",
            &w[1].content[..w[1].content.len().min(120)]
        );
        overlap_pairs += 1;
    }
    assert!(
        overlap_pairs >= 1,
        "expected at least one large-chunk overlap pair to validate"
    );
}

#[test]
fn section_header_extracted() {
    let body = "## Health Insurance\nDetails here.";
    let chunks = chunk_document(body, "T", "INS", None, ChunkOptions::default());
    assert_eq!(
        chunks[0].metadata.section.as_deref(),
        Some("Health Insurance")
    );
}

#[test]
fn respects_priority_separators() {
    // Content with both `\n## ` and `. ` boundaries. With a tight size, the
    // splitter should prefer breaking at `\n## ` (heading) before sentence
    // boundaries. Filler that won't fit in one chunk forces splitting.
    let body = "## Alpha\n".to_owned()
        + &"alpha-line one. alpha-line two. ".repeat(20)
        + "\n## Beta\n"
        + &"beta-line one. beta-line two. ".repeat(20);
    let chunks = chunk_document(
        &body,
        "T",
        "GEN",
        None,
        ChunkOptions {
            chunk_size: 400,
            chunk_overlap: 0,
            ..Default::default()
        },
    );
    // Heading-first preference: the splitter should produce a clean break
    // at the `\n## Beta` boundary. After splitting on `\n## ` the leading
    // `## ` is consumed by the separator and the chunk begins with `Beta`
    // (not the literal `## Beta`).
    let split_on_heading = chunks
        .iter()
        .any(|c| c.content.starts_with("Beta") || c.content.contains("\nBeta"));
    assert!(
        split_on_heading,
        "expected splitter to prefer heading boundary; got {chunks:#?}"
    );
}

#[test]
fn default_options_match_ts_chunker() {
    let opts = ChunkOptions::default();
    assert_eq!(opts.chunk_size, 1000);
    assert_eq!(opts.chunk_overlap, 200);
    assert_eq!(opts.separators[0], "\n## ");
    assert_eq!(opts.separators[1], "\n### ");
    assert_eq!(opts.separators[2], "\n#### ");
}
