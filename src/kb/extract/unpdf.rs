//! `UnpdfExtractor` — text-layer extraction via the `pdf-extract` crate.
//!
//! Port of `extractors/unpdf-extractor.ts`. Wraps the synchronous
//! `pdf_extract::extract_text_from_mem` in `tokio::task::spawn_blocking` to
//! keep CPU-bound parsing off the async executor.
//!
//! Produces flat text with no layout preservation (no headings, no tables,
//! no paragraph breaks). Kept as an opt-in escape hatch via
//! `KB_EXTRACT_PRIMARY=unpdf`; for retrieval quality `vision_llm` or `hybrid`
//! are usually better.

use std::time::Instant;

use async_trait::async_trait;

use crate::kb::extract::{elapsed_ms, ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

#[derive(Debug, Default)]
pub struct UnpdfExtractor;

impl UnpdfExtractor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Extractor for UnpdfExtractor {
    fn name(&self) -> &str {
        "unpdf"
    }

    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        let t0 = Instant::now();
        // pdf_extract is sync and CPU-bound; offload to the blocking pool.
        let bytes = pdf_bytes.to_vec();
        let text = tokio::task::spawn_blocking(move || {
            pdf_extract::extract_text_from_mem(&bytes).map_err(|e| KbError::Extraction {
                extractor: "unpdf".into(),
                message: e.to_string(),
            })
        })
        .await
        .map_err(|e| KbError::Other(format!("unpdf join error: {e}")))??;

        Ok(ExtractionResult {
            text,
            elapsed_ms: elapsed_ms(t0),
            pages: None,
            model: "unpdf".into(),
            prompt_tokens: None,
            completion_tokens: None,
            cost_usd: None,
        })
    }
}
