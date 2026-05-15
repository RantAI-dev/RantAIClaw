//! `VisionLlmExtractor` shell — real implementation lands in task 5.4.

use async_trait::async_trait;

use crate::kb::extract::{ExtractionResult, Extractor};
use crate::kb::{KbConfig, KbError, KbResult};

pub struct VisionLlmExtractor {
    name: String,
    #[allow(dead_code)]
    cfg: KbConfig,
    #[allow(dead_code)]
    model: String,
}

impl VisionLlmExtractor {
    pub fn new(cfg: KbConfig, model: String) -> Self {
        Self {
            name: model.clone(),
            cfg,
            model,
        }
    }
}

#[async_trait]
impl Extractor for VisionLlmExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, _pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        Err(KbError::Other(
            "VisionLlmExtractor not implemented yet (task 5.4)".into(),
        ))
    }
}
