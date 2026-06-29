//! LLM-based combined entity + relation extractor.
//!
//! Sends one POST per chunk to the configured chat endpoint (mirrors the
//! `rerank/llm.rs` HTTP pattern). The model is instructed to return a strict
//! JSON object with `entities` and `relations` arrays. Bad chunks are skipped
//! with a `tracing::warn!` so a single flaky response never fails the whole
//! batch.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::kb::intelligence::extract::{EntityRelationExtractor, Extracted};
use crate::kb::intelligence::types::{EntityType, RelationType};
use crate::kb::KbResult;

/// Combined LLM extractor: one POST per chunk, results accumulated across all chunks.
pub struct CombinedLlmExtractor {
    model: String,
    chat_url: String,
    api_key: String,
    client: Client,
}

impl CombinedLlmExtractor {
    pub fn new(model: String, chat_url: String, api_key: String) -> Self {
        Self {
            model,
            chat_url,
            api_key,
            client: Client::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Serde structs for the chat completion response
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: String,
}

// ---------------------------------------------------------------------------
// Serde structs for the extraction payload
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ExtractionPayload {
    #[serde(default)]
    entities: Vec<RawEntity>,
    #[serde(default)]
    relations: Vec<RawRelation>,
}

#[derive(Deserialize)]
struct RawEntity {
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

#[derive(Deserialize)]
struct RawRelation {
    source: String,
    target: String,
    #[serde(rename = "type")]
    relation_type: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

fn default_confidence() -> f32 {
    1.0
}

// ---------------------------------------------------------------------------
// Prompt builder
// ---------------------------------------------------------------------------

fn build_prompt(chunk: &str) -> String {
    format!(
        "You are an entity and relation extractor. Given the text below, extract all entities \
and relations. Output ONLY a JSON object with exactly this structure:\n\
{{\"entities\":[{{\"name\":\"...\",\"type\":\"...\",\"confidence\":0.0}}],\
\"relations\":[{{\"source\":\"...\",\"target\":\"...\",\"type\":\"...\",\"confidence\":0.0}}]}}\n\n\
Valid entity types: Person, Organization, Location, Product, Technology, Concept, Event, \
Date, Email, Url, Phone, Money, Function, Api, Error, File.\n\
Valid relation types: WorksFor, PartOf, LocatedIn, Implements, Calls, DependsOn, Uses, \
Produces, RelatedTo.\n\n\
Text:\n{chunk}"
    )
}

// ---------------------------------------------------------------------------
// Trait impl
// ---------------------------------------------------------------------------

#[async_trait]
impl EntityRelationExtractor for CombinedLlmExtractor {
    async fn extract(&self, chunks: &[&str]) -> KbResult<Extracted> {
        let mut out = Extracted::default();

        for &chunk in chunks {
            let prompt = build_prompt(chunk);
            let body = serde_json::json!({
                "model": &self.model,
                "messages": [{ "role": "user", "content": prompt }],
                "temperature": 0,
            });

            let resp = match self
                .client
                .post(&self.chat_url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "LLM extractor: HTTP request failed, skipping chunk");
                    continue;
                }
            };

            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    status = status.as_u16(),
                    body = %text,
                    "LLM extractor: non-success response, skipping chunk"
                );
                continue;
            }

            let chat_resp: ChatResponse = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "LLM extractor: failed to deserialize chat response, skipping chunk");
                    continue;
                }
            };

            let content = match chat_resp.choices.into_iter().next() {
                Some(c) => c.message.content,
                None => {
                    tracing::warn!("LLM extractor: empty choices array, skipping chunk");
                    continue;
                }
            };

            let payload: ExtractionPayload = match serde_json::from_str(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        content = %content,
                        "LLM extractor: content is not valid extraction JSON, skipping chunk"
                    );
                    continue;
                }
            };

            for ent in payload.entities {
                out.entities.push((
                    ent.name,
                    EntityType::from_str_lenient(&ent.entity_type),
                    ent.confidence,
                ));
            }

            for rel in payload.relations {
                out.relations.push((
                    rel.source,
                    rel.target,
                    RelationType::from_str_lenient(&rel.relation_type),
                    rel.confidence,
                ));
            }
        }

        Ok(out)
    }
}
