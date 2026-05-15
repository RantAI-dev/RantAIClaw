//! LLM-as-reranker — Rust port of `src/lib/rag/rerankers/llm-reranker.ts`.
//!
//! Sends a numbered list of candidate passages to the configured chat model
//! and parses back a JSON array of indices. Deterministic (temperature=0).
//! If the model returns fewer than `final_k` usable indices, the result is
//! filled from the original rank order so callers always get exactly
//! `final_k` items (or every candidate when fewer were supplied).

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::kb::rerank::{fill_remaining_in_order, Candidate, Reranked, Reranker};
use crate::kb::{KbError, KbResult};

const CANDIDATE_TEXT_LIMIT: usize = 400;
const MAX_TOKENS: u32 = 200;

/// Matches the JSON array of integers in the chat completion response.
/// Identical pattern to the TS regex `/\[[\d,\s]+\]/`.
static INDEX_ARRAY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[\d,\s]+\]").expect("static regex compiles"));

/// LLM-as-reranker provider. `chat_url` is taken from
/// `KbConfig::openrouter_chat_url` by `make_reranker`; tests + on-prem
/// deployments override the endpoint.
pub struct LlmReranker {
    model: String,
    chat_url: String,
    http: Client,
}

impl LlmReranker {
    pub fn new(model: String, chat_url: String) -> Self {
        Self {
            model,
            chat_url,
            http: Client::new(),
        }
    }
}

#[async_trait]
impl Reranker for LlmReranker {
    fn name(&self) -> &str {
        &self.model
    }

    async fn rerank(
        &self,
        query: &str,
        candidates: &[Candidate],
        final_k: usize,
    ) -> KbResult<Vec<Reranked>> {
        // Fail fast on missing key — mirrors `llm-reranker.ts:31`.
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| KbError::Config("OPENROUTER_API_KEY is not set".into()))?;

        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // Only short-circuit when strictly fewer candidates than requested —
        // we still want to reorder when counts are equal. Mirrors TS line 36.
        if candidates.len() < final_k {
            return Ok(candidates
                .iter()
                .enumerate()
                .map(|(i, c)| Reranked {
                    id: c.id.clone(),
                    final_rank: i,
                    #[allow(clippy::cast_precision_loss)]
                    score: (final_k - i) as f32,
                })
                .collect());
        }

        let numbered = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let truncated = char_truncate(&c.text, CANDIDATE_TEXT_LIMIT);
                format!("[{i}] {}", truncated.replace('\n', " "))
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        // Prompt VERBATIM from `llm-reranker.ts:46-53` — load-bearing.
        let prompt = format!(
            "You are a retrieval reranker. Given a query and candidate passages, output the indices of the top {final_k} most relevant passages in descending order of relevance, as a JSON array of integers. Output ONLY the JSON array.\n\nQuery: {query}\n\nPassages:\n{numbered}\n\nTop {final_k} indices:"
        );

        let body = serde_json::json!({
            "model": &self.model,
            "messages": [{ "role": "user", "content": prompt }],
            "max_tokens": MAX_TOKENS,
            "temperature": 0,
        });

        let resp = self
            .http
            .post(&self.chat_url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(KbError::ChatApi {
                status: status.as_u16(),
                body: truncate(&text, 300),
            });
        }

        let parsed: Value = resp.json().await?;
        let raw = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let picked_indices: Vec<usize> = INDEX_ARRAY_RE
            .find(raw)
            .and_then(|m| serde_json::from_str::<Vec<i64>>(m.as_str()).ok())
            .map(|v| {
                v.into_iter()
                    .filter_map(|n| usize::try_from(n).ok())
                    .collect()
            })
            .unwrap_or_default();

        // LLM provides ordering signal only — score is the inverted rank so
        // downstream consumers see `final_k - rank` like the TS source.
        let picked: Vec<(usize, f32)> = picked_indices
            .into_iter()
            .enumerate()
            .map(|(rank, idx)| {
                #[allow(clippy::cast_precision_loss)]
                let score = final_k.saturating_sub(rank) as f32;
                (idx, score)
            })
            .collect();

        Ok(fill_remaining_in_order(
            candidates,
            &picked,
            final_k,
            #[allow(clippy::cast_precision_loss)]
            |rank, _cand| final_k.saturating_sub(rank) as f32,
        ))
    }
}

/// Char-safe slice — never panics on a multibyte boundary the way
/// `&s[..max]` does. Behaves like the TS `s.slice(0, max)` after `replace`.
fn char_truncate(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        return s;
    }
    let cut = s
        .char_indices()
        .take(max_chars)
        .last()
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    &s[..cut]
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let cut = s
            .char_indices()
            .take(max)
            .last()
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        format!("{}…", &s[..cut])
    }
}
