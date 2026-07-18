//! `OllamaOcrExtractor` — SPIKE prototype for the `kb-ocr` feature.
//!
//! Design note: `docs/kb/ocr-design.md`. This is the smallest possible
//! end-to-end slice proving the "pre-route through an Ollama OCR pipeline"
//! shape referenced by the `TODO(kb-ocr)` markers in `kb::file::mod` and
//! `kb::file::image` — NOT a production-grade OCR pipeline. It is gated
//! behind `--features kb-ocr` (non-default) and is not wired into the
//! default `kb` build in any way.
//!
//! Shape mirrors [`crate::kb::extract::mineru::MineruExtractor`]: a thin HTTP
//! client that POSTs to a self-hosted sidecar (here, a local Ollama server's
//! `/api/generate` endpoint) and maps the JSON response into
//! [`ExtractionResult`]. Unlike `MineruExtractor` (which POSTs a whole PDF),
//! this extractor expects **image bytes** (PNG/JPEG/etc.) — Ollama's
//! `images` field takes raw base64 image data, no PDF support. Callers that
//! need PDF-page OCR must rasterize a page to an image first; this spike
//! does not implement that step (see the design note's open questions).
//!
//! No new Cargo dependency: reuses `reqwest`, `serde`, `serde_json`, and
//! `base64`, all already unconditional dependencies of this crate.
#![cfg(feature = "kb-ocr")]

use std::time::Instant;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use crate::kb::extract::{elapsed_ms, ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

/// Default local Ollama endpoint. Overridable via `KB_EXTRACT_OCR_BASE_URL`.
const DEFAULT_BASE_URL: &str = "http://localhost:11434/api/generate";

/// Default vision-capable Ollama model tag. Overridable via
/// `KB_EXTRACT_OCR_MODEL`. Operators must `ollama pull` a vision model
/// themselves — this prototype does not bundle or download models (out of
/// scope per the plan; see CLAUDE.md §10 anti-patterns).
const DEFAULT_MODEL: &str = "llava";

/// Prompt kept intentionally simple — this is a spike, not a tuned prompt.
/// A production port should treat this as a real design decision (structured
/// output? per-`DocumentTypeHint` prompts? see design note).
const OCR_PROMPT: &str = "Transcribe all text visible in this image verbatim. \
Output only the transcribed text, no commentary.";

#[derive(Debug)]
pub struct OllamaOcrExtractor {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct OllamaGenerateResponse {
    #[serde(default)]
    response: String,
}

impl OllamaOcrExtractor {
    /// Build an extractor against an explicit endpoint + model. Primary
    /// constructor for tests (point `base_url` at a `wiremock` server) and
    /// for callers that already resolved config elsewhere.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Allow injecting a custom client for tests, mirroring
    /// `MineruExtractor::with_client`.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Production constructor: reads `KB_EXTRACT_OCR_BASE_URL` /
    /// `KB_EXTRACT_OCR_MODEL`, falling back to a local Ollama default.
    ///
    /// Spike simplification: these two knobs live directly on this struct's
    /// env resolution rather than on `KbConfig`, so this prototype does not
    /// touch `src/kb/config.rs` (out of the plan's declared scope). A
    /// production port should promote them to `KbConfig` fields for
    /// consistency with `extract_vision_base_url` / `extract_mineru_base_url`.
    pub fn from_env() -> Self {
        let base_url = std::env::var("KB_EXTRACT_OCR_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model =
            std::env::var("KB_EXTRACT_OCR_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Self::new(base_url, model)
    }
}

#[async_trait]
impl Extractor for OllamaOcrExtractor {
    fn name(&self) -> &str {
        "OllamaOcrExtractor"
    }

    /// `image_bytes` must be image data (PNG/JPEG/GIF/WebP/etc.), not a PDF —
    /// see the module docs. Ollama accepts raw base64 (no `data:` URL
    /// prefix), unlike the OpenRouter vision path in
    /// [`crate::kb::file::image`].
    async fn extract(&self, image_bytes: &[u8]) -> KbResult<ExtractionResult> {
        let t0 = Instant::now();
        let b64 = B64.encode(image_bytes);
        let body = json!({
            "model": self.model,
            "prompt": OCR_PROMPT,
            "images": [b64],
            "stream": false,
        });

        let res = self
            .client
            .post(&self.base_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| KbError::Extraction {
                extractor: "OllamaOcrExtractor".into(),
                message: e.to_string(),
            })?;

        let status = res.status();
        if !status.is_success() {
            let body_text = res.text().await.unwrap_or_default();
            let truncated: String = body_text.chars().take(300).collect();
            return Err(KbError::Extraction {
                extractor: "OllamaOcrExtractor".into(),
                message: format!("ollama OCR endpoint {}: {}", status.as_u16(), truncated),
            });
        }

        let data: OllamaGenerateResponse = res.json().await.map_err(|e| KbError::Extraction {
            extractor: "OllamaOcrExtractor".into(),
            message: format!("response parse: {e}"),
        })?;

        Ok(ExtractionResult {
            text: data.response,
            elapsed_ms: elapsed_ms(t0),
            pages: Some(1),
            model: self.model.clone(),
            prompt_tokens: None,
            completion_tokens: None,
            cost_usd: None,
        })
    }
}
