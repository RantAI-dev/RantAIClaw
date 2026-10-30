//! Model routing: the router that resolves a task hint to a provider and a
//! model, and the guard that refuses a hint nothing can resolve.
//!
//! A `[[model_routes]]` table maps a hint to a provider and a model. A model
//! string that starts with `hint:` names one of those hints. Two things can go
//! wrong, and both are refused here instead of being sent to a provider:
//!
//! - the hint is not in the table: the router says so, naming the hint and
//!   the hints that are configured;
//! - there is no table at all: the provider is wrapped in a [`HintGuard`],
//!   which refuses the hint and names the missing table.
//!
//! Before this, an unknown hint fell back to the default provider with the
//! literal string `hint:whatever` as the model name, and a door with no route
//! table sent the same literal string to its provider. Both reached a model
//! that does not exist, and the answer named the model instead of the
//! missing table.

use super::traits::{
    build_tool_instructions_text, ChatMessage, ChatRequest, ChatResponse, ProviderCapabilities,
    StreamChunk, StreamError, StreamOptions, StreamResult, ToolsPayload,
};
use super::Provider;
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use std::collections::HashMap;

/// The prefix a model string carries when it names a task hint.
pub const HINT_PREFIX: &str = "hint:";

/// What to say when a `hint:` model reaches a provider with no route table.
pub const NO_ROUTES_REFUSAL: &str = "no [[model_routes]] configured";

/// A single route: maps a task hint to a provider + model combo.
#[derive(Debug, Clone)]
pub struct Route {
    pub provider_name: String,
    pub model: String,
}

/// The stream a refusal carries: one error chunk, no provider involved.
///
/// The streaming methods of the trait cannot fail with a `Result`, so a
/// refusal travels as the single item of the stream.
fn refusal_stream(error: anyhow::Error) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
    stream::once(async move { Err(StreamError::Provider(error.to_string())) }).boxed()
}

/// Multi-model router — routes requests to different provider+model combos
/// based on a task hint encoded in the model parameter.
///
/// The model parameter can be:
/// - A regular model name (e.g. "anthropic/claude-sonnet-4") → uses default provider
/// - A hint-prefixed string (e.g. "hint:reasoning") → resolves via route table
///
/// This wraps multiple pre-created providers and selects the right one per request.
pub struct RouterProvider {
    routes: HashMap<String, (usize, String)>, // hint → (provider_index, model)
    /// Every hint the route table names, sorted and de-duplicated.
    ///
    /// The refusal for an unknown hint lists these, so an operator sees the
    /// hints they configured — including one whose provider could not be
    /// built and was therefore skipped.
    configured_hints: Vec<String>,
    providers: Vec<(String, Box<dyn Provider>)>,
    default_index: usize,
    default_model: String,
}

impl RouterProvider {
    /// Create a new router with a default provider and optional routes.
    ///
    /// `providers` is a list of (name, provider) pairs. The first one is the default.
    /// `routes` maps hint names to Route structs containing provider_name and model.
    pub fn new(
        providers: Vec<(String, Box<dyn Provider>)>,
        routes: Vec<(String, Route)>,
        default_model: String,
    ) -> Self {
        // Every hint the table names, whether or not it resolves to a provider
        // that could be built. The refusal names them.
        let mut configured_hints: Vec<String> =
            routes.iter().map(|(hint, _)| hint.clone()).collect();
        configured_hints.sort();
        configured_hints.dedup();

        // Build provider name → index lookup
        let name_to_index: HashMap<&str, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.as_str(), i))
            .collect();

        // Resolve routes to provider indices
        let resolved_routes: HashMap<String, (usize, String)> = routes
            .into_iter()
            .filter_map(|(hint, route)| {
                let index = name_to_index.get(route.provider_name.as_str()).copied();
                match index {
                    Some(i) => Some((hint, (i, route.model))),
                    None => {
                        tracing::warn!(
                            hint = hint,
                            provider = route.provider_name,
                            "Route references unknown provider, skipping"
                        );
                        None
                    }
                }
            })
            .collect();

        Self {
            routes: resolved_routes,
            configured_hints,
            providers,
            default_index: 0,
            default_model,
        }
    }

    /// Resolve a model parameter to a (provider_index, actual_model) pair.
    ///
    /// If the model starts with "hint:", look up the hint in the route table.
    /// Otherwise, use the default provider with the given model name.
    ///
    /// A hint the table does not name is refused, before any request is sent:
    /// the refusal names the hint and the hints that are configured, so the
    /// operator can see what to write.
    fn resolve(&self, model: &str) -> anyhow::Result<(usize, String)> {
        if let Some(hint) = model.strip_prefix(HINT_PREFIX) {
            if let Some((idx, resolved_model)) = self.routes.get(hint) {
                return Ok((*idx, resolved_model.clone()));
            }
            if self.configured_hints.is_empty() {
                anyhow::bail!("{NO_ROUTES_REFUSAL}");
            }
            // The hint is in the table but its provider could not be built, so
            // the hint names a route that cannot be served. Saying "unknown"
            // would hide the real cause: the operator wrote the route, and
            // the route's provider is the thing that failed.
            if self.configured_hints.iter().any(|known| known == hint) {
                anyhow::bail!(
                    "route hint \"{hint}\" is configured but its provider is not available; configured hints: {}",
                    self.configured_hints.join(", ")
                );
            }
            anyhow::bail!(
                "unknown route hint \"{hint}\"; configured hints: {}",
                self.configured_hints.join(", ")
            );
        }

        // Not a hint — use default provider with the model as-is
        Ok((self.default_index, model.to_string()))
    }
}

#[async_trait]
impl Provider for RouterProvider {
    /// The first routed provider that has a concrete type to show.
    ///
    /// The router holds a chain per route; a test that wants the provider
    /// the factory built reaches it through here.
    #[cfg(test)]
    fn test_as_any(&self) -> Option<&dyn std::any::Any> {
        self.providers
            .iter()
            .find_map(|(_, provider)| provider.test_as_any())
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (provider_idx, resolved_model) = self.resolve(model)?;

        let (provider_name, provider) = &self.providers[provider_idx];
        tracing::info!(
            provider = provider_name.as_str(),
            model = resolved_model.as_str(),
            "Router dispatching request"
        );

        provider
            .chat_with_system(system_prompt, message, &resolved_model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (provider_idx, resolved_model) = self.resolve(model)?;
        let (_, provider) = &self.providers[provider_idx];
        provider
            .chat_with_history(messages, &resolved_model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (provider_idx, resolved_model) = self.resolve(model)?;
        let (_, provider) = &self.providers[provider_idx];
        provider.chat(request, &resolved_model, temperature).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (provider_idx, resolved_model) = self.resolve(model)?;
        let (_, provider) = &self.providers[provider_idx];
        provider
            .chat_with_tools(messages, tools, &resolved_model, temperature)
            .await
    }

    /// The primary provider's capabilities.
    ///
    /// The trait's capability methods carry no model, so a router cannot say
    /// what the route a turn will take supports. It reports the primary
    /// provider's, which is the provider a turn without a hint reaches — the
    /// same convention `ReliableProvider` uses for its primary. A router that
    /// reported the trait's defaults would take native tool calling away from
    /// every door it serves.
    fn capabilities(&self) -> ProviderCapabilities {
        self.providers
            .get(self.default_index)
            .map(|(_, p)| p.capabilities())
            .unwrap_or_default()
    }

    fn supports_native_tools(&self) -> bool {
        self.providers
            .get(self.default_index)
            .map(|(_, p)| p.supports_native_tools())
            .unwrap_or(false)
    }

    /// The primary provider's vision support.
    ///
    /// Not "any provider": a route to a provider that cannot read images must
    /// not claim it can, or an image reaches a text-only model.
    fn supports_vision(&self) -> bool {
        self.providers
            .get(self.default_index)
            .map(|(_, p)| p.supports_vision())
            .unwrap_or(false)
    }

    /// Whether any provider supports streaming.
    ///
    /// Same convention as `ReliableProvider`: a stream is served by the first
    /// provider that has one, so one streaming provider is enough.
    fn supports_streaming(&self) -> bool {
        self.providers.iter().any(|(_, p)| p.supports_streaming())
    }

    /// The tool payload of the primary provider.
    ///
    /// The payload that is actually sent comes from the provider the turn
    /// resolves to: `chat_with_tools` delegates to it. This answers for the
    /// primary, the same convention the capabilities above use.
    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        self.providers
            .get(self.default_index)
            .map(|(_, p)| p.convert_tools(tools))
            .unwrap_or_else(|| ToolsPayload::PromptGuided {
                instructions: build_tool_instructions_text(tools),
            })
    }

    /// Streaming pass-through to the resolved provider. Without this
    /// override the trait default is used, which calls non-streaming
    /// `chat()` and emits one big chunk — defeating SSE streaming.
    async fn chat_stream(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
        text_tx: tokio::sync::mpsc::Sender<String>,
    ) -> anyhow::Result<ChatResponse> {
        let (provider_idx, resolved_model) = self.resolve(model)?;
        let (_, provider) = &self.providers[provider_idx];
        provider
            .chat_stream(request, &resolved_model, temperature, text_tx)
            .await
    }

    /// Streaming pass-through to the resolved provider.
    ///
    /// Without this override the trait default is used, which returns an
    /// empty stream — the turn would end with no text at all.
    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let (provider_idx, resolved_model) = match self.resolve(model) {
            Ok(resolved) => resolved,
            Err(error) => return refusal_stream(error),
        };
        let (_, provider) = &self.providers[provider_idx];
        provider.stream_chat_with_system(
            system_prompt,
            message,
            &resolved_model,
            temperature,
            options,
        )
    }

    /// Streaming pass-through to the resolved provider.
    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let (provider_idx, resolved_model) = match self.resolve(model) {
            Ok(resolved) => resolved,
            Err(error) => return refusal_stream(error),
        };
        let (_, provider) = &self.providers[provider_idx];
        provider.stream_chat_with_history(messages, &resolved_model, temperature, options)
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        for (name, provider) in &self.providers {
            tracing::info!(provider = name, "Warming up routed provider");
            if let Err(e) = provider.warmup().await {
                tracing::warn!(provider = name, "Warmup failed (non-fatal): {e}");
            }
        }
        Ok(())
    }
}

/// A provider that refuses a `hint:` model instead of sending it.
///
/// A door with no `[[model_routes]]` has no route to resolve a hint against.
/// Sending the literal string `hint:fast` to the provider asks for a model
/// that does not exist, and the answer names the model, not the missing
/// table. This wrapper refuses the turn before any request is sent and names
/// the table that is missing.
///
/// Everything else is the wrapped provider's: its capabilities, its tool
/// payload, its streaming, its retry and fallback. A wrapper that reported
/// the default capabilities would silently take native tool calling away from
/// the door it wraps.
pub struct HintGuard {
    inner: Box<dyn Provider>,
}

impl HintGuard {
    /// Wrap `inner`.
    pub fn new(inner: Box<dyn Provider>) -> Self {
        Self { inner }
    }

    /// Refuse `model` when it names a hint this provider cannot resolve.
    fn check(&self, model: &str) -> anyhow::Result<()> {
        match model.strip_prefix(HINT_PREFIX) {
            Some(hint) => {
                anyhow::bail!(
                    "{NO_ROUTES_REFUSAL}, so the model hint \"{hint}\" cannot be resolved"
                )
            }
            None => Ok(()),
        }
    }
}

#[async_trait]
impl Provider for HintGuard {
    /// The wrapped provider's own answer: the guard adds a refusal, not a
    /// provider, so what is behind it is what the factory built.
    #[cfg(test)]
    fn test_as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.test_as_any()
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.check(model)?;
        self.inner
            .chat_with_system(system_prompt, message, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.check(model)?;
        self.inner
            .chat_with_history(messages, model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.check(model)?;
        self.inner.chat(request, model, temperature).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.check(model)?;
        self.inner
            .chat_with_tools(messages, tools, model, temperature)
            .await
    }

    async fn chat_stream(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
        text_tx: tokio::sync::mpsc::Sender<String>,
    ) -> anyhow::Result<ChatResponse> {
        self.check(model)?;
        self.inner
            .chat_stream(request, model, temperature, text_tx)
            .await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    /// The wrapped provider's native tool support.
    ///
    /// Delegated, not derived from `capabilities()`: the wrapped provider may
    /// answer the two differently, and the door must keep the answer it had
    /// before the guard wrapped it.
    fn supports_native_tools(&self) -> bool {
        self.inner.supports_native_tools()
    }

    /// The wrapped provider's vision support, for the same reason.
    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        self.inner.convert_tools(tools)
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        if let Err(error) = self.check(model) {
            return refusal_stream(error);
        }
        self.inner
            .stream_chat_with_system(system_prompt, message, model, temperature, options)
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        if let Err(error) = self.check(model) {
            return refusal_stream(error);
        }
        self.inner
            .stream_chat_with_history(messages, model, temperature, options)
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        self.inner.warmup().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct MockProvider {
        calls: Arc<AtomicUsize>,
        response: &'static str,
        last_model: parking_lot::Mutex<String>,
    }

    impl MockProvider {
        fn new(response: &'static str) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                response,
                last_model: parking_lot::Mutex::new(String::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn last_model(&self) -> String {
            self.last_model.lock().clone()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_model.lock() = model.to_string();
            Ok(self.response.to_string())
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
                vision: true,
            }
        }

        /// A payload that is not the trait's default, so a test can tell the
        /// wrapped provider's payload from the default one.
        fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
            ToolsPayload::OpenAI {
                tools: tools
                    .iter()
                    .map(|tool| serde_json::json!({ "name": tool.name }))
                    .collect(),
            }
        }
    }

    fn make_router(
        providers: Vec<(&'static str, &'static str)>,
        routes: Vec<(&str, &str, &str)>,
    ) -> (RouterProvider, Vec<Arc<MockProvider>>) {
        let mocks: Vec<Arc<MockProvider>> = providers
            .iter()
            .map(|(_, response)| Arc::new(MockProvider::new(response)))
            .collect();

        let provider_list: Vec<(String, Box<dyn Provider>)> = providers
            .iter()
            .zip(mocks.iter())
            .map(|((name, _), mock)| {
                (
                    name.to_string(),
                    Box::new(Arc::clone(mock)) as Box<dyn Provider>,
                )
            })
            .collect();

        let route_list: Vec<(String, Route)> = routes
            .iter()
            .map(|(hint, provider_name, model)| {
                (
                    hint.to_string(),
                    Route {
                        provider_name: provider_name.to_string(),
                        model: model.to_string(),
                    },
                )
            })
            .collect();

        let router = RouterProvider::new(provider_list, route_list, "default-model".to_string());

        (router, mocks)
    }

    // Arc<MockProvider> should also be a Provider
    #[async_trait]
    impl Provider for Arc<MockProvider> {
        async fn chat_with_system(
            &self,
            system_prompt: Option<&str>,
            message: &str,
            model: &str,
            temperature: f64,
        ) -> anyhow::Result<String> {
            self.as_ref()
                .chat_with_system(system_prompt, message, model, temperature)
                .await
        }

        fn capabilities(&self) -> ProviderCapabilities {
            self.as_ref().capabilities()
        }

        fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
            self.as_ref().convert_tools(tools)
        }
    }

    #[tokio::test]
    async fn routes_hint_to_correct_provider() {
        let (router, mocks) = make_router(
            vec![("fast", "fast-response"), ("smart", "smart-response")],
            vec![
                ("fast", "fast", "llama-3-70b"),
                ("reasoning", "smart", "claude-opus"),
            ],
        );

        let result = router
            .simple_chat("hello", "hint:reasoning", 0.5)
            .await
            .unwrap();
        assert_eq!(result, "smart-response");
        assert_eq!(mocks[1].call_count(), 1);
        assert_eq!(mocks[1].last_model(), "claude-opus");
        assert_eq!(mocks[0].call_count(), 0);
    }

    #[tokio::test]
    async fn routes_fast_hint() {
        let (router, mocks) = make_router(
            vec![("fast", "fast-response"), ("smart", "smart-response")],
            vec![("fast", "fast", "llama-3-70b")],
        );

        let result = router.simple_chat("hello", "hint:fast", 0.5).await.unwrap();
        assert_eq!(result, "fast-response");
        assert_eq!(mocks[0].call_count(), 1);
        assert_eq!(mocks[0].last_model(), "llama-3-70b");
    }

    /// A hint the table does not name is refused, and the refusal names the
    /// hints that are configured. Before this, the literal string
    /// `hint:nonexistent` was sent to the default provider as a model name.
    #[tokio::test]
    async fn an_unknown_hint_is_refused_and_names_the_configured_hints() {
        let (router, mocks) = make_router(
            vec![("default", "default-response"), ("other", "other-response")],
            vec![("fast", "default", "llama-3-70b")],
        );

        let error = router
            .simple_chat("hello", "hint:nonexistent", 0.5)
            .await
            .expect_err("an unknown hint must be refused");
        let message = error.to_string();
        assert!(message.contains("unknown route hint"), "{message}");
        assert!(message.contains("nonexistent"), "{message}");
        assert!(message.contains("fast"), "{message}");
        assert_eq!(mocks[0].call_count(), 0, "no provider was asked");
        assert_eq!(mocks[1].call_count(), 0, "no provider was asked");
    }

    #[tokio::test]
    async fn non_hint_model_uses_default_provider() {
        let (router, mocks) = make_router(
            vec![
                ("primary", "primary-response"),
                ("secondary", "secondary-response"),
            ],
            vec![("code", "secondary", "codellama")],
        );

        let result = router
            .simple_chat("hello", "anthropic/claude-sonnet-4-20250514", 0.5)
            .await
            .unwrap();
        assert_eq!(result, "primary-response");
        assert_eq!(mocks[0].call_count(), 1);
        assert_eq!(mocks[0].last_model(), "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn resolve_preserves_model_for_non_hints() {
        let (router, _) = make_router(vec![("default", "ok")], vec![]);

        let (idx, model) = router.resolve("gpt-4o").expect("a plain model resolves");
        assert_eq!(idx, 0);
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn resolve_strips_hint_prefix() {
        let (router, _) = make_router(
            vec![("fast", "ok"), ("smart", "ok")],
            vec![("reasoning", "smart", "claude-opus")],
        );

        let (idx, model) = router
            .resolve("hint:reasoning")
            .expect("a known hint resolves");
        assert_eq!(idx, 1);
        assert_eq!(model, "claude-opus");
    }

    #[test]
    fn skips_routes_with_unknown_provider() {
        let (router, _) = make_router(
            vec![("default", "ok")],
            vec![("broken", "nonexistent", "model")],
        );

        // Route should not exist
        assert!(!router.routes.contains_key("broken"));
    }

    /// A hint whose provider could not be built is still a configured hint:
    /// the refusal names it, so the operator sees the table they wrote.
    #[tokio::test]
    async fn a_hint_whose_provider_is_missing_is_still_named() {
        let (router, mocks) = make_router(
            vec![("default", "default-response")],
            vec![("broken", "nonexistent", "model")],
        );

        let error = router
            .simple_chat("hello", "hint:broken", 0.5)
            .await
            .expect_err("an unresolvable hint must be refused");
        let message = error.to_string();
        assert!(
            message.contains("route hint \"broken\" is configured"),
            "{message}"
        );
        assert!(message.contains("broken"), "{message}");
        assert_eq!(mocks[0].call_count(), 0, "no provider was asked");
    }

    #[tokio::test]
    async fn warmup_calls_all_providers() {
        let (router, _) = make_router(vec![("a", "ok"), ("b", "ok")], vec![]);

        // Warmup should not error
        assert!(router.warmup().await.is_ok());
    }

    #[tokio::test]
    async fn chat_with_system_passes_system_prompt() {
        let mock = Arc::new(MockProvider::new("response"));
        let router = RouterProvider::new(
            vec![(
                "default".into(),
                Box::new(Arc::clone(&mock)) as Box<dyn Provider>,
            )],
            vec![],
            "model".into(),
        );

        let result = router
            .chat_with_system(Some("system"), "hello", "model", 0.5)
            .await
            .unwrap();
        assert_eq!(result, "response");
        assert_eq!(mock.call_count(), 1);
    }

    #[tokio::test]
    async fn chat_with_tools_delegates_to_resolved_provider() {
        let mock = Arc::new(MockProvider::new("tool-response"));
        let router = RouterProvider::new(
            vec![(
                "default".into(),
                Box::new(Arc::clone(&mock)) as Box<dyn Provider>,
            )],
            vec![],
            "model".into(),
        );

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "use tools".to_string(),
        }];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run shell command",
                "parameters": {}
            }
        })];

        // chat_with_tools should delegate through the router to the mock.
        // MockProvider's default chat_with_tools calls chat_with_history -> chat_with_system.
        let result = router
            .chat_with_tools(&messages, &tools, "model", 0.7)
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("tool-response"));
        assert_eq!(mock.call_count(), 1);
        assert_eq!(mock.last_model(), "model");
    }

    #[tokio::test]
    async fn chat_with_tools_routes_hint_correctly() {
        let (router, mocks) = make_router(
            vec![("fast", "fast-tool"), ("smart", "smart-tool")],
            vec![("reasoning", "smart", "claude-opus")],
        );

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "reason about this".to_string(),
        }];
        let tools = vec![serde_json::json!({"type": "function", "function": {"name": "test"}})];

        let result = router
            .chat_with_tools(&messages, &tools, "hint:reasoning", 0.5)
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("smart-tool"));
        assert_eq!(mocks[1].call_count(), 1);
        assert_eq!(mocks[1].last_model(), "claude-opus");
        assert_eq!(mocks[0].call_count(), 0);
    }

    /// The guard refuses a hint and names the table that is missing.
    #[tokio::test]
    async fn the_guard_refuses_a_hint_and_names_the_missing_table() {
        let mock = Arc::new(MockProvider::new("response"));
        let guard = HintGuard::new(Box::new(Arc::clone(&mock)) as Box<dyn Provider>);

        let error = guard
            .simple_chat("hello", "hint:fast", 0.5)
            .await
            .expect_err("a hint must be refused without a route table");
        let message = error.to_string();
        assert!(message.contains(NO_ROUTES_REFUSAL), "{message}");
        assert!(message.contains("fast"), "{message}");
        assert_eq!(mock.call_count(), 0, "no request was sent");
    }

    /// A plain model still reaches the wrapped provider.
    #[tokio::test]
    async fn the_guard_passes_a_plain_model_to_the_wrapped_provider() {
        let mock = Arc::new(MockProvider::new("response"));
        let guard = HintGuard::new(Box::new(Arc::clone(&mock)) as Box<dyn Provider>);

        let result = guard.simple_chat("hello", "gpt-4o", 0.5).await.unwrap();
        assert_eq!(result, "response");
        assert_eq!(mock.call_count(), 1);
        assert_eq!(mock.last_model(), "gpt-4o");
    }

    /// The guard keeps the wrapped provider's capabilities: a wrapper that
    /// reported the defaults would take native tool calling away from the
    /// door it wraps.
    #[tokio::test]
    async fn the_guard_keeps_the_capabilities_of_the_wrapped_provider() {
        let mock = Arc::new(MockProvider::new("response"));
        let guard = HintGuard::new(Box::new(Arc::clone(&mock)) as Box<dyn Provider>);

        assert_eq!(
            guard.capabilities(),
            ProviderCapabilities {
                native_tool_calling: true,
                vision: true,
            }
        );
        assert!(guard.supports_native_tools());
        assert!(guard.supports_vision());
    }

    /// The tool payload of the wrapped provider is the one that is sent.
    #[tokio::test]
    async fn the_guard_keeps_the_tool_payload_of_the_wrapped_provider() {
        let mock = Arc::new(MockProvider::new("response"));
        let guard = HintGuard::new(Box::new(Arc::clone(&mock)) as Box<dyn Provider>);

        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        }];

        match guard.convert_tools(&tools) {
            ToolsPayload::OpenAI { tools: sent } => {
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0]["name"], "shell");
            }
            other => panic!("the wrapped provider's payload is expected, got {other:?}"),
        }
    }
}
