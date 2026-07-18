//! Tests for the KB extractor pipeline (`src/kb/extract/`).
//!
//! Each Phase-5 sub-task appends tests here. PDF fixtures are generated on
//! demand into `tests/fixtures/` so we don't commit binary blobs.
//!
//! HTTP-shaped extractors (mineru, vision_llm) are exercised against
//! wiremock mocks. Live OpenRouter coverage lives behind `#[ignore]` so the
//! default test run stays hermetic.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use rantaiclaw::kb::extract::hybrid::{merge_structural_with_text_layer, HybridExtractor};
use rantaiclaw::kb::extract::mineru::MineruExtractor;
use rantaiclaw::kb::extract::pdf_splitter::{get_page_count, split_pdf_by_page_count};
use rantaiclaw::kb::extract::smart_router::SmartRouterExtractor;
use rantaiclaw::kb::extract::text_layer_signals::{
    has_columnar_lines, has_dense_currency, is_unpdf_sufficient, is_unpdf_sufficient_with_size,
    RouterOpts,
};
use rantaiclaw::kb::extract::unpdf::UnpdfExtractor;
use rantaiclaw::kb::extract::vision_llm::VisionLlmExtractor;
use rantaiclaw::kb::extract::{ExtractionResult, Extractor};
use rantaiclaw::kb::{KbConfig, KbError, KbResult};

// ----- fixture helpers ---------------------------------------------------

fn fixtures_dir() -> PathBuf {
    // Tests run with CWD = crate root.
    PathBuf::from("tests/fixtures")
}

/// Write `bytes` to `path` atomically: serialize into a unique temp sibling,
/// then rename into place. Rename is atomic on the same filesystem, so a
/// concurrent reader never observes a half-written file. Without this, two
/// `#[tokio::test]` cases that both call an `ensure_*` helper for the same
/// fixture race: the first creates the path then streams bytes into it, and
/// the second sees `path.exists() == true` and reads the partial file,
/// failing with "Invalid file header".
fn write_fixture_atomic(path: &Path, bytes: &[u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{unique}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .unwrap_or_else(|e| panic!("write_fixture_atomic: write {}: {e}", tmp.display()));
    std::fs::rename(&tmp, path)
        .unwrap_or_else(|e| panic!("write_fixture_atomic: rename into {}: {e}", path.display()));
}

/// Build a tiny PDF whose page 1 contains `text`. Cached on disk so repeated
/// test runs don't regenerate it.
fn ensure_text_pdf() -> PathBuf {
    let path = fixtures_dir().join("sample-text.pdf");
    if path.exists() {
        return path;
    }
    std::fs::create_dir_all(fixtures_dir()).expect("ensure_text_pdf: create fixtures dir");
    let bytes = build_pdf(&["RantaiClawSample Hello World"]);
    write_fixture_atomic(&path, &bytes);
    path
}

/// Build an 8-page PDF. Each page has a single short string.
fn ensure_8_page_pdf() -> PathBuf {
    let path = fixtures_dir().join("sample-8-page.pdf");
    if path.exists() {
        return path;
    }
    std::fs::create_dir_all(fixtures_dir()).expect("ensure_8_page_pdf: create fixtures dir");
    let pages: Vec<String> = (1..=8).map(|i| format!("Page {i}")).collect();
    let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
    let bytes = build_pdf(&refs);
    write_fixture_atomic(&path, &bytes);
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
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            content
                .encode()
                .expect("build_pdf: encode lopdf page content stream"),
        ));
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
    doc.save_to(&mut buf)
        .expect("build_pdf: save lopdf Document to in-memory buffer");
    buf
}

#[allow(dead_code)] // used by task 5.3 splitter test
fn read_fixture(path: &Path) -> Vec<u8> {
    std::fs::read(path)
        .unwrap_or_else(|e| panic!("read_fixture: failed to read {}: {e}", path.display()))
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

// ----- unpdf page-count regression (2026-06-26) --------------------------

#[tokio::test]
async fn unpdf_reports_real_page_count() {
    // Regression: unpdf returned pages=None, which collapses SmartRouter's
    // per-page sufficiency threshold (300 chars/page) to a 300-char *total*
    // threshold so OCR fallback never fires on design/scan-heavy PDFs. The TS
    // original (unpdf-extractor.ts) returns totalPages.
    let path = ensure_8_page_pdf();
    let bytes = std::fs::read(&path).unwrap();
    let result = UnpdfExtractor::new().extract(&bytes).await.unwrap();
    assert_eq!(
        result.pages,
        Some(8),
        "unpdf must report the real page count"
    );
}

#[tokio::test]
async fn smart_router_routes_low_density_multipage_pdf_to_ocr_fallback() {
    // 8 pages x ~115 chars of prose = ~920 chars total.
    //   Buggy (unpdf pages=None -> 1): 920 >= 300 -> "sufficient" -> NO fallback (the bug).
    //   Fixed (unpdf pages=8):         920 < 300*8=2400 -> "insufficient" -> OCR fallback.
    let pages: Vec<String> = (1..=8)
        .map(|i| format!("Page {i} {}", "lorem ipsum dolor sit ".repeat(5)))
        .collect();
    let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
    let pdf = build_pdf(&refs);

    let text_layer = Box::new(UnpdfExtractor::new()); // REAL unpdf — exercises the pages plumbing
    let fallback = Box::new(CannedExtractor::ok("vision", "OCR STRUCTURED OUTPUT", 8));
    let router = SmartRouterExtractor::new(text_layer, fallback);

    let r = router.extract(&pdf).await.unwrap();
    assert!(
        r.model.contains("fallback:"),
        "low-density multipage PDF must route to OCR fallback once page count is real; got model: {}",
        r.model
    );
    assert_eq!(r.text, "OCR STRUCTURED OUTPUT");
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
        standalone_query_enabled: false,
        extract_vision_base_url: base_url,
        extract_vision_api_key: "rantaiclaw_test_key".into(),
        extract_mineru_base_url: String::new(),
        embedding_base_url: String::new(),
        embedding_api_key: String::new(),
        embed_batch_size: 100,
        embed_concurrency: 2,
        query_embed_cache_size: 8,
        query_embed_cache_ttl_ms: 60_000,
        openrouter_chat_url: "http://localhost".into(),
        intelligence_enabled: false,
        intelligence_model: "openai/gpt-4.1-nano".into(),
        intelligence_resolution: "exact".into(),
        graph_max_nodes: 200,
        graphrag_enabled: false,
        graphrag_max_neighbors: 20,
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
        result.text, "segment text\n\nsegment text\n\nsegment text",
        "segments must concatenate with double-newline separators"
    );
    // Model field encodes the segment count for observability.
    assert!(
        result.model.contains("3 segments"),
        "model name should annotate segment count, got: {}",
        result.model
    );
}

// ----- task 5.5: MineruExtractor (wiremock) ------------------------------

#[tokio::test]
async fn mineru_extractor_posts_multipart_and_parses_response() {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/extract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "text": "## RantaiClawSample\n\nbody",
            "ms": 4321,
            "pages": 2
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ext = MineruExtractor::new(server.uri()).unwrap();
    let result = ext.extract(b"%PDF-1.5\nhello").await.unwrap();
    assert_eq!(result.text, "## RantaiClawSample\n\nbody");
    assert_eq!(result.elapsed_ms, 4321);
    assert_eq!(result.pages, Some(2));
    assert_eq!(result.model, "mineru-2.5-pro");
}

#[tokio::test]
async fn mineru_extractor_normalizes_base_url_trailing_extract() {
    // The TS port accepts `/extract` suffix and trims it. Verify the Rust
    // port behaves the same: server still receives POST /extract, not
    // POST /extract/extract.
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/extract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "ok" })))
        .expect(1)
        .mount(&server)
        .await;

    let url_with_suffix = format!("{}/extract/", server.uri());
    let ext = MineruExtractor::new(url_with_suffix).unwrap();
    ext.extract(b"%PDF-1.5\n").await.unwrap();
}

#[tokio::test]
async fn mineru_extractor_surfaces_sidecar_errors() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/extract"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let ext = MineruExtractor::new(server.uri()).unwrap();
    let err = ext
        .extract(b"%PDF-1.5\n")
        .await
        .expect_err("5xx sidecar response must surface as KbError::Extraction");
    let msg = err.to_string();
    assert!(msg.contains("500"), "error must include status code: {msg}");
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

// ----- task 5.6: text_layer_signals --------------------------------------

#[test]
fn signals_clean_prose_is_sufficient() {
    let opts = RouterOpts::default();
    // 1500 chars of plain prose across an imagined 5-page doc — 300/page.
    let text = "lorem ipsum dolor sit amet ".repeat(80); // ~2160 chars
    assert!(is_unpdf_sufficient(&text, 5, &opts));
}

#[test]
fn signals_empty_text_triggers_fallback() {
    let opts = RouterOpts::default();
    assert!(!is_unpdf_sufficient("", 1, &opts));
    assert!(!is_unpdf_sufficient("short", 5, &opts));
}

#[test]
fn signals_columnar_lines_trigger_fallback() {
    // 6 columnar lines (>5 = default max_columnar_lines) — should fall back.
    let line = "Name        Age        Salary        Location";
    let columnar = std::iter::repeat_n(line, 20).collect::<Vec<_>>().join("\n");
    let opts = RouterOpts::default();
    assert!(has_columnar_lines(&columnar, opts.max_columnar_lines));
    // Padded to enough chars to clear min_chars_per_page; columnar guard
    // should still reject.
    let padded = format!("{}\n{}", columnar, "filler text ".repeat(100));
    assert!(!is_unpdf_sufficient(&padded, 1, &opts));
}

#[test]
fn signals_dense_currency_triggers_fallback() {
    let line = "Revenue $1,234.56 $7,890 $999,999.99 $12 $3,456";
    let text = std::iter::repeat_n(line, 10).collect::<Vec<_>>().join("\n");
    let opts = RouterOpts::default();
    assert!(has_dense_currency(&text, opts.max_currency_matches));
    assert!(!is_unpdf_sufficient(&text, 1, &opts));
}

#[test]
fn signals_low_text_to_filesize_ratio_triggers_fallback() {
    // Reproduces the real-world NQRust brochures: ~10 KB of scattered text in a
    // ~16 MB design PDF. The per-page char floor is CLEARED (so the page-count
    // check alone accepts it), but text is a tiny fraction of the file, so it's
    // image-heavy and must route to OCR.
    let opts = RouterOpts::default();
    let text = "lorem ipsum dolor ".repeat(560); // ~10080 chars
                                                 // Page-count check accepts it: 10080 >= 300 * 30.
    assert!(is_unpdf_sufficient(&text, 30, &opts));
    // Size-aware check rejects it: ~10 KB / 16 MB ≈ 0.063% < 0.5% floor.
    assert!(!is_unpdf_sufficient_with_size(&text, 30, 16_000_000, &opts));
    // Small file (<1 MB): density guard does not apply → stays sufficient.
    assert!(is_unpdf_sufficient_with_size(&text, 30, 50_000, &opts));
}

// ----- task 5.7: SmartRouter + Hybrid ------------------------------------

/// A canned-result extractor used to drive SmartRouter/Hybrid behavior
/// without standing up real PDF parsing.
struct CannedExtractor {
    name: String,
    result: KbResult<ExtractionResult>,
}

impl CannedExtractor {
    fn ok(name: &str, text: &str, pages: u32) -> Self {
        Self {
            name: name.into(),
            result: Ok(ExtractionResult {
                text: text.into(),
                elapsed_ms: 1,
                pages: Some(pages),
                model: name.into(),
                prompt_tokens: None,
                completion_tokens: None,
                cost_usd: None,
            }),
        }
    }
    fn err(name: &str, msg: &str) -> Self {
        Self {
            name: name.into(),
            result: Err(KbError::Extraction {
                extractor: name.into(),
                message: msg.into(),
            }),
        }
    }
}

#[async_trait]
impl Extractor for CannedExtractor {
    fn name(&self) -> &str {
        &self.name
    }
    async fn extract(&self, _: &[u8]) -> KbResult<ExtractionResult> {
        match &self.result {
            Ok(r) => Ok(r.clone()),
            Err(e) => Err(KbError::Extraction {
                extractor: self.name.clone(),
                message: e.to_string(),
            }),
        }
    }
}

#[tokio::test]
async fn smart_router_returns_text_layer_when_sufficient() {
    // ~600 chars over a 1-page doc — passes default min_chars_per_page=300.
    let text = "lorem ipsum dolor sit amet ".repeat(40);
    let text_layer = Box::new(CannedExtractor::ok("unpdf", &text, 1));
    let fallback = Box::new(CannedExtractor::err("vision", "should NOT be called"));
    let router = SmartRouterExtractor::new(text_layer, fallback);
    let r = router.extract(b"unused").await.unwrap();
    assert!(r.text.contains("lorem"));
    assert!(
        r.model.starts_with("smart("),
        "model should be wrapped, got: {}",
        r.model
    );
    assert!(
        !r.model.contains("fallback:"),
        "fallback must not be invoked, got: {}",
        r.model
    );
}

#[tokio::test]
async fn smart_router_routes_large_low_density_pdf_to_ocr_fallback() {
    // A 16 MB design PDF whose text layer clears the per-page floor but is a
    // tiny fraction of the file → image-heavy → must route to OCR. Exercises
    // SmartRouter threading the input byte length into the sufficiency check.
    let text = "lorem ipsum dolor ".repeat(560); // ~10 KB, clears 300 * 30
    let text_layer = Box::new(CannedExtractor::ok("unpdf", &text, 30));
    let fallback = Box::new(CannedExtractor::ok("vision", "OCR STRUCTURED OUTPUT", 30));
    let router = SmartRouterExtractor::new(text_layer, fallback);
    let big_pdf = vec![0u8; 16_000_000];
    let r = router.extract(&big_pdf).await.unwrap();
    assert!(
        r.model.contains("fallback:"),
        "large low-density PDF must route to OCR, got model: {}",
        r.model
    );
    assert_eq!(r.text, "OCR STRUCTURED OUTPUT");
}

#[tokio::test]
async fn smart_router_falls_back_when_text_layer_empty() {
    let text_layer = Box::new(CannedExtractor::ok("unpdf", "", 1));
    let fallback = Box::new(CannedExtractor::ok("vision", "structured output", 1));
    let router = SmartRouterExtractor::new(text_layer, fallback);
    let r = router.extract(b"unused").await.unwrap();
    assert_eq!(r.text, "structured output");
    assert!(
        r.model.contains("fallback:"),
        "model must mark fallback usage, got: {}",
        r.model
    );
}

#[tokio::test]
async fn smart_router_falls_back_when_text_layer_errors() {
    let text_layer = Box::new(CannedExtractor::err("unpdf", "boom"));
    let fallback = Box::new(CannedExtractor::ok("vision", "structured output", 1));
    let router = SmartRouterExtractor::new(text_layer, fallback);
    let r = router.extract(b"unused").await.unwrap();
    assert_eq!(r.text, "structured output");
}

#[tokio::test]
async fn smart_router_surfaces_aggregate_error_when_both_fail() {
    let text_layer = Box::new(CannedExtractor::err("unpdf", "tlfail"));
    let fallback = Box::new(CannedExtractor::err("vision", "fbfail"));
    let router = SmartRouterExtractor::new(text_layer, fallback);
    let err = router.extract(b"unused").await.expect_err("must fail");
    let msg = err.to_string();
    assert!(msg.contains("tlfail"), "msg must include tl error: {msg}");
    assert!(msg.contains("fbfail"), "msg must include fb error: {msg}");
}

#[tokio::test]
async fn hybrid_merges_both_outputs() {
    let structural = Box::new(CannedExtractor::ok(
        "mineru",
        "# Heading\n\nQuick brown fox jumps over the lazy dog.",
        2,
    ));
    let text_layer = Box::new(CannedExtractor::ok(
        "unpdf",
        "Quick brown fox jumps over the lazy dog.",
        2,
    ));
    let hybrid = HybridExtractor::new(structural, text_layer);
    let r = hybrid.extract(b"unused").await.unwrap();
    assert!(r.text.contains("# Heading"), "should keep heading");
    assert!(r.text.contains("Quick brown fox"), "should keep prose");
    assert!(
        r.model.starts_with("hybrid("),
        "model name must annotate hybrid: {}",
        r.model
    );
}

#[tokio::test]
async fn hybrid_degrades_to_structural_when_text_layer_fails() {
    let structural = Box::new(CannedExtractor::ok("mineru", "## structural body", 3));
    let text_layer = Box::new(CannedExtractor::err("unpdf", "boom"));
    let hybrid = HybridExtractor::new(structural, text_layer);
    let r = hybrid.extract(b"unused").await.unwrap();
    assert_eq!(r.text, "## structural body");
    // The single-source path returns the inner result unchanged, so model
    // stays "mineru" rather than wrapping.
    assert_eq!(r.model, "mineru");
}

#[tokio::test]
async fn hybrid_degrades_to_text_layer_when_structural_fails() {
    let structural = Box::new(CannedExtractor::err("mineru", "boom"));
    let text_layer = Box::new(CannedExtractor::ok("unpdf", "flat prose", 3));
    let hybrid = HybridExtractor::new(structural, text_layer);
    let r = hybrid.extract(b"unused").await.unwrap();
    assert_eq!(r.text, "flat prose");
    assert_eq!(r.model, "unpdf");
}

#[tokio::test]
async fn hybrid_errors_when_both_fail() {
    let structural = Box::new(CannedExtractor::err("mineru", "sfail"));
    let text_layer = Box::new(CannedExtractor::err("unpdf", "tfail"));
    let hybrid = HybridExtractor::new(structural, text_layer);
    let err = hybrid.extract(b"unused").await.expect_err("must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("sfail"),
        "msg must include structural err: {msg}"
    );
    assert!(
        msg.contains("tfail"),
        "msg must include text-layer err: {msg}"
    );
}

#[test]
fn merge_preserves_tables_verbatim() {
    let structural = "| a | b |\n|---|---|\n| 1 | 2 |\n\nSome prose text here.";
    let text_layer = "Some prose text here.";
    let merged = merge_structural_with_text_layer(structural, text_layer);
    assert!(merged.contains("| a | b |"));
    assert!(merged.contains("Some prose text"));
}

#[test]
fn merge_handles_non_ascii_prose_without_panic() {
    // Regression for a `try_substitute` slicing bug: when `to_lowercase()`
    // changed byte length (German `ß` -> `ss`, Turkish `İ` -> `i̇`, etc.) or
    // when multi-byte UTF-8 chars made byte indices land off char boundaries,
    // the previous code panicked while slicing the original text layer with
    // an offset computed from its lowercased copy. The merge must now run to
    // completion on real-world non-ASCII content.
    let structural = "# Pembayaran Berbasis Saham\n\nPembayaran Berbasis Saham diakui sebagai biaya pada periode berjalan.";
    let text_layer = "Pembayaran Berbasis Saham diakui sebagai biaya pada periode berjalan.";
    let merged = merge_structural_with_text_layer(structural, text_layer);
    assert!(
        merged.contains("# Pembayaran Berbasis Saham"),
        "heading must survive: {merged}"
    );
    assert!(
        merged.contains("Pembayaran Berbasis Saham diakui"),
        "prose must survive: {merged}"
    );

    // German `ß` -> `ss` exercises the lowercased-bytes-change branch.
    let structural_de = "Die Straße ist lang und kurvig genug für mehrere Sätze.";
    let text_layer_de = "Die Straße ist lang und kurvig genug für mehrere Sätze.";
    let _ = merge_structural_with_text_layer(structural_de, text_layer_de);

    // Mixed multi-byte (Japanese kana + ASCII) must not slice on a UTF-8 boundary.
    let structural_mb = "概要 — quick brown fox jumps over the lazy dog 終わり.";
    let text_layer_mb = "概要 — quick brown fox jumps over the lazy dog 終わり.";
    let _ = merge_structural_with_text_layer(structural_mb, text_layer_mb);
}

// ----- kb-ocr spike: OllamaOcrExtractor (wiremock) -----------------------
//
// SPIKE prototype behind `--features kb-ocr` (non-default). Design note:
// docs/kb/ocr-design.md. Mirrors the mineru_extractor_* tests above: no
// live Ollama server required, `OllamaOcrExtractor::new` points directly at
// the wiremock server so nothing here touches process env vars.

#[cfg(feature = "kb-ocr")]
#[tokio::test]
async fn ocr_ollama_extractor_posts_image_and_parses_response() {
    use rantaiclaw::kb::extract::ocr_ollama::OllamaOcrExtractor;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "response": "RantaiClawSample OCR text",
            "done": true
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ext = OllamaOcrExtractor::new(format!("{}/api/generate", server.uri()), "llava");
    // A 1x1 PNG signature — content is irrelevant, the mock ignores the body.
    let png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let result = ext.extract(png).await.unwrap();
    assert_eq!(result.text, "RantaiClawSample OCR text");
    assert_eq!(result.model, "llava");
    assert_eq!(result.pages, Some(1));
}

#[cfg(feature = "kb-ocr")]
#[tokio::test]
async fn ocr_ollama_extractor_surfaces_endpoint_errors() {
    use rantaiclaw::kb::extract::ocr_ollama::OllamaOcrExtractor;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(ResponseTemplate::new(500).set_body_string("model not found"))
        .mount(&server)
        .await;

    let ext = OllamaOcrExtractor::new(format!("{}/api/generate", server.uri()), "llava");
    let err = ext
        .extract(b"\x89PNG")
        .await
        .expect_err("5xx endpoint response must surface as KbError::Extraction");
    let msg = err.to_string();
    assert!(msg.contains("500"), "error must include status code: {msg}");
}

// ----- live integration (skipped unless OPENROUTER_API_KEY set) ----------

#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY"]
async fn vision_llm_extracts_real_pdf() {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY env var required for live test");
    let mut cfg = vision_cfg("https://openrouter.ai/api/v1/chat/completions".into());
    cfg.extract_vision_api_key = api_key;
    let ext = VisionLlmExtractor::new(cfg, "openai/gpt-4.1-nano".into());
    let pdf = make_pdf(2);
    let r = ext
        .extract(&pdf)
        .await
        .expect("live extract should succeed");
    assert!(!r.text.is_empty());
}
