//! `MineruExtractor` shell — real implementation lands in task 5.5.

use async_trait::async_trait;

use crate::kb::extract::{ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

pub struct MineruExtractor {
    base_url: String,
}

impl MineruExtractor {
    pub fn new(base_url: String) -> KbResult<Self> {
        if base_url.is_empty() {
            return Err(KbError::Config(
                "MineruExtractor requires a base URL".into(),
            ));
        }
        Ok(Self { base_url })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[async_trait]
impl Extractor for MineruExtractor {
    fn name(&self) -> &str {
        "MineruExtractor"
    }

    async fn extract(&self, _pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        Err(KbError::Other(
            "MineruExtractor not implemented yet (task 5.5)".into(),
        ))
    }
}
