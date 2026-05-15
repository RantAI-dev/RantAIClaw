//! `VisionLlmExtractor` — sends a base64-encoded PDF to an OpenRouter-compatible
//! chat-completions endpoint asking for clean Markdown.
//!
//! Port of `extractors/vision-llm-extractor.ts`. The prompt is the constant
//! [`EXTRACTION_PROMPT`] (verbatim copy from the TS source — it's load-bearing
//! for output quality, especially with table-heavy PDFs).
//!
//! For PDFs whose page count exceeds `segment_pages` the extractor splits the
//! source into N-page segments via [`crate::kb::extract::pdf_splitter`] and
//! extracts each in parallel up to `segment_concurrency`. Usage fields are
//! summed.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::kb::extract::pdf_splitter::{get_page_count, split_pdf_by_page_count};
use crate::kb::extract::{elapsed_ms, ExtractionResult, Extractor};
use crate::kb::{KbConfig, KbError, KbResult};

pub struct VisionLlmExtractor {
    name: String,
    model: String,
    cfg: KbConfig,
    max_tokens: u32,
    segment_pages: u32,
    segment_concurrency: usize,
    client: reqwest::Client,
}

/// Defaults mirror `vision-llm-extractor.ts`.
const DEFAULT_MAX_TOKENS: u32 = 16_000;
const DEFAULT_SEGMENT_PAGES: u32 = 25;
const DEFAULT_SEGMENT_CONCURRENCY: usize = 4;

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChoiceMsg>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ChoiceMsg {
    #[serde(default)]
    message: Option<MsgContent>,
}

#[derive(Debug, Deserialize)]
struct MsgContent {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u32>,
    #[serde(default)]
    completion_tokens: Option<u32>,
    #[serde(default)]
    cost: Option<f64>,
}

impl VisionLlmExtractor {
    pub fn new(cfg: KbConfig, model: String) -> Self {
        Self::with_options(
            cfg,
            model,
            DEFAULT_MAX_TOKENS,
            DEFAULT_SEGMENT_PAGES,
            DEFAULT_SEGMENT_CONCURRENCY,
        )
    }

    pub fn with_options(
        cfg: KbConfig,
        model: String,
        max_tokens: u32,
        segment_pages: u32,
        segment_concurrency: usize,
    ) -> Self {
        Self {
            name: model.clone(),
            model,
            cfg,
            max_tokens,
            segment_pages: segment_pages.max(1),
            segment_concurrency: segment_concurrency.max(1),
            client: reqwest::Client::new(),
        }
    }

    /// Override the HTTP client (used by wiremock tests).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    async fn extract_single_call(
        &self,
        pdf_bytes: &[u8],
        filename: &str,
        pages_hint: u32,
    ) -> KbResult<ExtractionResult> {
        let api_key = KbConfig::resolve_key(&self.cfg.extract_vision_api_key);
        if api_key.is_empty() {
            return Err(KbError::Extraction {
                extractor: self.model.clone(),
                message: "No API key configured: set KB_EXTRACT_VISION_API_KEY or \
                          OPENROUTER_API_KEY"
                    .into(),
            });
        }

        let base64 = B64.encode(pdf_bytes);
        let body = json!({
            "model": self.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "file",
                            "file": {
                                "filename": filename,
                                "file_data": format!("data:application/pdf;base64,{base64}"),
                            },
                        },
                        { "type": "text", "text": EXTRACTION_PROMPT },
                    ],
                },
            ],
            "max_tokens": self.max_tokens,
            "temperature": 0,
        });

        let t0 = Instant::now();
        let res = self
            .client
            .post(&self.cfg.extract_vision_base_url)
            .bearer_auth(&api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| KbError::Extraction {
                extractor: self.model.clone(),
                message: e.to_string(),
            })?;
        let took_ms = elapsed_ms(t0);

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(300).collect();
            return Err(KbError::Extraction {
                extractor: self.model.clone(),
                message: format!(
                    "VisionLlmExtractor {} {}: {}",
                    self.model,
                    status.as_u16(),
                    truncated
                ),
            });
        }

        let data: ChatResponse = res.json().await.map_err(|e| KbError::Extraction {
            extractor: self.model.clone(),
            message: format!("response parse: {e}"),
        })?;

        let text = data
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message)
            .and_then(|m| m.content)
            .unwrap_or_default();
        let usage = data.usage.unwrap_or_default();

        Ok(ExtractionResult {
            text,
            elapsed_ms: took_ms,
            pages: if pages_hint == 0 { None } else { Some(pages_hint) },
            model: self.model.clone(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cost_usd: usage.cost,
        })
    }
}

#[async_trait]
impl Extractor for VisionLlmExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        // Best-effort page count. If we can't parse the PDF we still try a
        // single-call extraction — the upstream model may handle it.
        let page_count = get_page_count(pdf_bytes).await.unwrap_or(0);

        if page_count == 0 || page_count <= self.segment_pages {
            return self
                .extract_single_call(pdf_bytes, "document.pdf", page_count)
                .await;
        }

        // Split + parallel extract.
        let segments = split_pdf_by_page_count(pdf_bytes, self.segment_pages).await?;
        let segment_count = segments.len();
        let t_start = Instant::now();

        let semaphore = Arc::new(Semaphore::new(self.segment_concurrency));
        let mut handles = Vec::with_capacity(segment_count);

        for (idx, seg_bytes) in segments.into_iter().enumerate() {
            let permit_sem = semaphore.clone();
            let filename = format!("document-segment-{}-of-{}.pdf", idx + 1, segment_count);
            // Each worker needs its own copies of inputs for 'static lifetime
            // on the spawned future. The reqwest client and config strings
            // are cheap to clone (Arc-backed internally).
            let worker_ext = WorkerCtx {
                client: self.client.clone(),
                api_key_override: self.cfg.extract_vision_api_key.clone(),
                base_url: self.cfg.extract_vision_base_url.clone(),
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                pages_hint: self.segment_pages,
            };
            let handle = tokio::spawn(async move {
                let _permit = permit_sem
                    .acquire_owned()
                    .await
                    .map_err(|_| KbError::Other("semaphore closed".into()))?;
                worker_ext
                    .run(&seg_bytes, &filename, idx + 1, segment_count)
                    .await
            });
            handles.push(handle);
        }

        let mut results: Vec<ExtractionResult> = Vec::with_capacity(segment_count);
        for h in handles {
            let r = h
                .await
                .map_err(|e| KbError::Other(format!("vision-llm worker join: {e}")))??;
            results.push(r);
        }

        let concat_text = results
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let total_prompt: u32 = results.iter().filter_map(|r| r.prompt_tokens).sum();
        let total_completion: u32 = results.iter().filter_map(|r| r.completion_tokens).sum();
        let total_cost: f64 = results.iter().filter_map(|r| r.cost_usd).sum();

        Ok(ExtractionResult {
            text: concat_text,
            elapsed_ms: elapsed_ms(t_start),
            pages: Some(page_count),
            model: format!(
                "{} ({} segments × {}pg)",
                self.model, segment_count, self.segment_pages
            ),
            prompt_tokens: if total_prompt == 0 {
                None
            } else {
                Some(total_prompt)
            },
            completion_tokens: if total_completion == 0 {
                None
            } else {
                Some(total_completion)
            },
            cost_usd: if total_cost == 0.0 {
                None
            } else {
                Some(total_cost)
            },
        })
    }
}

/// Per-segment worker — owns just enough state to issue one HTTP call without
/// borrowing back into the parent extractor.
struct WorkerCtx {
    client: reqwest::Client,
    api_key_override: String,
    base_url: String,
    model: String,
    max_tokens: u32,
    pages_hint: u32,
}

impl WorkerCtx {
    async fn run(
        &self,
        pdf_bytes: &[u8],
        filename: &str,
        segment_idx: usize,
        segment_total: usize,
    ) -> KbResult<ExtractionResult> {
        let api_key = KbConfig::resolve_key(&self.api_key_override);
        if api_key.is_empty() {
            return Err(KbError::Extraction {
                extractor: self.model.clone(),
                message: "No API key configured: set KB_EXTRACT_VISION_API_KEY or \
                          OPENROUTER_API_KEY"
                    .into(),
            });
        }
        let base64 = B64.encode(pdf_bytes);
        let body = json!({
            "model": self.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "file",
                            "file": {
                                "filename": filename,
                                "file_data": format!("data:application/pdf;base64,{base64}"),
                            },
                        },
                        { "type": "text", "text": EXTRACTION_PROMPT },
                    ],
                },
            ],
            "max_tokens": self.max_tokens,
            "temperature": 0,
        });

        let t0 = Instant::now();
        let res = self
            .client
            .post(&self.base_url)
            .bearer_auth(&api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| KbError::Extraction {
                extractor: self.model.clone(),
                message: format!(
                    "[VisionLlmExtractor segment {segment_idx}/{segment_total}] {e}"
                ),
            })?;
        let took_ms = elapsed_ms(t0);

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            let truncated: String = body.chars().take(300).collect();
            return Err(KbError::Extraction {
                extractor: self.model.clone(),
                message: format!(
                    "[VisionLlmExtractor segment {}/{}] {} {}: {}",
                    segment_idx,
                    segment_total,
                    self.model,
                    status.as_u16(),
                    truncated
                ),
            });
        }

        let data: ChatResponse = res.json().await.map_err(|e| KbError::Extraction {
            extractor: self.model.clone(),
            message: format!(
                "[VisionLlmExtractor segment {segment_idx}/{segment_total}] parse: {e}"
            ),
        })?;
        let text = data
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message)
            .and_then(|m| m.content)
            .unwrap_or_default();
        let usage = data.usage.unwrap_or_default();

        Ok(ExtractionResult {
            text,
            elapsed_ms: took_ms,
            pages: if self.pages_hint == 0 {
                None
            } else {
                Some(self.pages_hint)
            },
            model: self.model.clone(),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cost_usd: usage.cost,
        })
    }
}

/// Verbatim copy of `EXTRACTION_PROMPT` from
/// `src/lib/rag/extractors/vision-llm-extractor.ts:153-162`. Load-bearing
/// for output quality — do not edit without re-running the SOTA bench.
pub const EXTRACTION_PROMPT: &str = "Extract the full textual content of this PDF as clean, COMPACT Markdown.

Strict rules:
- Headings: # / ## / ### matching document hierarchy
- Lists: \"- \" or \"1. \" with ONE space
- Tables: standard Markdown pipes with a single space of padding on each side of cell content. DO NOT pad cells with many spaces or align columns — compact tables only.
- Inline math: $...$ ; block math: $$...$$
- Code blocks: fenced with triple backticks

Do not summarize. Do not omit content. Do not add commentary. Output ONLY the extracted Markdown.";
