//! KB document extractor pipeline.
//!
//! Mirrors `src/lib/rag/extractors/` from the Node platform:
//!
//! - [`Extractor`] is the narrow async trait every backend implements.
//! - [`ExtractionResult`] is the uniform return type (text + bookkeeping).
//! - [`build_extractor`] dispatches a config sentinel
//!   (`"unpdf" | "mineru" | "hybrid" | "smart" | <openrouter-model-id>`) to a
//!   concrete extractor. Port of `extractors/index.ts:21-43`.
//! - [`extract_with_fallback`] runs `primary`, falling back to `fallback` if
//!   the primary errors. Port of `extractors/index.ts:58-71`.
//!
//! Strictly isolated from [`crate::memory`]: KB documents are org-level, the
//! agent's short-term memory is unrelated and lives in `memory/`.

use async_trait::async_trait;

use crate::kb::{KbConfig, KbError, KbResult};

pub mod hybrid;
pub mod mineru;
pub mod pdf_splitter;
pub mod smart_router;
pub mod text_layer_signals;
pub mod unpdf;
pub mod vision_llm;

/// Saturating-cast `Duration::as_millis()` (`u128`) to `u64`. PDF extractions
/// never take >584 million years; the saturate keeps clippy's pedantic
/// truncation lint quiet without sacrificing observability.
#[inline]
pub(crate) fn elapsed_ms(t0: std::time::Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Output of any [`Extractor`]. Token / cost fields are summed when the
/// extractor splits work across multiple model calls (see
/// [`vision_llm::VisionLlmExtractor`]).
#[derive(Debug, Clone, Default)]
pub struct ExtractionResult {
    pub text: String,
    pub elapsed_ms: u64,
    pub pages: Option<u32>,
    pub model: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
}

/// Backend abstraction. Implementations must be cheap to `Arc<dyn>`-share and
/// safe to call concurrently from many tasks (the trait inherits `Send + Sync`).
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Stable identifier — used in logs, error messages, and the
    /// `ExtractionResult.model` field of higher-level wrappers.
    fn name(&self) -> &str;

    /// Run extraction against an in-memory PDF.
    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult>;
}

/// Build the configured extractor from a sentinel string.
///
/// Port of `extractors/index.ts:21-43`. Sentinels:
/// - `"unpdf"`       — text-layer only via the `pdf-extract` crate
/// - `"mineru"`      — POSTs to a MinerU2.5 sidecar
///   (requires `cfg.extract_mineru_base_url`)
/// - `"hybrid"`      — MinerU + unpdf in parallel + merge
/// - `"smart"`       — unpdf → text-layer heuristics → fallback
///   (requires `cfg.extract_smart_fallback` to NOT be `"smart"`)
/// - anything else   — treated as an OpenRouter model id and dispatched to
///   [`vision_llm::VisionLlmExtractor`]
pub fn build_extractor(cfg: &KbConfig, sentinel: &str) -> KbResult<Box<dyn Extractor>> {
    match sentinel {
        "unpdf" => Ok(Box::new(unpdf::UnpdfExtractor::new())),
        "mineru" => {
            if cfg.extract_mineru_base_url.is_empty() {
                return Err(KbError::Config(
                    "KB_EXTRACT_MINERU_BASE_URL required for the 'mineru' extractor".into(),
                ));
            }
            Ok(Box::new(mineru::MineruExtractor::new(
                cfg.extract_mineru_base_url.clone(),
            )?))
        }
        "hybrid" => {
            if cfg.extract_mineru_base_url.is_empty() {
                return Err(KbError::Config(
                    "KB_EXTRACT_MINERU_BASE_URL required for the 'hybrid' extractor".into(),
                ));
            }
            let mineru = mineru::MineruExtractor::new(cfg.extract_mineru_base_url.clone())?;
            let unpdf_ext = unpdf::UnpdfExtractor::new();
            Ok(Box::new(hybrid::HybridExtractor::new(
                Box::new(mineru),
                Box::new(unpdf_ext),
            )))
        }
        "smart" => {
            if cfg.extract_smart_fallback == "smart" {
                return Err(KbError::Config(
                    "KB_EXTRACT_SMART_FALLBACK cannot be 'smart' — that would recurse infinitely. \
                     Set it to a terminal extractor sentinel (\"unpdf\", \"mineru\", \"hybrid\") or \
                     an OpenRouter model id (e.g. \"openai/gpt-4.1-nano\")."
                        .into(),
                ));
            }
            let fallback = build_extractor(cfg, &cfg.extract_smart_fallback)?;
            Ok(Box::new(smart_router::SmartRouterExtractor::new(
                Box::new(unpdf::UnpdfExtractor::new()),
                fallback,
            )))
        }
        model_id => Ok(Box::new(vision_llm::VisionLlmExtractor::new(
            cfg.clone(),
            model_id.to_string(),
        ))),
    }
}

/// Run `primary.extract`; if it returns `Err`, log + retry with `fallback`.
/// Errors out only when both backends fail.
///
/// Port of `extractors/index.ts:58-71`.
pub async fn extract_with_fallback(
    bytes: &[u8],
    primary: &dyn Extractor,
    fallback: &dyn Extractor,
) -> KbResult<ExtractionResult> {
    match primary.extract(bytes).await {
        Ok(r) => Ok(r),
        Err(e) => {
            let msg = e.to_string();
            let truncated: String = msg.chars().take(120).collect();
            tracing::warn!(
                primary = primary.name(),
                fallback = fallback.name(),
                "primary extractor failed ({}); falling back",
                truncated
            );
            fallback.extract(bytes).await
        }
    }
}
