//! Opt-in contextual retrieval — port of `src/lib/rag/contextual-retrieval.ts`.
//!
//! One OpenRouter chat call per document with the full doc + chunk list,
//! producing N short context-prefix strings (one per chunk). The full-doc
//! content block carries `cache_control: {type:"ephemeral"}` so subsequent
//! chunks-of-the-same-doc calls hit Anthropic-style prompt caching.
//!
//! Returns `vec![""; chunks.len()]` on every failure path — disabled, missing
//! API key, network error, parse error, length mismatch. Never throws.

use std::time::Duration;

use crate::kb::KbConfig;

const OPENROUTER_URL_DEFAULT: &str = "https://openrouter.ai/api/v1/chat/completions";
const TIMEOUT: Duration = Duration::from_secs(30);

const PROMPT_HEADER: &str = "You are helping index a knowledge base. Given the full document below (cached) and a list of chunks from it, generate a short 1-sentence context for each chunk. The context should describe what the chunk is about *in relation to the full document* — what section it belongs to, what it continues from, or what key entity it describes. This helps downstream retrieval resolve ambiguous chunks.\n\nOutput EXACTLY a JSON array of strings — one string per chunk, same order as input. No prose, no markdown fences.";

fn openrouter_url() -> String {
    std::env::var("KB_OPENROUTER_CHAT_URL")
        .unwrap_or_else(|_| OPENROUTER_URL_DEFAULT.to_string())
}

/// Generate one contextual prefix per chunk.
///
/// Returns `vec![""; chunks.len()]` when:
/// - `cfg.contextual_retrieval_enabled` is false
/// - `OPENROUTER_API_KEY` is unset
/// - HTTP call fails or returns non-2xx
/// - response can't be parsed as a JSON array
/// - parsed array length doesn't match `chunks.len()`
///
/// Never throws — caller treats empty prefixes as a no-op (chunks indexed
/// without contextual context). The 800-char per-chunk truncation matches
/// the TS source: contextual generation is for indexing-time enrichment,
/// not deep semantic analysis.
pub async fn generate_contextual_prefixes(
    cfg: &KbConfig,
    full_document: &str,
    chunks: &[String],
) -> Vec<String> {
    let empty = || chunks.iter().map(|_| String::new()).collect::<Vec<_>>();

    if !cfg.contextual_retrieval_enabled {
        return empty();
    }
    if chunks.is_empty() {
        return Vec::new();
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        return empty();
    }

    // Compose chunk block: `[i] <first 800 chars of chunk, newlines→spaces>`.
    let chunk_block = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let trimmed: String = c.chars().take(800).collect::<String>().replace('\n', " ");
            format!("[{i}] {trimmed}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let body = serde_json::json!({
        "model": cfg.contextual_retrieval_model,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": format!("FULL DOCUMENT:\n{full_document}"),
                    "cache_control": { "type": "ephemeral" }
                },
                {
                    "type": "text",
                    "text": format!(
                        "{PROMPT_HEADER}\n\nCHUNKS:\n{chunk_block}\n\nRespond with a JSON array of {} strings:",
                        chunks.len()
                    )
                }
            ]
        }],
        "max_tokens": 1500,
        "temperature": 0,
    });

    match fetch_prefixes(&api_key, &body, chunks.len()).await {
        Ok(prefixes) => prefixes,
        Err(e) => {
            tracing::warn!(
                target: "kb::retrieve::contextual",
                error = %e,
                "contextual retrieval failed; chunks indexed without context",
            );
            empty()
        }
    }
}

/// POST and parse. Returns `Err` on any failure — caller maps that to empty
/// prefixes. The "parsed array length must equal chunks.len()" check is
/// inside this function: a 5-element response for 3 chunks is treated as a
/// parse failure, not silently truncated.
async fn fetch_prefixes(
    api_key: &str,
    body: &serde_json::Value,
    expected_len: usize,
) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let resp = client
        .post(openrouter_url())
        .bearer_auth(api_key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("CR {}", resp.status().as_u16()));
    }
    let data: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    let raw = data
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let start = raw.find('[').ok_or("no JSON array in response")?;
    let end = raw.rfind(']').ok_or("no closing bracket")?;
    if end < start {
        return Err("malformed array bounds".into());
    }
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&raw[start..=end])
        .map_err(|e| format!("parse: {e}"))?;
    if parsed.len() != expected_len {
        return Err(format!(
            "length mismatch: got {} prefixes, expected {expected_len}",
            parsed.len()
        ));
    }
    Ok(parsed
        .into_iter()
        .map(|v| match v.as_str() {
            Some(s) => s.trim().to_string(),
            None => String::new(),
        })
        .collect())
}
