//! `rig-core` adapter — replaces the hand-rolled Anthropic, OpenAI,
//! and Gemini provider files with one `Provider` impl that delegates
//! all SSE parsing, request shaping, and per-provider quirk handling
//! to [`rig_core`].
//!
//! ## Why this exists
//!
//! Before v0.6.39-alpha, every provider in `src/providers/` carried
//! its own request/response Serde structs, its own SSE parser, and
//! its own state machine for tool-call accumulation. Two of those
//! ten files actually streamed (OpenRouter and the OpenAI-compatible
//! shim used by Groq/MiniMax/xAI/etc.); three of them — Anthropic,
//! OpenAI native, Gemini — sat without streaming because their wire
//! formats diverge from OpenAI's and we hadn't written parsers for
//! the event-typed Anthropic SSE / Gemini SSE-with-`\n\n`-blocks
//! shapes.
//!
//! `rig-core` (MIT, 0xPlaygrounds/rig) already implements native
//! streaming for 22 providers including all three of those gaps. The
//! v0.6.0 sprint's `Feature: Per-Provider SSE Streaming` task closes
//! by routing those three through `rig` and gaining a dozen extra
//! providers as a side effect (cohere/deepseek/perplexity/etc.).
//!
//! ## What this file does
//!
//! Exposes `RigProvider` — one type, three variants under the hood,
//! one `Provider` trait impl. The variant is chosen at construction
//! time from the canonical provider name string we get from
//! `create_provider()`. After that the trait method dispatch is
//! uniform.
//!
//! ## What it doesn't do (yet)
//!
//! - Doesn't touch OpenRouter or the OpenAI-compatible shim. Those
//!   already stream correctly via our hand-rolled SSE; replacing
//!   them is v0.6.x cleanup, not v0.6.0 sprint scope.
//! - Doesn't touch Bedrock, Copilot, Ollama, OpenAI-Codex. Those
//!   use specialized paths (AWS SDK shape, GitHub Copilot OAuth,
//!   local NDJSON, OpenAI responses API) that are out of scope for
//!   this PR.
//! - Doesn't expose every rig knob — we wire the fields the agent
//!   loop actually uses (messages, tools, model, temperature) and
//!   skip the rest until they're needed.
//!
//! ## Fallback
//!
//! Build with `--features legacy-providers` to route Anthropic/
//! OpenAI/Gemini back through the hand-rolled files (kept in tree
//! through v0.7.0 for safety). The factory in `mod.rs` chooses at
//! compile time. Will be removed after one release cycle of clean
//! production use.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::message::{AssistantContent, ToolFunction};
use rig_core::completion::{
    CompletionModel, CompletionRequest, Message as RigMessage, ToolDefinition,
};
use rig_core::one_or_many::OneOrMany;
use rig_core::providers::{anthropic, gemini, openai};
use rig_core::streaming::StreamedAssistantContent;
use tokio::sync::mpsc;

use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities, ToolCall, ToolsPayload,
};
use crate::tools::ToolSpec;

/// Inner client dispatch — one concrete `rig_core` client per supported
/// provider. We can't `Box<dyn CompletionClient>` because the trait has
/// an associated type for its model, so we enum-dispatch instead. Each
/// variant is cheap to construct and `Clone` — the underlying
/// `reqwest::Client` is `Arc`-wrapped internally.
enum RigClient {
    Anthropic(anthropic::Client),
    OpenAi(openai::CompletionsClient),
    Gemini(gemini::Client),
}

/// `Provider` impl backed by `rig_core`.
///
/// Construct via [`RigProvider::for_provider`]. The resulting struct
/// satisfies our `Provider` trait and routes every method through the
/// appropriate `rig` client.
pub struct RigProvider {
    inner: RigClient,
    /// Canonical name used in tracing + error messages
    /// (`"anthropic"`, `"openai"`, `"gemini"`).
    canonical_name: &'static str,
}

impl std::fmt::Debug for RigProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RigProvider")
            .field("provider", &self.canonical_name)
            .finish()
    }
}

impl RigProvider {
    /// Construct a `RigProvider` for one of the supported native
    /// providers. Returns `Err` if the name isn't on our adopt list
    /// — caller should fall back to a different factory branch (or
    /// the legacy hand-rolled file under
    /// `--features legacy-providers`).
    pub fn for_provider(canonical_name: &'static str, api_key: Option<&str>) -> Result<Self> {
        Self::for_provider_with_url(canonical_name, api_key, None)
    }

    /// Same as [`Self::for_provider`] but lets the caller override the
    /// provider's default API base URL. Only meaningful for OpenAI
    /// (used to point at compat endpoints like MiniMax, Together,
    /// xAI when the user prefers `openai` as the canonical name).
    /// Ignored for Anthropic + Gemini — rig's clients don't support
    /// arbitrary base URLs there.
    pub fn for_provider_with_url(
        canonical_name: &'static str,
        api_key: Option<&str>,
        api_url: Option<&str>,
    ) -> Result<Self> {
        let inner = match canonical_name {
            "anthropic" => {
                let key = api_key.context("anthropic: ANTHROPIC_API_KEY required")?;
                let client = anthropic::Client::builder()
                    .api_key(key)
                    .build()
                    .context("rig anthropic client build")?;
                RigClient::Anthropic(client)
            }
            "openai" => {
                let key = api_key.context("openai: OPENAI_API_KEY required")?;
                let mut builder = openai::CompletionsClient::builder().api_key(key);
                if let Some(url) = api_url {
                    builder = builder.base_url(url);
                }
                let client = builder.build().context("rig openai client build")?;
                RigClient::OpenAi(client)
            }
            "gemini" => {
                let key = api_key.context("gemini: GEMINI_API_KEY required")?;
                let client = gemini::Client::builder()
                    .api_key(key)
                    .build()
                    .context("rig gemini client build")?;
                RigClient::Gemini(client)
            }
            other => {
                return Err(anyhow!(
                    "RigProvider doesn't support `{other}` yet — add the variant or use legacy"
                ));
            }
        };
        Ok(Self {
            inner,
            canonical_name,
        })
    }
}

/// Build the `CompletionRequest` shape `rig` expects from our
/// `ChatRequest` + per-call (model, temperature). System messages are
/// flattened to the `preamble` field for compatibility with all three
/// providers — rig also accepts a leading `Message::System` in
/// `chat_history` but mixing the two confuses some providers.
fn build_rig_request(
    request: ChatRequest<'_>,
    model: &str,
    temperature: f64,
) -> Result<CompletionRequest> {
    let mut preamble: Option<String> = None;
    let mut history: Vec<RigMessage> = Vec::new();
    for m in request.messages {
        match m.role.as_str() {
            "system" => {
                // Concatenate multiple system messages (rare but
                // legal) into one preamble block.
                preamble = Some(match preamble.take() {
                    Some(prev) => format!("{prev}\n\n{}", m.content),
                    None => m.content.clone(),
                });
            }
            "user" => history.push(RigMessage::user(m.content.clone())),
            "assistant" => history.push(RigMessage::assistant(m.content.clone())),
            "tool" => {
                // Tool results carry no id in our `ChatMessage` shape
                // — rig requires one. Synthesize a fallback id; the
                // agent loop already pairs tool calls with results
                // by position when ids are missing.
                history.push(RigMessage::tool_result("unknown", m.content.clone()));
            }
            other => {
                tracing::warn!(target: "rig_native", role = %other, "unknown role; treating as user");
                history.push(RigMessage::user(m.content.clone()));
            }
        }
    }
    let chat_history = OneOrMany::many(history)
        .map_err(|e| anyhow!("rig_native: chat history must have at least one message: {e}"))?;

    let tools: Vec<ToolDefinition> = request
        .tools
        .map(|specs| {
            specs
                .iter()
                .map(|s| ToolDefinition {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    parameters: s.parameters.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(CompletionRequest {
        model: Some(model.to_string()),
        preamble,
        chat_history,
        documents: Vec::new(),
        tools,
        temperature: Some(temperature),
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
    })
}

/// Collapse rig's `AssistantContent` enum into our `ChatResponse`
/// shape. Text → ChatResponse.text (concatenated), ToolCall →
/// pushed to tool_calls. Reasoning + Image content currently
/// flatten into the text — we don't expose reasoning blocks
/// through the agent loop yet.
fn flatten_assistant(choice: OneOrMany<AssistantContent>) -> (Option<String>, Vec<ToolCall>) {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for part in choice {
        match part {
            AssistantContent::Text(t) => text.push_str(&t.text),
            AssistantContent::ToolCall(tc) => tool_calls.push(rig_tool_to_ours(tc.id, tc.function)),
            AssistantContent::Reasoning(r) => {
                // Preserve as text for now — could surface
                // separately once the agent loop has a reasoning
                // event channel. `r.content` is a `Vec<ReasoningContent>`;
                // we extract the text variant of each block.
                for block in r.content {
                    if let rig_core::completion::message::ReasoningContent::Text {
                        text: t, ..
                    } = block
                    {
                        text.push_str(&t);
                    }
                }
            }
            AssistantContent::Image(_) => {
                tracing::debug!(target: "rig_native", "ignoring image content in chat response");
            }
        }
    }
    let text_opt = if text.is_empty() { None } else { Some(text) };
    (text_opt, tool_calls)
}

fn rig_tool_to_ours(id: String, f: ToolFunction) -> ToolCall {
    ToolCall {
        id,
        name: f.name,
        arguments: f.arguments.to_string(),
    }
}

#[async_trait]
impl Provider for RigProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        // All three providers we wrap have native tool calling, vision,
        // and streaming. We declare it explicitly so the agent loop
        // doesn't degrade to the prompt-guided tool path.
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        // rig accepts tools as a typed `Vec<ToolDefinition>` and we
        // build that inline in `build_rig_request`. The agent loop's
        // `convert_tools` path is only used for prompt-guided fallback
        // when `supports_native_tools()` returns false — which we
        // don't, since rig handles native tool calling for all three
        // providers. So this method is effectively dead code for the
        // rig path; the variant we return doesn't matter as long as
        // it's well-formed. Return per-provider native shape so any
        // future caller that does inspect it gets a sane payload.
        let payload: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        match self.canonical_name {
            "anthropic" => ToolsPayload::Anthropic { tools: payload },
            "gemini" => ToolsPayload::Gemini {
                function_declarations: payload,
            },
            _ => ToolsPayload::OpenAI { tools: payload },
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        let mut messages = Vec::new();
        if let Some(s) = system_prompt {
            messages.push(ChatMessage::system(s));
        }
        messages.push(ChatMessage::user(message));
        let response = self
            .chat(
                ChatRequest {
                    messages: &messages,
                    tools: None,
                },
                model,
                temperature,
            )
            .await?;
        Ok(response.text.unwrap_or_default())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> Result<ChatResponse> {
        let req = build_rig_request(request, model, temperature)?;
        match &self.inner {
            RigClient::Anthropic(c) => {
                let m = c.completion_model(model);
                let resp = m
                    .completion(req)
                    .await
                    .with_context(|| format!("rig anthropic completion failed (model={model})"))?;
                let (text, tool_calls) = flatten_assistant(resp.choice);
                Ok(ChatResponse { text, tool_calls })
            }
            RigClient::OpenAi(c) => {
                let m = c.completion_model(model);
                let resp = m
                    .completion(req)
                    .await
                    .with_context(|| format!("rig openai completion failed (model={model})"))?;
                let (text, tool_calls) = flatten_assistant(resp.choice);
                Ok(ChatResponse { text, tool_calls })
            }
            RigClient::Gemini(c) => {
                let m = c.completion_model(model);
                let resp = m
                    .completion(req)
                    .await
                    .with_context(|| format!("rig gemini completion failed (model={model})"))?;
                let (text, tool_calls) = flatten_assistant(resp.choice);
                Ok(ChatResponse { text, tool_calls })
            }
        }
    }

    async fn chat_stream(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
        text_tx: mpsc::Sender<String>,
    ) -> Result<ChatResponse> {
        let req = build_rig_request(request, model, temperature)?;
        match &self.inner {
            RigClient::Anthropic(c) => {
                stream_through_rig(c.completion_model(model), req, text_tx, self.canonical_name)
                    .await
            }
            RigClient::OpenAi(c) => {
                stream_through_rig(c.completion_model(model), req, text_tx, self.canonical_name)
                    .await
            }
            RigClient::Gemini(c) => {
                stream_through_rig(c.completion_model(model), req, text_tx, self.canonical_name)
                    .await
            }
        }
    }

    async fn warmup(&self) -> Result<()> {
        // rig's reqwest::Client is built lazily; the first request
        // performs the TLS handshake. We could send a 1-token probe
        // here but that costs the user real money. No-op for now.
        Ok(())
    }
}

/// Generic stream handler — works for any `CompletionModel` because
/// rig normalizes `StreamedAssistantContent` across providers.
async fn stream_through_rig<M>(
    model: M,
    req: CompletionRequest,
    text_tx: mpsc::Sender<String>,
    canonical: &str,
) -> Result<ChatResponse>
where
    M: CompletionModel,
{
    let stream_resp = model
        .stream(req)
        .await
        .with_context(|| format!("rig {canonical} stream open failed"))?;

    let mut full_text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    // Pre-allocate so accumulating tool call argument fragments doesn't
    // reallocate per chunk. Most calls have <2KB of args.
    let mut tool_arg_acc: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut tool_meta: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

    let mut stream = std::pin::pin!(stream_resp);
    while let Some(item) = stream.next().await {
        let item = item.with_context(|| format!("rig {canonical} stream chunk"))?;
        match item {
            StreamedAssistantContent::Text(t) => {
                if !t.text.is_empty() {
                    full_text.push_str(&t.text);
                    // Channel send error means the consumer hung up.
                    // That's fine; keep accumulating for the return
                    // value so the agent loop still gets a complete
                    // ChatResponse.
                    let _ = text_tx.send(t.text).await;
                }
            }
            StreamedAssistantContent::ToolCall {
                tool_call,
                internal_call_id: _,
            } => {
                // Complete tool call — push directly.
                tool_calls.push(rig_tool_to_ours(tool_call.id, tool_call.function));
            }
            StreamedAssistantContent::ToolCallDelta {
                id,
                internal_call_id: _,
                content,
            } => {
                // Partial tool call — `content` is an enum: either
                // `Name(String)` carrying the tool name, or
                // `Delta(String)` carrying a JSON-fragment of args.
                // Accumulate by id; we materialize into a ToolCall
                // after the stream ends.
                use rig_core::streaming::ToolCallDeltaContent;
                match content {
                    ToolCallDeltaContent::Name(name) => {
                        tool_meta
                            .entry(id.clone())
                            .and_modify(|(n, _)| {
                                if n.is_empty() {
                                    n.clone_from(&name);
                                }
                            })
                            .or_insert((name, id.clone()));
                    }
                    ToolCallDeltaContent::Delta(args) => {
                        tool_arg_acc.entry(id.clone()).or_default().push_str(&args);
                    }
                }
            }
            StreamedAssistantContent::Reasoning(r) => {
                // Streaming reasoning blocks — flatten into text for
                // now. Same compat note as the non-streaming path.
                for block in r.content {
                    if let rig_core::completion::message::ReasoningContent::Text {
                        text: t, ..
                    } = block
                    {
                        full_text.push_str(&t);
                    }
                }
            }
            StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                full_text.push_str(&reasoning);
            }
            other => {
                tracing::debug!(
                    target: "rig_native",
                    canonical,
                    variant = ?std::mem::discriminant(&other),
                    "ignoring stream variant"
                );
            }
        }
    }

    // Finalize delta-accumulated tool calls (OpenAI-style).
    for (id, args) in tool_arg_acc {
        let name = tool_meta
            .get(&id)
            .map(|(n, _)| n.clone())
            .unwrap_or_default();
        if !name.is_empty() {
            tool_calls.push(ToolCall {
                id,
                name,
                arguments: args,
            });
        }
    }

    let text_opt = if full_text.is_empty() {
        None
    } else {
        Some(full_text)
    };
    Ok(ChatResponse {
        text: text_opt,
        tool_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_provider_rejects_unknown() {
        let err = RigProvider::for_provider("nonexistent", Some("k")).unwrap_err();
        assert!(err.to_string().contains("doesn't support"));
    }

    #[test]
    fn for_provider_requires_key() {
        let err = RigProvider::for_provider("anthropic", None).unwrap_err();
        assert!(
            err.to_string().contains("API key required")
                || err.to_string().contains("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn for_provider_constructs_anthropic_with_key() {
        let p = RigProvider::for_provider("anthropic", Some("sk-fake")).expect("ctor");
        assert_eq!(p.canonical_name, "anthropic");
    }

    #[test]
    fn for_provider_constructs_openai_with_key() {
        let p = RigProvider::for_provider("openai", Some("sk-fake")).expect("ctor");
        assert_eq!(p.canonical_name, "openai");
    }

    #[test]
    fn for_provider_constructs_gemini_with_key() {
        let p = RigProvider::for_provider("gemini", Some("AIza-fake")).expect("ctor");
        assert_eq!(p.canonical_name, "gemini");
    }

    #[test]
    fn capabilities_declare_native_tools_and_vision_and_streaming() {
        let p = RigProvider::for_provider("anthropic", Some("sk")).unwrap();
        let caps = p.capabilities();
        assert!(caps.native_tool_calling);
        assert!(caps.vision);
        assert!(p.supports_streaming());
    }

    #[test]
    fn build_rig_request_flattens_system_to_preamble() {
        let messages = vec![ChatMessage::system("be brief"), ChatMessage::user("hi")];
        let req = build_rig_request(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "test-model",
            0.5,
        )
        .unwrap();
        assert_eq!(req.preamble.as_deref(), Some("be brief"));
        assert_eq!(req.model.as_deref(), Some("test-model"));
        assert_eq!(req.temperature, Some(0.5));
        // System extracted; only the user message remains in history.
        assert_eq!(req.chat_history.len(), 1);
    }

    #[test]
    fn build_rig_request_concatenates_multiple_system_messages() {
        let messages = vec![
            ChatMessage::system("be brief"),
            ChatMessage::system("also formal"),
            ChatMessage::user("hi"),
        ];
        let req = build_rig_request(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "m",
            0.0,
        )
        .unwrap();
        let preamble = req.preamble.expect("preamble");
        assert!(preamble.contains("be brief"));
        assert!(preamble.contains("also formal"));
    }

    #[test]
    fn build_rig_request_carries_tools() {
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let messages = vec![ChatMessage::user("hi")];
        let req = build_rig_request(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
            },
            "m",
            0.0,
        )
        .unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "shell");
    }
}
