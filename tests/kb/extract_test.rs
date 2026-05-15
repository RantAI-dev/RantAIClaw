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
use rantaiclaw::kb::extract::vision_llm::VisionLlmExtractor;
use rantaiclaw::kb::extract::Extractor;
use rantaiclaw::kb::KbConfig;

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

// ----- task 5.4: VisionLlmExtractor (wiremock) ---------------------------

/// Build a [`KbConfig`] with all extractor knobs set for testing. Only the
/// vision base URL + api key matter for this suite; everything else is a
/// placeholder so we don't hit real `from_env()` and have to gate env-vars.
fn vision_cfg(base_url: String) -> KbConfig {
    KbConfig {
        extract_primary: "smart".into(),
        extract_fallback: "unpdf".into(),
        extract_smart_fallback: "rantaiclaw_test_model_a".into(),
        embedding_model: "rantaiclaw_test_model_a".into(),
        embedding_dim: 4,
        default_max_chunks: 8,
        rerank_enabled: false,
        rerank_provider: String::new(),
        rerank_model: "rantaiclaw_test_model_a".into(),
        rerank_initial_k: 20,
        rerank_final_k: 5,
        hybrid_bm25_enabled: true,
        contextual_retrieval_enabled: false,
        contextual_retrieval_model: "rantaiclaw_test_model_a".into(),
        query_expansion_enabled: false,
        query_expansion_model: "rantaiclaw_test_model_a".into(),
        query_expansion_paraphrases: 3,
        extract_vision_base_url: base_url,
        extract_vision_api_key: "rantaiclaw_test_key".into(),
        extract_mineru_base_url: String::new(),
        embedding_base_url: String::new(),
        embedding_api_key: String::new(),
        embed_batch_size: 100,
        embed_concurrency: 2,
        query_embed_cache_size: 8,
        query_embed_cache_ttl_ms: 60_000,
    }
}

fn make_pdf(pages: usize) -> Vec<u8> {
    let strings: Vec<String> = (1..=pages).map(|i| format!("Page {i} body")).collect();
    let refs: Vec<&str> = strings.iter().map(String::as_str).collect();
    build_pdf(&refs)
}

#[tokio::test]
async fn vision_llm_small_pdf_makes_single_call() {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let body = json!({
        "choices": [{ "message": { "content": "# rantaiclaw extracted" }}],
        "usage": { "prompt_tokens": 11, "completion_tokens": 22, "cost": 0.0004 }
    });
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = vision_cfg(format!("{}/chat", server.uri()));
    let ext = VisionLlmExtractor::with_options(cfg, "rantaiclaw/test-model".into(), 100, 5, 2);

    let pdf = make_pdf(3); // 3 pages — below segment_pages=5
    let result = ext.extract(&pdf).await.unwrap();
    assert_eq!(result.text, "# rantaiclaw extracted");
    assert_eq!(result.model, "rantaiclaw/test-model");
    assert_eq!(result.prompt_tokens, Some(11));
    assert_eq!(result.completion_tokens, Some(22));
    assert!((result.cost_usd.unwrap_or(0.0) - 0.0004).abs() < 1e-9);
}

#[tokio::test]
async fn vision_llm_large_pdf_splits_into_segments_and_sums_usage() {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // 8 pages with segment_pages=3 => ceil(8/3) = 3 segments => 3 calls.
    let body = json!({
        "choices": [{ "message": { "content": "segment text" }}],
        "usage": { "prompt_tokens": 100, "completion_tokens": 50, "cost": 0.001 }
    });
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(3)
        .mount(&server)
        .await;

    let cfg = vision_cfg(format!("{}/chat", server.uri()));
    let ext = VisionLlmExtractor::with_options(cfg, "rantaiclaw/test-model".into(), 100, 3, 4);
    let pdf = make_pdf(8);
    let result = ext.extract(&pdf).await.unwrap();

    assert_eq!(result.prompt_tokens, Some(300));
    assert_eq!(result.completion_tokens, Some(150));
    assert!((result.cost_usd.unwrap_or(0.0) - 0.003).abs() < 1e-9);
    assert_eq!(
        result.text,
        "segment text\n\nsegment text\n\nsegment text",
        "segments must concatenate with double-newline separators"
    );
    // Model field encodes the segment count for observability.
    assert!(
        result.model.contains("3 segments"),
        "model name should annotate segment count, got: {}",
        result.model
    );
}

#[tokio::test]
async fn vision_llm_4xx_surfaces_extraction_error() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let cfg = vision_cfg(format!("{}/chat", server.uri()));
    let ext = VisionLlmExtractor::with_options(cfg, "rantaiclaw/test-model".into(), 100, 5, 2);
    let pdf = make_pdf(2);
    let err = ext
        .extract(&pdf)
        .await
        .expect_err("4xx must surface KbError::Extraction");
    let msg = err.to_string();
    assert!(
        msg.contains("400"),
        "error should mention the HTTP status, got: {msg}"
    );
}
