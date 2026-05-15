//! Tests for the KB extractor pipeline (`src/kb/extract/`).
//!
//! Each Phase-5 sub-task appends tests here. PDF fixtures are generated on
//! demand into `tests/fixtures/` so we don't commit binary blobs.
//!
//! HTTP-shaped extractors (mineru, vision_llm) are exercised against
//! wiremock mocks. Live OpenRouter coverage lives behind `#[ignore]` so the
//! default test run stays hermetic.

use std::path::{Path, PathBuf};

use rantaiclaw::kb::extract::pdf_splitter::{get_page_count, split_pdf_by_page_count};
use rantaiclaw::kb::extract::unpdf::UnpdfExtractor;
use rantaiclaw::kb::extract::Extractor;

// ----- fixture helpers ---------------------------------------------------

fn fixtures_dir() -> PathBuf {
    // Tests run with CWD = crate root.
    PathBuf::from("tests/fixtures")
}

/// Build a tiny PDF whose page 1 contains `text`. Cached on disk so repeated
/// test runs don't regenerate it.
fn ensure_text_pdf() -> PathBuf {
    let path = fixtures_dir().join("sample-text.pdf");
    if path.exists() {
        return path;
    }
    std::fs::create_dir_all(fixtures_dir()).unwrap();
    let bytes = build_pdf(&["RantaiClawSample Hello World"]);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Build an 8-page PDF. Each page has a single short string.
fn ensure_8_page_pdf() -> PathBuf {
    let path = fixtures_dir().join("sample-8-page.pdf");
    if path.exists() {
        return path;
    }
    std::fs::create_dir_all(fixtures_dir()).unwrap();
    let pages: Vec<String> = (1..=8).map(|i| format!("Page {i}")).collect();
    let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
    let bytes = build_pdf(&refs);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Build a minimal PDF with one page per element in `pages`.
///
/// Uses `lopdf` directly so we control every byte and avoid pulling in an
/// extra dep just for fixtures. Each page renders `text` in Helvetica.
fn build_pdf(pages: &[&str]) -> Vec<u8> {
    use lopdf::content::{Content, Operation};
    use lopdf::{dictionary, Document, Object, Stream};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut page_ids: Vec<Object> = Vec::with_capacity(pages.len());
    for text in pages {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![100.into(), 600.into()]),
                Operation::new("Tj", vec![Object::string_literal(*text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        page_ids.push(page_id.into());
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids,
            "Count" => pages.len() as i64,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.compress();
    let mut buf: Vec<u8> = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

#[allow(dead_code)] // used by task 5.3 splitter test
fn read_fixture(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

// ----- task 5.2: UnpdfExtractor ------------------------------------------

#[tokio::test]
async fn unpdf_extracts_sample_pdf() {
    let path = ensure_text_pdf();
    let bytes = std::fs::read(&path).unwrap();
    let result = UnpdfExtractor::new().extract(&bytes).await.unwrap();
    assert_eq!(result.model, "unpdf");
    assert!(
        result.text.contains("RantaiClawSample"),
        "expected sample marker in unpdf output, got: {:?}",
        result.text
    );
}

#[tokio::test]
async fn unpdf_returns_extraction_error_on_garbage_input() {
    let bytes = b"not a pdf at all";
    let err = UnpdfExtractor::new()
        .extract(bytes)
        .await
        .expect_err("garbage bytes must not parse");
    let msg = err.to_string();
    assert!(
        msg.contains("extraction failed") || msg.contains("unpdf"),
        "expected extraction-failure message, got: {msg}"
    );
}

// ----- task 5.3: PDF splitter --------------------------------------------

#[tokio::test]
async fn pdf_splitter_get_page_count_returns_total() {
    let path = ensure_8_page_pdf();
    let bytes = read_fixture(&path);
    let count = get_page_count(&bytes).await.unwrap();
    assert_eq!(count, 8);
}

#[tokio::test]
async fn pdf_splitter_splits_8_pages_by_3_into_three_segments() {
    let path = ensure_8_page_pdf();
    let bytes = read_fixture(&path);
    let segments = split_pdf_by_page_count(&bytes, 3).await.unwrap();
    assert_eq!(segments.len(), 3, "expected 3 segments of 3+3+2 pages");
    // Each segment must be a valid PDF whose page count is what we expect.
    let expected_pages = [3u32, 3u32, 2u32];
    for (i, seg) in segments.iter().enumerate() {
        let pages = get_page_count(seg).await.unwrap_or_else(|e| {
            panic!("segment {i} is not a valid PDF: {e}");
        });
        assert_eq!(
            pages, expected_pages[i],
            "segment {i} should have {} pages",
            expected_pages[i]
        );
    }
}

#[tokio::test]
async fn pdf_splitter_no_op_when_pages_fit_one_segment() {
    let path = ensure_8_page_pdf();
    let bytes = read_fixture(&path);
    let segments = split_pdf_by_page_count(&bytes, 10).await.unwrap();
    assert_eq!(
        segments.len(),
        1,
        "splitting by a segment >= total pages must return a single segment"
    );
    assert_eq!(segments[0], bytes, "single segment must be the input bytes");
}
