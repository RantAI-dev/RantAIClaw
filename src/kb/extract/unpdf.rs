//! `UnpdfExtractor` shell — real implementation lands in task 5.2.

use async_trait::async_trait;

use crate::kb::extract::{ExtractionResult, Extractor};
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

    async fn extract(&self, _pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        Err(KbError::Other(
            "UnpdfExtractor not implemented yet (task 5.2)".into(),
        ))
    }
}
