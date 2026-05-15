//! `HybridExtractor` shell — real implementation lands in task 5.7.

use async_trait::async_trait;

use crate::kb::extract::{ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

pub struct HybridExtractor {
    name: String,
    #[allow(dead_code)]
    structural: Box<dyn Extractor>,
    #[allow(dead_code)]
    text_layer: Box<dyn Extractor>,
}

impl HybridExtractor {
    pub fn new(structural: Box<dyn Extractor>, text_layer: Box<dyn Extractor>) -> Self {
        let name = format!("Hybrid({}+{})", structural.name(), text_layer.name());
        Self {
            name,
            structural,
            text_layer,
        }
    }
}

#[async_trait]
impl Extractor for HybridExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, _pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        Err(KbError::Other(
            "HybridExtractor not implemented yet (task 5.7)".into(),
        ))
    }
}
