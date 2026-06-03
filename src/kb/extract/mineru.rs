//! `MineruExtractor` — HTTP client for the MinerU2.5-Pro sidecar
//! (`services/mineru-server/server.py` in the Node platform).
//!
//! Port of `extractors/mineru-extractor.ts`. Posts the PDF as multipart form
//! data; receives Markdown back.

use std::time::Instant;

use async_trait::async_trait;
use reqwest::multipart;
use serde::Deserialize;

use crate::kb::extract::{elapsed_ms, ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

#[derive(Debug)]
pub struct MineruExtractor {
    base_url: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct MineruResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    ms: Option<u64>,
    #[serde(default)]
    pages: Option<u32>,
}

impl MineruExtractor {
    /// Build a MinerU client. Errors if `base_url` is empty. Trailing slashes
    /// and a trailing `/extract` are stripped so callers can pass either
    /// `http://host:8100` or `http://host:8100/extract`.
    pub fn new(base_url: String) -> KbResult<Self> {
        if base_url.is_empty() {
            return Err(KbError::Config(
                "MineruExtractor requires a base URL — set KB_EXTRACT_MINERU_BASE_URL \
                 (e.g. http://localhost:8100)"
                    .into(),
            ));
        }
        let normalized = base_url
            .trim_end_matches('/')
            .trim_end_matches("/extract")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            base_url: normalized,
            client: reqwest::Client::new(),
        })
    }

    /// Allow injecting a custom client for tests.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }
}

#[async_trait]
impl Extractor for MineruExtractor {
    fn name(&self) -> &str {
        "MineruExtractor"
    }

    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        let t0 = Instant::now();
        let part = multipart::Part::bytes(pdf_bytes.to_vec())
            .file_name("document.pdf")
            .mime_str("application/pdf")
            .map_err(|e| KbError::Extraction {
                extractor: "MineruExtractor".into(),
                message: format!("invalid mime: {e}"),
            })?;
        let form = multipart::Form::new().part("file", part);

        let url = format!("{}/extract", self.base_url);
        let res = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| KbError::Extraction {
                extractor: "MineruExtractor".into(),
                message: e.to_string(),
            })?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(300).collect();
            return Err(KbError::Extraction {
                extractor: "MineruExtractor".into(),
                message: format!("mineru sidecar {}: {}", status.as_u16(), truncated),
            });
        }

        let data: MineruResponse = res.json().await.map_err(|e| KbError::Extraction {
            extractor: "MineruExtractor".into(),
            message: format!("response parse: {e}"),
        })?;

        let took_ms = data.ms.unwrap_or_else(|| elapsed_ms(t0));
        Ok(ExtractionResult {
            text: data.text,
            elapsed_ms: took_ms,
            pages: data.pages,
            model: "mineru-2.5-pro".into(),
            prompt_tokens: None,
            completion_tokens: None,
            cost_usd: None,
        })
    }
}
