//! `SmartRouterExtractor` shell — real implementation lands in task 5.7.

use async_trait::async_trait;

use crate::kb::extract::{ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

pub struct SmartRouterExtractor {
    name: String,
    #[allow(dead_code)]
    text_layer: Box<dyn Extractor>,
    #[allow(dead_code)]
    fallback: Box<dyn Extractor>,
}

impl SmartRouterExtractor {
    pub fn new(text_layer: Box<dyn Extractor>, fallback: Box<dyn Extractor>) -> Self {
        let name = format!("SmartRouter({}+{})", text_layer.name(), fallback.name());
        Self {
            name,
            text_layer,
            fallback,
        }
    }
}

#[async_trait]
impl Extractor for SmartRouterExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, _pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        Err(KbError::Other(
            "SmartRouterExtractor not implemented yet (task 5.7)".into(),
        ))
    }
}
