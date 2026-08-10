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
    /// Max in-flight chat requests. Extraction used to be strictly
    /// sequential — one POST per chunk — so a 200-chunk document made 200
    /// serial round-trips (plan 112 item 6). Bounded, not unbounded: the
    /// same reasoning as `embed_concurrency`.
    concurrency: usize,
}

impl CombinedLlmExtractor {
    pub fn new(model: String, chat_url: String, api_key: String) -> Self {
        Self {
            model,
            chat_url,
            api_key,
            client: Client::new(),
            concurrency: 4,
        }
    }

    /// Override the request-concurrency bound (callers pass
    /// `cfg.embed_concurrency` so one knob governs both HTTP fan-outs).
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }
}

/// Per-chunk outcome for the concurrent fan-out — either the parsed
/// entities/relations or a short failure reason (never the upstream body).
enum ChunkOutcome {
    Ok {
        entities: Vec<(String, EntityType, f32)>,
        relations: Vec<(String, String, RelationType, f32)>,
    },
    Failed(String),
}

impl CombinedLlmExtractor {
    async fn extract_one(&self, chunk: &str) -> ChunkOutcome {
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
                return ChunkOutcome::Failed("transport error".into());
            }
        };

        let status = resp.status();
        if !status.is_success() {
            // Status only — the upstream body may echo request contents or
            // credential material and never belongs in logs (matches the
            // api.rs upstream-error policy).
            tracing::warn!(
                status = status.as_u16(),
                "LLM extractor: non-success response, skipping chunk"
            );
            return ChunkOutcome::Failed(format!("http {}", status.as_u16()));
        }

        let chat_resp: ChatResponse = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "LLM extractor: failed to deserialize chat response, skipping chunk");
                return ChunkOutcome::Failed("invalid response".into());
            }
        };

        let content = match chat_resp.choices.into_iter().next() {
            Some(c) => c.message.content,
            None => {
                tracing::warn!("LLM extractor: empty choices array, skipping chunk");
                return ChunkOutcome::Failed("empty response".into());
            }
        };

        let payload: ExtractionPayload = match serde_json::from_str(&content) {
            Ok(p) => p,
            Err(e) => {
                // Content echoes model output over document text — keep it
                // out of logs for the same reason as the body above.
                tracing::warn!(
                    error = %e,
                    "LLM extractor: content is not valid extraction JSON, skipping chunk"
                );
                return ChunkOutcome::Failed("invalid json".into());
            }
        };

        ChunkOutcome::Ok {
            entities: payload
                .entities
                .into_iter()
                .map(|ent| {
                    (
                        ent.name,
                        EntityType::from_str_lenient(&ent.entity_type),
                        sanitize_confidence(ent.confidence),
                    )
                })
                .collect(),
            relations: payload
                .relations
                .into_iter()
                .map(|rel| {
                    (
                        rel.source,
                        rel.target,
                        RelationType::from_str_lenient(&rel.relation_type),
                        sanitize_confidence(rel.confidence),
                    )
                })
                .collect(),
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
        use futures::stream::{self, StreamExt};

        // Bounded fan-out; buffered (ordered) so results come back in chunk
        // order and first_error deterministically belongs to the lowest
        // failing chunk index.
        // Futures are created eagerly (they're lazy until polled) and then
        // driven with bounded parallelism — sidesteps a HRTB inference
        // failure on the closure-returning-borrowing-future shape.
        let futs: Vec<_> = chunks.iter().map(|chunk| self.extract_one(chunk)).collect();
        let outcomes: Vec<ChunkOutcome> = stream::iter(futs)
            .buffered(self.concurrency)
            .collect()
            .await;

        let mut out = Extracted::default();
        for (chunk_index, outcome) in outcomes.into_iter().enumerate() {
            match outcome {
                ChunkOutcome::Ok {
                    entities,
                    relations,
                } => {
                    for (name, ty, conf) in entities {
                        out.entities.push((chunk_index, name, ty, conf));
                    }
                    out.relations.extend(relations);
                }
                ChunkOutcome::Failed(reason) => {
                    out.failed_chunks += 1;
                    out.first_error.get_or_insert(reason);
                }
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
