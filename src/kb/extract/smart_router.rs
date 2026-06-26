//! `SmartRouterExtractor` — text-layer first, OCR fallback when heuristics
//! flag the text-layer output as insufficient.
//!
//! Port of `extractors/smart-router-extractor.ts`. See
//! `text_layer_signals` for the actual heuristics.

use async_trait::async_trait;

use crate::kb::extract::text_layer_signals::{is_unpdf_sufficient_with_size, RouterOpts};
use crate::kb::extract::{ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

pub struct SmartRouterExtractor {
    name: String,
    text_layer: Box<dyn Extractor>,
    fallback: Box<dyn Extractor>,
    opts: RouterOpts,
}

impl SmartRouterExtractor {
    pub fn new(text_layer: Box<dyn Extractor>, fallback: Box<dyn Extractor>) -> Self {
        Self::with_opts(text_layer, fallback, RouterOpts::default())
    }

    pub fn with_opts(
        text_layer: Box<dyn Extractor>,
        fallback: Box<dyn Extractor>,
        opts: RouterOpts,
    ) -> Self {
        let name = format!("SmartRouter({}+{})", text_layer.name(), fallback.name());
        Self {
            name,
            text_layer,
            fallback,
            opts,
        }
    }
}

#[async_trait]
impl Extractor for SmartRouterExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        let mut text_layer_err: Option<String> = None;
        let mut text_layer_result: Option<ExtractionResult> = None;

        match self.text_layer.extract(pdf_bytes).await {
            Ok(r) => text_layer_result = Some(r),
            Err(e) => {
                let msg = e.to_string();
                let truncated: String = msg.chars().take(100).collect();
                tracing::warn!(
                    text_layer = self.text_layer.name(),
                    fallback = self.fallback.name(),
                    "smart-router text-layer extractor threw ({}); falling through",
                    truncated
                );
                text_layer_err = Some(msg);
            }
        }

        if let Some(ref tl) = text_layer_result {
            let pages = tl.pages.unwrap_or(1);
            if is_unpdf_sufficient_with_size(&tl.text, pages, pdf_bytes.len(), &self.opts) {
                let inner_model = if tl.model.is_empty() {
                    self.text_layer.name().to_string()
                } else {
                    tl.model.clone()
                };
                return Ok(ExtractionResult {
                    model: format!("smart({inner_model})"),
                    ..tl.clone()
                });
            }
        }

        match self.fallback.extract(pdf_bytes).await {
            Ok(fb) => {
                let inner_model = if fb.model.is_empty() {
                    self.fallback.name().to_string()
                } else {
                    fb.model.clone()
                };
                Ok(ExtractionResult {
                    model: format!("smart(fallback:{inner_model})"),
                    ..fb
                })
            }
            Err(e) => {
                let fb_msg = e.to_string();
                let fb_short: String = fb_msg.chars().take(150).collect();
                let tl_msg = text_layer_err.unwrap_or_else(|| "insufficient output".into());
                let tl_short: String = tl_msg.chars().take(150).collect();
                Err(KbError::Extraction {
                    extractor: self.name.clone(),
                    message: format!(
                        "Both extractors failed — textLayer({}): {}; fallback({}): {}",
                        self.text_layer.name(),
                        tl_short,
                        self.fallback.name(),
                        fb_short
                    ),
                })
            }
        }
    }
}
