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

/// Confidence assigned when the model returns a non-positive score (0, negative, or a
/// `NaN`). The prompt instructs the model never to emit 0, but we sanitise defensively
/// so a single misbehaving response never surfaces as "0%" in the graph UI.
const UNSPECIFIED_CONFIDENCE: f32 = 0.5;

/// Clamp a model-reported confidence into the usable `(0, 1]` range. Non-positive or
/// non-finite values collapse to [`UNSPECIFIED_CONFIDENCE`]; values above 1 clamp to 1.
fn sanitize_confidence(raw: f32) -> f32 {
    if !raw.is_finite() || raw <= 0.0 {
        UNSPECIFIED_CONFIDENCE
    } else if raw > 1.0 {
        1.0
    } else {
        raw
    }
}

// ---------------------------------------------------------------------------
// Prompt builder
// ---------------------------------------------------------------------------

fn build_prompt(chunk: &str) -> String {
    format!(
        "You are an entity and relation extractor. Given the text below, extract all entities \
and relations. Output ONLY a JSON object with exactly this structure:\n\
{{\"entities\":[{{\"name\":\"...\",\"type\":\"...\",\"confidence\":0.95}}],\
\"relations\":[{{\"source\":\"...\",\"target\":\"...\",\"type\":\"...\",\"confidence\":0.9}}]}}\n\n\
`confidence` is your certainty for that item: a number strictly between 0 and 1. Use a high \
value (0.9-1.0) for facts clearly stated in the text and a lower value when inferred. \
NEVER output 0 — every extracted item must carry a real, non-zero confidence.\n\n\
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
                    sanitize_confidence(ent.confidence),
                ));
            }

            for rel in payload.relations {
                out.relations.push((
                    rel.source,
                    rel.target,
                    RelationType::from_str_lenient(&rel.relation_type),
                    sanitize_confidence(rel.confidence),
                ));
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{build_prompt, sanitize_confidence, UNSPECIFIED_CONFIDENCE};

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn prompt_does_not_seed_zero_confidence_and_forbids_it() {
        let p = build_prompt("hello world");
        // The structural example must not seed a 0 the model can echo back verbatim.
        assert!(
            !p.contains("\"confidence\":0.0"),
            "prompt still contains a 0.0 confidence example: {p}"
        );
        // A realistic non-zero example and an explicit non-zero instruction are present.
        assert!(p.contains("0.95"), "prompt lost its non-zero example");
        assert!(
            p.contains("NEVER output 0"),
            "prompt lost the never-zero instruction"
        );
    }

    #[test]
    fn sanitize_confidence_floors_non_positive_and_clamps_high() {
        assert!(approx(sanitize_confidence(0.0), UNSPECIFIED_CONFIDENCE));
        assert!(approx(sanitize_confidence(-0.3), UNSPECIFIED_CONFIDENCE));
        assert!(approx(
            sanitize_confidence(f32::NAN),
            UNSPECIFIED_CONFIDENCE
        ));
        assert!(approx(sanitize_confidence(0.9), 0.9));
        assert!(approx(sanitize_confidence(1.5), 1.0));
    }
}
