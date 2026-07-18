//! Image file processor — sends the image to an OpenRouter vision LLM and
//! returns a structured description suitable for search/retrieval.
//!
//! Port of `processImage` in `file-processor.ts:152-239`. Unlike
//! [`crate::kb::extract::vision_llm`] (PDF → Markdown), this path sends a
//! plain `image_url` block with a `data:{mime};base64,…` URL, not a PDF
//! `file` block. The prompt is copied verbatim from the TS source.
//!
//! The Ollama OCR fallback present in the TS source (`use_ocr_pipeline`) is
//! intentionally not ported in Phase 6 — calling with that flag returns a
//! typed error so we never silently downgrade.
//!
//! SPIKE (`--features kb-ocr`, non-default): when that feature is on, the
//! flag routes through [`crate::kb::extract::ocr_ollama::OllamaOcrExtractor`]
//! instead of erroring — the "single image → OCR text" slice from
//! `docs/kb/ocr-design.md`. Without the feature, the error above still
//! applies unchanged.

use std::path::Path;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use tokio::time::sleep;

use crate::kb::file::ProcessingOptions;
use crate::kb::{KbConfig, KbError, KbResult};

/// Vision model — pinned to gpt-4o-mini per `file-processor.ts:11`. We
/// deliberately do NOT add a config knob here in Phase 6; if a knob is
/// requested later it should land in `KbConfig`, not be tunneled through
/// `ProcessingOptions`.
pub const VISION_MODEL: &str = "openai/gpt-4o-mini";

/// Verbatim port of the prompt from `file-processor.ts:206-211`. Load-bearing
/// for output quality on small models.
const IMAGE_PROMPT: &str =
    "Analyze this image and provide:\n1. A detailed description of what the image shows\n2. Any text visible in the image (OCR)\n3. Key information or data points visible\n\nFormat your response as structured text that can be used for search and retrieval. Be thorough but concise.";

const MAX_TOKENS: u32 = 1_500;

// Retry policy mirrors `embed/openrouter.rs` — 3 attempts, exponential
// backoff capped at 10s. Two callers isn't enough for a shared helper
// (rule-of-three), so we inline the loop and leave a TODO marker.
// TODO(refactor): if a third HTTP-retry caller lands, lift this loop into
// a shared `kb::http_retry` helper.
const MAX_RETRIES: u32 = 3;
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 10_000;

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChoiceMsg>,
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

/// Read `path`, base64-encode, POST to the vision endpoint, return
/// `"[Image: {filename}]\n\n{description}"`. The wrapper line preserves the
/// filename so chunk retrievers can still surface the source.
pub async fn process_image(
    cfg: &KbConfig,
    path: &Path,
    opts: &ProcessingOptions,
) -> KbResult<String> {
    if opts.use_ocr_pipeline {
        #[cfg(feature = "kb-ocr")]
        {
            return process_image_via_ocr(path).await;
        }
        #[cfg(not(feature = "kb-ocr"))]
        {
            // TODO(kb-ocr): same Ollama-OCR TODO as `process_pdf` in super.
            return Err(KbError::Other(
                "OCR pipeline not yet implemented; set use_ocr_pipeline=false".into(),
            ));
        }
    }

    let api_key = KbConfig::resolve_key(&cfg.extract_vision_api_key);
    if api_key.is_empty() {
        return Err(KbError::Config(
            "No API key configured: set KB_EXTRACT_VISION_API_KEY or OPENROUTER_API_KEY".into(),
        ));
    }

    let bytes = tokio::fs::read(path).await?;
    let mime = mime_for_path(path);
    let base64 = B64.encode(&bytes);
    let data_url = format!("data:{mime};base64,{base64}");

    let body = json!({
        "model": VISION_MODEL,
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": IMAGE_PROMPT },
                    {
                        "type": "image_url",
                        "image_url": { "url": data_url },
                    },
                ],
            }
        ],
        "max_tokens": MAX_TOKENS,
    });

    let client = reqwest::Client::new();
    let description =
        post_with_retry(&client, &cfg.extract_vision_base_url, &api_key, &body).await?;

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    Ok(format!("[Image: {filename}]\n\n{description}"))
}

/// SPIKE (`kb-ocr`): OCR a single image via a local/self-hosted Ollama vision
/// model instead of the OpenRouter vision-LLM path above. Prototype only —
/// see `docs/kb/ocr-design.md`. Config (endpoint/model) comes from
/// [`crate::kb::extract::ocr_ollama::OllamaOcrExtractor::from_env`], not
/// `KbConfig`, so this stays out of the non-OCR extract path entirely.
#[cfg(feature = "kb-ocr")]
async fn process_image_via_ocr(path: &Path) -> KbResult<String> {
    use crate::kb::extract::ocr_ollama::OllamaOcrExtractor;
    use crate::kb::extract::Extractor;

    let bytes = tokio::fs::read(path).await?;
    let extractor = OllamaOcrExtractor::from_env();
    let result = extractor.extract(&bytes).await?;

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    Ok(format!("[Image: {filename}]\n\n{}", result.text))
}

/// Map a path's extension to an HTTP `Content-Type`. Unknown extensions
/// default to `image/png`, matching `file-processor.ts:168`.
fn mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        _ => "image/png",
    }
}

async fn post_with_retry(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> KbResult<String> {
    let mut last_err: Option<KbError> = None;
    for attempt in 1..=MAX_RETRIES {
        let send_result = client
            .post(url)
            .bearer_auth(api_key)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await;

        let resp = match send_result {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(KbError::Http(e));
                if attempt < MAX_RETRIES {
                    sleep(backoff_for(attempt)).await;
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            if is_transient(status.as_u16()) && attempt < MAX_RETRIES {
                tracing::warn!(
                    target: "kb::file::image",
                    attempt,
                    max = MAX_RETRIES,
                    status = status.as_u16(),
                    "vision attempt failed, retrying",
                );
                sleep(backoff_for(attempt)).await;
                continue;
            }
            return Err(KbError::ChatApi {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let parsed: ChatResponse = resp.json().await?;
        let description = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message)
            .and_then(|m| m.content)
            .unwrap_or_default();
        return Ok(description);
    }
    Err(last_err.unwrap_or_else(|| KbError::Other("vision request failed".into())))
}

fn is_transient(status: u16) -> bool {
    status >= 500 || status == 429
}

fn backoff_for(attempt: u32) -> Duration {
    let ms = RETRY_BASE_MS
        .saturating_mul(1u64 << (attempt - 1))
        .min(RETRY_MAX_MS);
    Duration::from_millis(ms)
}
