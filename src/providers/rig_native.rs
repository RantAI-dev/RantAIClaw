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
//! OpenAI/Gemini back through the hand-rolled files. The factory in
//! `mod.rs` chooses at compile time; the three modules are gated out
//! of default builds but retained in tree pending maintainer approval
//! to delete them (see `plans/016-legacy-providers-sunset.md`).

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
    /// provider's default API base URL. Used for OpenAI (compat endpoints
    /// like MiniMax, Together, xAI when the user prefers `openai` as the
    /// canonical name) and Anthropic (e.g. `anthropic-custom:` endpoints).
    /// Ignored for Gemini — rig's Gemini client doesn't support arbitrary
    /// base URLs.
    pub fn for_provider_with_url(
        canonical_name: &'static str,
        api_key: Option<&str>,
        api_url: Option<&str>,
    ) -> Result<Self> {
        let inner = match canonical_name {
            "anthropic" => {
                let key = api_key.context("anthropic: ANTHROPIC_API_KEY required")?;
                let mut builder = anthropic::Client::builder().api_key(key);
                if let Some(url) = api_url {
                    builder = builder.base_url(url);
                }
                let client = builder.build().context("rig anthropic client build")?;
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

/// OpenAI reasoning models (the `gpt-5*` family and the `o1`/`o3`/`o4` series)
/// reject any `temperature` other than the default of `1` — sending `0.7` yields
/// `400 unsupported_value`. Detect them by id prefix so we omit `temperature`
/// instead of failing the completion. Other providers (claude-*, gemini-*) are
/// unaffected. Tolerates an optional `provider/` prefix on the id.
fn model_locks_temperature(model: &str) -> bool {
    let id = model.trim().to_ascii_lowercase();
    let id = id.rsplit('/').next().unwrap_or(id.as_str());
    id.starts_with("gpt-5") || id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4")
}

/// Rebuild an assistant turn from the agent loop's native-mode JSON envelope
/// (`{"content": <str|null>, "tool_calls": [{"id","name","arguments"}]}`) into a
/// structured rig message carrying real `AssistantContent::ToolCall`s. Without
/// this the tool_calls collapse into plain text and the follow-up completion has
/// no assistant tool call for the tool result to pair with — which strict
/// providers (OpenAI) reject. Falls back to a plain text assistant message when
/// `content` is not that envelope. Mirrors the `convert_messages` decode the
/// native providers already perform.
fn assistant_message_from(content: &str) -> RigMessage {
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok();
    let Some(calls) = parsed
        .as_ref()
        .and_then(|v| v.get("tool_calls"))
        .and_then(|v| serde_json::from_value::<Vec<ToolCall>>(v.clone()).ok())
    else {
        return RigMessage::assistant(content.to_string());
    };

    let mut blocks: Vec<AssistantContent> = Vec::new();
    if let Some(text) = parsed
        .as_ref()
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        blocks.push(AssistantContent::text(text));
    }
    for call in calls {
        // `arguments` is a JSON string in our shape; rig wants a `Value`.
        let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        blocks.push(AssistantContent::tool_call(call.id, call.name, arguments));
    }

    match OneOrMany::many(blocks) {
        Ok(content_blocks) => RigMessage::Assistant {
            id: None,
            content: content_blocks,
        },
        // Envelope had no text and no tool calls — keep the raw text.
        Err(_) => RigMessage::assistant(content.to_string()),
    }
}

/// Rebuild a tool-result turn from the agent loop's JSON envelope
/// (`{"tool_call_id": <id>, "content": <result>}`), preserving the real id so it
/// pairs with the assistant tool call. The previous hardcoded `"unknown"` id
/// caused OpenAI to 400 on every post-tool-call completion.
fn tool_result_message_from(content: &str) -> RigMessage {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(id) = value.get("tool_call_id").and_then(|v| v.as_str()) {
            let result = value
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or(content);
            return RigMessage::tool_result(id, result);
        }
    }
    // Non-conforming tool message (no envelope): best-effort passthrough.
    RigMessage::tool_result("unknown", content.to_string())
}

/// Build the `CompletionRequest` shape `rig` expects from our
/// `ChatRequest` + per-call (model, temperature). System messages are
/// flattened to the `preamble` field for compatibility with all three
/// providers — rig also accepts a leading `Message::System` in
/// `chat_history` but mixing the two confuses some providers.
/// Anthropic's Messages API rejects a completion with no `max_tokens`
/// ("max_tokens must be set for Anthropic"), and rig only auto-defaults the
/// claude-4 tiers — so a claude-3.x request built with `max_tokens: None`
/// errors before it is ever sent (the request never reaches the network).
/// Fill a per-model default: mirror rig's own claude-4 values, and fall back to
/// 4096 — the claude-3 output cap and the legacy `AnthropicProvider` default —
/// for everything else. 4096 never exceeds any Anthropic model's cap, so it is
/// always accepted. Only applied for the Anthropic backend; OpenAI/Gemini keep
/// `None` (their APIs treat it as "model default").
fn with_anthropic_max_tokens(mut req: CompletionRequest, model: &str) -> CompletionRequest {
    if req.max_tokens.is_none() {
        req.max_tokens = Some(match model {
            m if m.starts_with("claude-opus-4-7") || m.starts_with("claude-opus-4-6") => 128_000,
            m if m.starts_with("claude-opus-4")
                || m.starts_with("claude-sonnet-4")
                || m.starts_with("claude-haiku-4-5") =>
            {
                64_000
            }
            _ => 4_096,
        });
    }
    req
}

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
            "assistant" => history.push(assistant_message_from(&m.content)),
            "tool" => history.push(tool_result_message_from(&m.content)),
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
        // Reasoning models only accept the default temperature; omit ours for them.
        temperature: if model_locks_temperature(model) {
            None
        } else {
            Some(temperature)
        },
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

/// Collapse tool calls that share an id. rig's streaming can surface one
/// logical tool call twice — once via the immediate-complete `ToolCall` event
/// and again via the delta-accumulated copy — yielding two entries with the
/// same id. OpenAI assigns a unique id per call, so duplicate ids are never
/// legitimate; sending two `tool_calls` (and two tool results) with the same id
/// makes the next completion 400. Keep first-seen order; merge a non-empty name
/// or arguments from a later duplicate over an empty start stub.
fn dedupe_tool_calls_by_id(calls: Vec<ToolCall>) -> Vec<ToolCall> {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: std::collections::HashMap<String, ToolCall> = std::collections::HashMap::new();
    for call in calls {
        match by_id.get_mut(&call.id) {
            Some(existing) => {
                if existing.name.is_empty() && !call.name.is_empty() {
                    existing.name = call.name;
                }
                if existing.arguments.trim().is_empty() && !call.arguments.trim().is_empty() {
                    existing.arguments = call.arguments;
                }
            }
            None => {
                order.push(call.id.clone());
                by_id.insert(call.id.clone(), call);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
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
                let req = with_anthropic_max_tokens(req, model);
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
                let req = with_anthropic_max_tokens(req, model);
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
        // rig may surface a tool call via both the immediate-complete event and
        // the delta path; collapse same-id duplicates before they reach the
        // next request (OpenAI 400s on duplicate tool_call ids).
        tool_calls: dedupe_tool_calls_by_id(tool_calls),
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

    /// `for_provider_with_url("anthropic", ..)` is the reroute target for
    /// the `anthropic-custom:` factory branch in `mod.rs`. The `Provider`
    /// trait has no base-URL getter and `Box<dyn Provider>` isn't
    /// downcastable, so `is_ok()` on construction can't prove the custom
    /// URL was actually threaded through to rig's HTTP client — a naive
    /// reroute could silently drop it and still construct fine. Instead,
    /// point the custom base URL at a wiremock server and prove the
    /// client actually sent its request there: if `.base_url(url)` weren't
    /// wired up, rig would fall back to the real `api.anthropic.com` and
    /// this mock would never see a request.
    #[tokio::test]
    async fn for_provider_with_url_anthropic_honors_custom_base_url() {
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "type": "message",
                    "id": "msg_test",
                    "model": "claude-3-haiku-20240307",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "hello from mock"}],
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = RigProvider::for_provider_with_url(
            "anthropic",
            Some("test-key"),
            Some(&mock_server.uri()),
        )
        .expect("construct anthropic provider with custom base url");

        let result = provider
            .chat_with_system(None, "hi", "claude-3-haiku-20240307", 0.5)
            .await;

        // The core assertion: exactly one request landed on the mock. This
        // is what proves the custom base URL was honored, independent of
        // whether the mocked response happens to parse cleanly.
        mock_server.verify().await;

        assert_eq!(
            result.expect("chat should succeed against the mock server"),
            "hello from mock"
        );
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
    fn model_locks_temperature_flags_reasoning_models_only() {
        for m in [
            "gpt-5-mini",
            "gpt-5.5",
            "gpt-5.5-codex",
            "o1",
            "o3-mini",
            "o4-mini",
            "openai/gpt-5-nano",
        ] {
            assert!(model_locks_temperature(m), "{m} should lock temperature");
        }
        for m in [
            "gpt-4o",
            "gpt-4o-mini",
            "claude-sonnet-4-6",
            "gemini-2.0-flash",
        ] {
            assert!(!model_locks_temperature(m), "{m} should allow temperature");
        }
    }

    #[test]
    fn build_rig_request_omits_temperature_for_reasoning_models() {
        let messages = vec![ChatMessage::user("hi")];
        let req = build_rig_request(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "gpt-5-mini",
            0.7,
        )
        .unwrap();
        assert_eq!(
            req.temperature, None,
            "gpt-5 models reject a custom temperature; it must be omitted"
        );
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

    #[test]
    fn build_rig_request_rebuilds_assistant_tool_calls() {
        // The agent loop stores an assistant tool turn as JSON in `content`:
        // {"content": <str|null>, "tool_calls": [{"id","name","arguments"}]}.
        // It must be rebuilt as a structured AssistantContent::ToolCall (id
        // preserved), not flattened into plain text.
        let assistant_json = serde_json::json!({
            "content": serde_json::Value::Null,
            "tool_calls": [{
                "id": "call_1",
                "name": "glob_search",
                "arguments": "{\"pattern\":\"**/*\"}"
            }]
        })
        .to_string();
        let messages = vec![
            ChatMessage::user("find files"),
            ChatMessage {
                role: "assistant".into(),
                content: assistant_json,
            },
        ];
        let req = build_rig_request(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "gpt-5-mini",
            0.0,
        )
        .unwrap();

        let found = req.chat_history.clone().into_iter().any(|m| match m {
            RigMessage::Assistant { content, .. } => content.into_iter().any(|c| {
                matches!(
                    c,
                    AssistantContent::ToolCall(tc)
                        if tc.id == "call_1" && tc.function.name == "glob_search"
                )
            }),
            _ => false,
        });
        assert!(
            found,
            "assistant tool_calls must be rebuilt as AssistantContent::ToolCall with id preserved"
        );
    }

    #[test]
    fn build_rig_request_uses_real_tool_call_id() {
        // The tool result is stored as {"tool_call_id": <id>, "content": <result>}.
        // The real id must reach rig's ToolResult — never the "unknown" placeholder,
        // which OpenAI rejects as an unmatched tool message.
        let assistant_json = serde_json::json!({
            "content": serde_json::Value::Null,
            "tool_calls": [{"id": "call_1", "name": "glob_search", "arguments": "{}"}]
        })
        .to_string();
        let tool_json = serde_json::json!({
            "tool_call_id": "call_1",
            "content": "3 files found"
        })
        .to_string();
        let messages = vec![
            ChatMessage::user("find files"),
            ChatMessage {
                role: "assistant".into(),
                content: assistant_json,
            },
            ChatMessage {
                role: "tool".into(),
                content: tool_json,
            },
        ];
        let req = build_rig_request(
            ChatRequest {
                messages: &messages,
                tools: None,
            },
            "gpt-5-mini",
            0.0,
        )
        .unwrap();

        let ids: Vec<String> = req
            .chat_history
            .clone()
            .into_iter()
            .filter_map(|m| match m {
                RigMessage::User { content } => content.into_iter().find_map(|c| match c {
                    rig_core::completion::message::UserContent::ToolResult(tr) => Some(tr.id),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert!(
            ids.contains(&"call_1".to_string()),
            "tool result must carry the real tool_call_id; got {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "unknown"),
            "tool result must not use the 'unknown' placeholder id"
        );
    }

    #[test]
    fn build_rig_request_keeps_plain_assistant_text() {
        // Regression guard: a plain (non-JSON) assistant message must still
        // become a text block, not be mistaken for a tool turn.
        let messages = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("just text, no tools"),
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
        let has_text = req.chat_history.clone().into_iter().any(|m| match m {
            RigMessage::Assistant { content, .. } => content
                .into_iter()
                .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "just text, no tools")),
            _ => false,
        });
        assert!(has_text, "plain assistant text must remain a text block");
    }

    #[test]
    fn dedupe_tool_calls_collapses_same_id() {
        // rig's streaming can surface one logical tool call twice (the
        // immediate-complete event plus the delta-accumulated copy). OpenAI
        // assigns a unique id per call, so two entries sharing an id are a
        // duplicate and must collapse — otherwise the next request 400s.
        let calls = vec![
            ToolCall {
                id: "call_1".into(),
                name: "glob_search".into(),
                arguments: "{\"pattern\":\"**/*\"}".into(),
            },
            ToolCall {
                id: "call_1".into(),
                name: "glob_search".into(),
                arguments: "{\"pattern\":\"**/*\"}".into(),
            },
        ];
        let deduped = dedupe_tool_calls_by_id(calls);
        assert_eq!(deduped.len(), 1, "same-id duplicates must collapse to one");
        assert_eq!(deduped[0].id, "call_1");
        assert_eq!(deduped[0].arguments, "{\"pattern\":\"**/*\"}");
    }

    #[test]
    fn dedupe_tool_calls_prefers_complete_args() {
        // If the duplicate carries the full args and the first was an empty
        // start stub, keep the complete arguments.
        let calls = vec![
            ToolCall {
                id: "call_1".into(),
                name: "glob_search".into(),
                arguments: String::new(),
            },
            ToolCall {
                id: "call_1".into(),
                name: String::new(),
                arguments: "{\"pattern\":\"src/**\"}".into(),
            },
        ];
        let deduped = dedupe_tool_calls_by_id(calls);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].name, "glob_search", "non-empty name retained");
        assert_eq!(
            deduped[0].arguments, "{\"pattern\":\"src/**\"}",
            "complete args retained over empty stub"
        );
    }

    #[test]
    fn dedupe_tool_calls_keeps_distinct_ids_in_order() {
        let calls = vec![
            ToolCall {
                id: "call_a".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "call_b".into(),
                name: "file_read".into(),
                arguments: "{}".into(),
            },
        ];
        let deduped = dedupe_tool_calls_by_id(calls);
        assert_eq!(deduped.len(), 2, "distinct ids must all survive");
        assert_eq!(deduped[0].id, "call_a", "order preserved");
        assert_eq!(deduped[1].id, "call_b");
    }
}
