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

pub mod child;
pub mod child_process;
pub mod hybrid;
pub mod inbound;
pub mod mineru;
#[cfg(feature = "kb-ocr")]
pub mod ocr_ollama;
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
        model_id => {
            // Every OpenRouter model id contains a '/'. A bare word here is
            // a typo'd sentinel (e.g. `unpfd`), which used to fall through
            // to the vision extractor and surface later as a runtime API
            // error instead of a config error (plan 113).
            if !model_id.contains('/') {
                return Err(KbError::Config(format!(
                    "unknown extractor '{model_id}': expected one of unpdf | mineru | hybrid | smart, or an OpenRouter model id (e.g. `openai/gpt-4.1-nano`)"
                )));
            }
            Ok(Box::new(vision_llm::VisionLlmExtractor::new(
                cfg.clone(),
                model_id.to_string(),
            )))
        }
    }
}

/// Run `primary.extract`; if it returns `Err`, log + retry with `fallback`.
/// Errors out only when both backends fail, and then with both causes, so an
/// upstream outage on the primary is not hidden behind a document error from
/// the fallback.
///
/// Port of `extractors/index.ts:58-71`.
pub async fn extract_with_fallback(
    bytes: &[u8],
    primary: &dyn Extractor,
    fallback: &dyn Extractor,
) -> KbResult<ExtractionResult> {
    let primary_err = match primary.extract(bytes).await {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };
    let msg = primary_err.to_string();
    let truncated: String = msg.chars().take(120).collect();
    tracing::warn!(
        primary = primary.name(),
        fallback = fallback.name(),
        "primary extractor failed ({}); falling back",
        truncated
    );
    match fallback.extract(bytes).await {
        Ok(r) => Ok(r),
        Err(fallback_err) => {
            let p_msg: String = primary_err.to_string().chars().take(150).collect();
            let f_msg: String = fallback_err.to_string().chars().take(150).collect();
            let p_names = failing_service_names(std::slice::from_ref(&primary_err));
            let f_names = failing_service_names(std::slice::from_ref(&fallback_err));
            let causes = vec![primary_err, fallback_err];
            Err(KbError::extraction_failed(
                failing_service_names(&causes),
                format!(
                    "Both extractors failed: primary({p_names}): {p_msg}; fallback({f_names}): {f_msg}"
                ),
                causes,
            ))
        }
    }
}

/// The plain, user-facing names of the extractors that failed, read back from
/// the causes' own `extractor` fields instead of a composite's internal
/// `name()` label: when an upstream service was unreachable, the services
/// that were; otherwise the extractors that rejected the document. A leaf
/// names its service and a composite carries the names of what failed inside
/// it, so a nested chain never leaks a `Fallback(...)`-style label into
/// user-facing text.
pub(crate) fn failing_service_names(causes: &[KbError]) -> String {
    let upstream = causes
        .iter()
        .any(|c| matches!(c, KbError::ExtractorUnavailable { .. }));
    let mut names: Vec<&str> = Vec::new();
    for cause in causes {
        let name = match cause {
            KbError::ExtractorUnavailable { extractor, .. } if upstream => extractor,
            KbError::Extraction { extractor, .. } if !upstream => extractor,
            _ => continue,
        };
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return "unknown".to_string();
    }
    names.join(" and ")
}

#[cfg(test)]
mod tests {
    use super::failing_service_names;
    use crate::kb::KbError;

    fn unavailable(extractor: &str) -> KbError {
        KbError::ExtractorUnavailable {
            extractor: extractor.to_string(),
            message: "unreachable".into(),
        }
    }

    fn bad_document(extractor: &str) -> KbError {
        KbError::Extraction {
            extractor: extractor.to_string(),
            message: "corrupt".into(),
        }
    }

    #[test]
    fn failing_service_names_deduplicates_the_same_service() {
        let causes = [unavailable("MinerU service"), unavailable("MinerU service")];
        assert_eq!(failing_service_names(&causes), "MinerU service");
    }

    #[test]
    fn failing_service_names_joins_distinct_services_with_and() {
        let causes = [unavailable("MinerU service"), unavailable("OCR service")];
        assert_eq!(
            failing_service_names(&causes),
            "MinerU service and OCR service"
        );
    }

    #[test]
    fn failing_service_names_names_only_the_unreachable_services_when_one_is_down() {
        let causes = [bad_document("unpdf"), unavailable("MinerU service")];
        assert_eq!(failing_service_names(&causes), "MinerU service");
    }

    #[test]
    fn failing_service_names_names_the_document_extractors_when_no_upstream_failed() {
        let causes = [bad_document("unpdf"), bad_document("unpdf")];
        assert_eq!(failing_service_names(&causes), "unpdf");
    }
}
