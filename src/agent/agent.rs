use crate::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher, XmlToolDispatcher};
use crate::agent::events::{AgentEvent, AgentEventSender, TurnResult};
use crate::agent::memory_loader::{DefaultMemoryLoader, MemoryLoader};
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::config::Config;
use crate::cost::TokenUsage;
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, ChatRequest, ConversationMessage, Provider};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool, ToolSpec};
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    tool_specs: Vec<ToolSpec>,
    memory: Arc<dyn Memory>,
    observer: Arc<dyn Observer>,
    prompt_builder: SystemPromptBuilder,
    tool_dispatcher: Box<dyn ToolDispatcher>,
    memory_loader: Box<dyn MemoryLoader>,
    config: crate::config::AgentConfig,
    model_name: String,
    temperature: f64,
    workspace_dir: std::path::PathBuf,
    /// Shared security policy handle. Held on the agent so external
    /// callers (TUI slash commands, channel reply parsers) can mutate
    /// the runtime allowlist without re-deriving the policy from
    /// config. `None` for agents constructed via the bare builder
    /// (tests, custom embeds); always `Some` after `from_config`.
    security: Option<Arc<SecurityPolicy>>,
    /// MCP server health snapshot taken during `from_config`. Used
    /// by the TUI's `/mcp` slash command to show which servers
    /// connected vs. failed without re-probing.
    mcp_health: Vec<crate::mcp::discover::McpServerHealth>,
    /// Per-server qualified-tool-name list captured from MCP
    /// discovery. Mirrors `mcp_health` but is keyed by server name
    /// for `/mcp` rendering.
    mcp_tools_by_server: std::collections::HashMap<String, Vec<String>>,
    identity_config: crate::config::IdentityConfig,
    skills: Vec<crate::skills::Skill>,
    skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
    auto_save: bool,
    history: Vec<ConversationMessage>,
    classification_config: crate::config::QueryClassificationConfig,
    available_hints: Vec<String>,
    /// Conversation scope for layered memory. `None` (default) stores and
    /// recalls turn memory globally — prior behavior. When set, this agent's
    /// turn memory is stored under and recalled from this conversation id
    /// (via `recall_layered`), so distinct conversations don't bleed context.
    conversation_id: Option<String>,
    /// Optional Layer-A tool-approval gate. `None` (default — TUI / `agent run`)
    /// means tools are not gated here (the shell tool's own `PendingApprovals`
    /// still applies). The console SSE surface sets this so non-read-only tools
    /// require an in-browser decision via `approval_backend`.
    approval_manager: Option<Arc<crate::approval::ApprovalManager>>,
    /// Inline approval backend used when `approval_manager` gates a call.
    /// `None` falls back to the CLI prompt for the `cli` channel. The console
    /// sets a `WebModalApprovalBackend`.
    approval_backend: Option<Arc<dyn crate::approval::ApprovalBackend>>,
}

pub struct AgentBuilder {
    provider: Option<Box<dyn Provider>>,
    tools: Option<Vec<Box<dyn Tool>>>,
    memory: Option<Arc<dyn Memory>>,
    observer: Option<Arc<dyn Observer>>,
    prompt_builder: Option<SystemPromptBuilder>,
    tool_dispatcher: Option<Box<dyn ToolDispatcher>>,
    memory_loader: Option<Box<dyn MemoryLoader>>,
    config: Option<crate::config::AgentConfig>,
    model_name: Option<String>,
    temperature: Option<f64>,
    workspace_dir: Option<std::path::PathBuf>,
    identity_config: Option<crate::config::IdentityConfig>,
    skills: Option<Vec<crate::skills::Skill>>,
    skills_prompt_mode: Option<crate::config::SkillsPromptInjectionMode>,
    auto_save: Option<bool>,
    classification_config: Option<crate::config::QueryClassificationConfig>,
    available_hints: Option<Vec<String>>,
    conversation_id: Option<String>,
    approval_manager: Option<Arc<crate::approval::ApprovalManager>>,
    approval_backend: Option<Arc<dyn crate::approval::ApprovalBackend>>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            provider: None,
            tools: None,
            memory: None,
            observer: None,
            prompt_builder: None,
            tool_dispatcher: None,
            memory_loader: None,
            config: None,
            model_name: None,
            temperature: None,
            workspace_dir: None,
            identity_config: None,
            skills: None,
            skills_prompt_mode: None,
            auto_save: None,
            classification_config: None,
            available_hints: None,
            conversation_id: None,
            approval_manager: None,
            approval_backend: None,
        }
    }

    /// Gate non-read-only tools through `manager`, deciding each via `backend`
    /// (e.g. the console's web-modal). Both default to `None` (no Layer-A gate),
    /// preserving TUI / `agent run` behavior.
    pub fn approval(
        mut self,
        manager: Option<Arc<crate::approval::ApprovalManager>>,
        backend: Option<Arc<dyn crate::approval::ApprovalBackend>>,
    ) -> Self {
        self.approval_manager = manager;
        self.approval_backend = backend;
        self
    }

    /// Scope this agent's turn memory to a conversation id (layered memory).
    /// Omit for the default global behavior.
    pub fn conversation_id(mut self, conversation_id: Option<String>) -> Self {
        self.conversation_id = conversation_id;
        self
    }

    pub fn provider(mut self, provider: Box<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn tools(mut self, tools: Vec<Box<dyn Tool>>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn prompt_builder(mut self, prompt_builder: SystemPromptBuilder) -> Self {
        self.prompt_builder = Some(prompt_builder);
        self
    }

    pub fn tool_dispatcher(mut self, tool_dispatcher: Box<dyn ToolDispatcher>) -> Self {
        self.tool_dispatcher = Some(tool_dispatcher);
        self
    }

    pub fn memory_loader(mut self, memory_loader: Box<dyn MemoryLoader>) -> Self {
        self.memory_loader = Some(memory_loader);
        self
    }

    pub fn config(mut self, config: crate::config::AgentConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn model_name(mut self, model_name: String) -> Self {
        self.model_name = Some(model_name);
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn workspace_dir(mut self, workspace_dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    pub fn identity_config(mut self, identity_config: crate::config::IdentityConfig) -> Self {
        self.identity_config = Some(identity_config);
        self
    }

    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn skills_prompt_mode(
        mut self,
        skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
    ) -> Self {
        self.skills_prompt_mode = Some(skills_prompt_mode);
        self
    }

    pub fn auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = Some(auto_save);
        self
    }

    pub fn classification_config(
        mut self,
        classification_config: crate::config::QueryClassificationConfig,
    ) -> Self {
        self.classification_config = Some(classification_config);
        self
    }

    pub fn available_hints(mut self, available_hints: Vec<String>) -> Self {
        self.available_hints = Some(available_hints);
        self
    }

    pub fn build(self) -> Result<Agent> {
        let tools = self
            .tools
            .ok_or_else(|| anyhow::anyhow!("tools are required"))?;
        let tool_specs = tools.iter().map(|tool| tool.spec()).collect();

        Ok(Agent {
            provider: self
                .provider
                .ok_or_else(|| anyhow::anyhow!("provider is required"))?,
            tools,
            tool_specs,
            memory: self
                .memory
                .ok_or_else(|| anyhow::anyhow!("memory is required"))?,
            observer: self
                .observer
                .ok_or_else(|| anyhow::anyhow!("observer is required"))?,
            prompt_builder: self
                .prompt_builder
                .unwrap_or_else(SystemPromptBuilder::with_defaults),
            tool_dispatcher: self
                .tool_dispatcher
                .ok_or_else(|| anyhow::anyhow!("tool_dispatcher is required"))?,
            memory_loader: self
                .memory_loader
                .unwrap_or_else(|| Box::new(DefaultMemoryLoader::default())),
            config: self.config.unwrap_or_default(),
            model_name: self
                .model_name
                .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".into()),
            temperature: self.temperature.unwrap_or(0.7),
            workspace_dir: self
                .workspace_dir
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
            security: None,
            mcp_health: Vec::new(),
            mcp_tools_by_server: std::collections::HashMap::new(),
            identity_config: self.identity_config.unwrap_or_default(),
            skills: self.skills.unwrap_or_default(),
            skills_prompt_mode: self.skills_prompt_mode.unwrap_or_default(),
            auto_save: self.auto_save.unwrap_or(false),
            history: Vec::new(),
            classification_config: self.classification_config.unwrap_or_default(),
            available_hints: self.available_hints.unwrap_or_default(),
            conversation_id: self.conversation_id,
            approval_manager: self.approval_manager,
            approval_backend: self.approval_backend,
        })
    }
}

/// Placeholder `TokenUsage` for turns that do not yet track real usage.
///
/// `Agent::turn_streaming` must emit a `Usage` event before `Done`. Until the
/// inline loop wires real token accounting from provider responses this helper
/// produces a zero-valued record scoped to the effective model name.
fn empty_usage(model: &str) -> TokenUsage {
    TokenUsage::new(model.to_string(), 0, 0, 0.0, 0.0)
}

/// Read `<policy_dir>/command_allowlist.toml` into a flat Vec of glob
/// patterns. Used by `build_system_prompt` to surface the pre-approved
/// command list to the model so it knows what will pass without
/// prompting. Returns `Err` only on TOML parse failure; missing file
/// → `Ok(empty)` so a fresh profile (no policy provisioned yet) gets
/// the generic safety section instead of a hard error.
fn read_command_allowlist(policy_dir: &std::path::Path) -> Result<Vec<String>> {
    let path = policy_dir.join("command_allowlist.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let table: toml::value::Table = toml::from_str(&raw)?;
    let patterns = table
        .get("command_allowlist")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("patterns"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(patterns)
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn history(&self) -> &[ConversationMessage] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Seed the agent with prior conversation turns so the model remembers
    /// an earlier exchange — used when resuming a session (`/resume`) or
    /// continuing a web/channel conversation. Pass `(role, content)` turns
    /// in order (role = "user"/"assistant"/"system", default user); the
    /// system prompt is rebuilt and placed first when the prior turns don't
    /// already carry one, so the next `turn()` doesn't append a system
    /// prompt out of order. Takes primitives (not `ConversationMessage`) so
    /// callers across the lib/bin boundary stay type-compatible.
    pub fn restore_history(&mut self, prior: &[(String, String)]) -> Result<()> {
        self.history.clear();
        if prior.is_empty() {
            return Ok(());
        }
        let has_system = prior.iter().any(|(role, _)| role == "system");
        if !has_system {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }
        for (role, content) in prior {
            let chat = match role.as_str() {
                "system" => ChatMessage::system(content),
                "assistant" => ChatMessage::assistant(content),
                _ => ChatMessage::user(content),
            };
            self.history.push(ConversationMessage::Chat(chat));
        }
        Ok(())
    }

    pub async fn from_config(config: &Config) -> Result<Self> {
        let observer: Arc<dyn Observer> =
            Arc::from(observability::create_observer(&config.observability));
        let runtime: Arc<dyn runtime::RuntimeAdapter> =
            Arc::from(runtime::create_runtime(&config.runtime)?);
        let policy_dir = crate::profile::ProfileManager::active()
            .ok()
            .map(|p| p.policy_dir());
        let security = Arc::new(SecurityPolicy::from_config_with_policy_dir(
            &config.autonomy,
            &config.workspace_dir,
            policy_dir,
        ));
        // Bind the async-approval registry to the policy so the shell
        // tool can ask the user (via whichever UI is subscribed) when
        // it hits an allowlist miss in Supervised mode.
        let pending = Arc::new(crate::security::PendingApprovals::default());
        security.set_pending(pending);

        let memory: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes(
            &config.memory,
            &config.embedding_routes,
            Some(&config.storage.provider.config),
            &config.workspace_dir,
            config.api_key.as_deref(),
        )?);

        let composio_key = if config.composio.enabled {
            config.composio.api_key.as_deref()
        } else {
            None
        };
        let composio_entity_id = if config.composio.enabled {
            Some(config.composio.entity_id.as_str())
        } else {
            None
        };

        let mut tools = tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            runtime,
            memory.clone(),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.workspace_dir,
            &config.agents,
            config.api_key.as_deref(),
            config,
        );

        // Strict preset filter: when the active policy is Strict, drop the
        // `shell` tool so the model can't even attempt a call (plan-mode
        // analog). Shared with the channels runtime via `apply_preset_tool_filter`
        // so Strict means the same thing on every surface.
        tools::apply_preset_tool_filter(&mut tools);

        // MCP discovery — spawn each configured server, query
        // `tools/list`, splice each tool into the registry as an
        // `McpTool`. Failures are non-fatal (logged); the agent
        // boots without the broken server's tools, and `/mcp`
        // surfaces what happened.
        let mcp_discovery = crate::mcp::discover::discover_mcp_tools(&config.mcp_servers).await;
        // Build the per-server qualified-tool-name map before the
        // tools are moved into the registry.
        let mut mcp_tools_by_server: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for t in &mcp_discovery.tools {
            // Tool name is `mcp__<server>__<tool>` — split to find server.
            let name = t.name();
            if let Some(rest) = name.strip_prefix("mcp__") {
                if let Some((server, _)) = rest.split_once("__") {
                    mcp_tools_by_server
                        .entry(server.to_string())
                        .or_default()
                        .push(name.to_string());
                }
            }
        }
        let mcp_health = mcp_discovery.health.clone();
        if !mcp_discovery.tools.is_empty() {
            tracing::info!(
                target: "agent",
                count = mcp_discovery.tools.len(),
                servers = mcp_discovery.health.len(),
                "appending MCP tools to registry"
            );
            tools.extend(mcp_discovery.tools);
        }

        let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");

        let model_name = config
            .default_model
            .as_deref()
            .unwrap_or("anthropic/claude-sonnet-4-20250514")
            .to_string();

        let provider: Box<dyn Provider> = providers::create_routed_provider(
            provider_name,
            config.api_key.as_deref(),
            config.api_url.as_deref(),
            &config.reliability,
            &config.model_routes,
            &model_name,
        )?;

        let dispatcher_choice = config.agent.tool_dispatcher.as_str();
        let tool_dispatcher: Box<dyn ToolDispatcher> = match dispatcher_choice {
            "native" => Box::new(NativeToolDispatcher),
            "xml" => Box::new(XmlToolDispatcher),
            _ if provider.supports_native_tools() => Box::new(NativeToolDispatcher),
            _ => Box::new(XmlToolDispatcher),
        };

        let available_hints: Vec<String> =
            config.model_routes.iter().map(|r| r.hint.clone()).collect();

        Agent::builder()
            .provider(provider)
            .tools(tools)
            .memory(memory)
            .observer(observer)
            .tool_dispatcher(tool_dispatcher)
            .memory_loader(Box::new(DefaultMemoryLoader::new(
                5,
                config.memory.min_relevance_score,
            )))
            .prompt_builder(SystemPromptBuilder::with_defaults())
            .config(config.agent.clone())
            .model_name(model_name)
            .temperature(config.default_temperature)
            .workspace_dir(config.workspace_dir.clone())
            .classification_config(config.query_classification.clone())
            .available_hints(available_hints)
            .identity_config(config.identity.clone())
            .skills(crate::skills::load_skills_with_config(
                &config.workspace_dir,
                config,
            ))
            .skills_prompt_mode(config.skills.prompt_injection_mode)
            .auto_save(config.memory.auto_save)
            .build()
            .map(|mut agent| {
                agent.security = Some(security);
                agent.mcp_health = mcp_health;
                agent.mcp_tools_by_server = mcp_tools_by_server;
                agent
            })
    }

    /// Inject (or clear) the Layer-A tool-approval gate after construction.
    /// `from_config` leaves these `None`; the console SSE surface calls this to
    /// gate non-read-only tools through an in-browser `WebModalApprovalBackend`.
    pub fn set_approval(
        &mut self,
        manager: Option<Arc<crate::approval::ApprovalManager>>,
        backend: Option<Arc<dyn crate::approval::ApprovalBackend>>,
    ) {
        self.approval_manager = manager;
        self.approval_backend = backend;
    }

    /// Shared security policy handle — `Some` when the agent was built
    /// via [`Agent::from_config`], `None` for bare-builder agents
    /// (tests/custom embeds). Use this to mutate the runtime allowlist
    /// or resolve pending approvals from outside the agent loop.
    pub fn security(&self) -> Option<Arc<SecurityPolicy>> {
        self.security.clone()
    }

    /// Shared memory backend handle. Always populated — the agent
    /// always has a memory store (defaulting to the `None` no-op
    /// backend if none was configured). Use this to drive the user-
    /// facing `/memory`, `/forget`, etc. slash commands from outside
    /// the agent loop.
    pub fn memory_handle(&self) -> Arc<dyn Memory> {
        self.memory.clone()
    }

    /// MCP server health snapshot from boot. Empty for bare-builder
    /// agents or when no `[mcp_servers.*]` blocks were configured.
    pub fn mcp_health(&self) -> &[crate::mcp::discover::McpServerHealth] {
        &self.mcp_health
    }

    /// Per-server live MCP tool registry (qualified names). Cloned so
    /// callers can store/own it independently.
    pub fn mcp_tools_by_server(&self) -> std::collections::HashMap<String, Vec<String>> {
        self.mcp_tools_by_server.clone()
    }

    /// Compact older turns into a structured-markdown summary. Replaces
    /// the agent's in-memory conversation history with
    /// `[system_prompt, system(summary_envelope), ...recent]` so the
    /// next turn picks up cleanly with most of the context budget
    /// freed.
    ///
    /// `keep_last` is the number of *user* turns to preserve verbatim
    /// at the tail. Saturates to a minimum of 1.
    ///
    /// Streams the summary text through `events` as `Chunk` packets
    /// (so the TUI sees it appear in scrollback like a normal reply),
    /// then emits `CompactionComplete` with counts.
    ///
    /// Returns an `Err` if the history is too short to compact (fewer
    /// than `keep_last + 1` user messages). The caller's TUI shows
    /// that error verbatim — it's user-facing, not a panic path.
    pub async fn compact_streaming(
        &mut self,
        keep_last: usize,
        events: Option<AgentEventSender>,
    ) -> Result<crate::agent::compaction::CompactionResult> {
        use crate::agent::compaction::{
            build_side_request, compute_split_index, stream_summary_as_chunks, summary_envelope,
            CompactionResult,
        };

        let keep_last = keep_last.max(1);
        let original_count = self.history.len();

        let split_idx = compute_split_index(&self.history, keep_last).ok_or_else(|| {
            anyhow::anyhow!(
                "Nothing to compact yet — need more than {keep_last} user turn(s) in history."
            )
        })?;

        if let Some(tx) = events.as_ref() {
            let _ = tx
                .send(AgentEvent::CompactionStart {
                    original_count,
                    keep_last,
                })
                .await;
        }

        let to_compact: Vec<ConversationMessage> = self.history[..split_idx].to_vec();
        let side_messages = build_side_request(&to_compact);

        let observer_started = Instant::now();
        self.observer.record_event(&ObserverEvent::LlmRequest {
            provider: "compaction".to_string(),
            model: self.model_name.clone(),
            messages_count: side_messages.len(),
        });

        // Tools-disabled side call. Mirrors `force_final_summary` in
        // `loop_.rs` — same shape, different prompt.
        let resp = self
            .provider
            .chat(
                ChatRequest {
                    messages: &side_messages,
                    tools: None,
                },
                &self.model_name,
                self.temperature,
            )
            .await;

        let resp = match resp {
            Ok(r) => {
                self.observer.record_event(&ObserverEvent::LlmResponse {
                    provider: "compaction".to_string(),
                    model: self.model_name.clone(),
                    duration: observer_started.elapsed(),
                    success: true,
                    error_message: None,
                });
                r
            }
            Err(e) => {
                self.observer.record_event(&ObserverEvent::LlmResponse {
                    provider: "compaction".to_string(),
                    model: self.model_name.clone(),
                    duration: observer_started.elapsed(),
                    success: false,
                    error_message: Some(providers::sanitize_api_error(&e.to_string())),
                });
                if let Some(tx) = events.as_ref() {
                    let _ = tx
                        .send(AgentEvent::Error(format!("compaction failed: {e}")))
                        .await;
                }
                return Err(e);
            }
        };

        let summary = resp.text_or_empty().to_string();
        if summary.trim().is_empty() {
            if let Some(tx) = events.as_ref() {
                let _ = tx
                    .send(AgentEvent::Error(
                        "compaction failed: provider returned empty summary".into(),
                    ))
                    .await;
            }
            return Err(anyhow::anyhow!(
                "Compaction failed: provider returned empty summary."
            ));
        }

        // Stream the summary into scrollback so the user sees what
        // got produced — same UX as a regular streaming response.
        stream_summary_as_chunks(&summary, events.as_ref()).await;

        // Replace history: keep the original system prompt + insert
        // the wrapped summary right after it, then re-append the
        // tail we preserved.
        let mut kept: Vec<ConversationMessage> = self.history[split_idx..].to_vec();
        let mut original_system: Vec<ConversationMessage> = self
            .history
            .iter()
            .filter(|m| matches!(m, ConversationMessage::Chat(c) if c.role == "system"))
            .cloned()
            .collect();
        self.history.clear();
        self.history.append(&mut original_system);
        self.history
            .push(ConversationMessage::Chat(summary_envelope(&summary)));
        self.history.append(&mut kept);

        let kept_count = self.history.len();

        if let Some(tx) = events.as_ref() {
            let _ = tx
                .send(AgentEvent::CompactionComplete {
                    summary: summary.clone(),
                    original_count,
                    keep_last,
                    kept_count,
                })
                .await;
        }

        Ok(CompactionResult {
            summary,
            original_count,
            kept_count,
        })
    }

    fn trim_history(&mut self) {
        let max = self.config.max_history_messages;
        if self.history.len() <= max {
            return;
        }

        let mut system_messages = Vec::new();
        let mut other_messages = Vec::new();

        for msg in self.history.drain(..) {
            match &msg {
                ConversationMessage::Chat(chat) if chat.role == "system" => {
                    system_messages.push(msg);
                }
                _ => other_messages.push(msg),
            }
        }

        if other_messages.len() > max {
            let drop_count = other_messages.len() - max;
            other_messages.drain(0..drop_count);
        }

        self.history = system_messages;
        self.history.extend(other_messages);
    }

    fn build_system_prompt(&self) -> Result<String> {
        let instructions = self.tool_dispatcher.prompt_instructions(&self.tools);

        // Resolve the active preset + its on-disk command_allowlist so
        // the SafetySection can render preset-specific guidance + the
        // exact pre-approved patterns. Both come from the active
        // profile's policy dir. Failure to read either is non-fatal —
        // we fall back to None / empty and the safety section renders
        // its generic floor.
        let (autonomy_preset, allowed_commands) = match crate::profile::ProfileManager::active() {
            Ok(profile) => {
                let dir = profile.policy_dir();
                let preset = crate::approval::policy_writer::read_active_preset(&dir);
                let allowlist = read_command_allowlist(&dir).unwrap_or_default();
                (preset, allowlist)
            }
            Err(_) => (None, Vec::new()),
        };

        let ctx = PromptContext {
            workspace_dir: &self.workspace_dir,
            model_name: &self.model_name,
            surface: crate::agent::prompt::PromptSurface::Agent,
            bootstrap_max_chars: 20_000,
            tools: &self.tools,
            skills: &self.skills,
            skills_prompt_mode: self.skills_prompt_mode,
            identity_config: Some(&self.identity_config),
            dispatcher_instructions: &instructions,
            autonomy_preset,
            allowed_commands: &allowed_commands,
        };
        self.prompt_builder.build(&ctx)
    }

    fn classify_model(&self, user_message: &str) -> String {
        if let Some(hint) = super::classifier::classify(&self.classification_config, user_message) {
            if self.available_hints.contains(&hint) {
                tracing::info!(hint = hint.as_str(), "Auto-classified query");
                return format!("hint:{hint}");
            }
        }
        self.model_name.clone()
    }

    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        self.turn_streaming(user_message, None, None)
            .await
            .map(|r| r.text)
    }

    /// Execute a single agent turn with optional structured event streaming and
    /// cancellation support.
    ///
    /// Event ordering invariants (when `events` is `Some`):
    ///   * `Chunk` events (if any) precede `Usage`.
    ///   * On success: `Usage` immediately precedes `Done { cancelled: false }`.
    ///   * On error: `Error(msg)` precedes `Done { cancelled: false, final_text: "" }`.
    ///   * On cancellation: `Done { cancelled: true }` fires with whatever
    ///     partial text has been produced.
    ///   * `Done` fires exactly once per call.
    ///
    /// History is preserved across early-exit paths — any tool results or
    /// partial assistant text already appended remain intact.
    pub async fn turn_streaming(
        &mut self,
        user_message: &str,
        events: Option<AgentEventSender>,
        cancel: Option<CancellationToken>,
    ) -> Result<TurnResult> {
        let result = self
            .turn_inner(user_message, events.as_ref(), cancel.as_ref())
            .await;

        // Emit terminal events exactly once, regardless of outcome.
        match &result {
            Ok(tr) => {
                if let Some(tx) = events.as_ref() {
                    let _ = tx.send(AgentEvent::Usage(tr.usage.clone())).await;
                    let _ = tx
                        .send(AgentEvent::Done {
                            final_text: tr.text.clone(),
                            cancelled: tr.cancelled,
                        })
                        .await;
                }
            }
            Err(err) => {
                if let Some(tx) = events.as_ref() {
                    let _ = tx.send(AgentEvent::Error(err.to_string())).await;
                    let _ = tx
                        .send(AgentEvent::Done {
                            final_text: String::new(),
                            cancelled: false,
                        })
                        .await;
                }
            }
        }

        result
    }

    async fn turn_inner(
        &mut self,
        user_message: &str,
        events: Option<&AgentEventSender>,
        cancel: Option<&CancellationToken>,
    ) -> Result<TurnResult> {
        if self.history.is_empty() {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }

        // Store and recall turn memory under this agent's conversation scope.
        // `None` (default) keeps the prior global behavior; when set, write-side
        // (`store`) and read-side (`recall_layered`) agree so a conversation's
        // memory is isolated yet still backfilled from shared/global memory.
        let conversation_scope = self.conversation_id.as_deref();

        if self.auto_save {
            let _ = self
                .memory
                .store(
                    "user_msg",
                    user_message,
                    MemoryCategory::Conversation,
                    conversation_scope,
                )
                .await;
        }

        // Loader routes through recall_layered with the same conversation scope
        // used for writes above, so reads and writes stay consistent.
        // ready for when write-side scoping threads a conversation_id through.
        let context = self
            .memory_loader
            .load_context(self.memory.as_ref(), user_message, conversation_scope)
            .await
            .unwrap_or_default();

        let enriched = if context.is_empty() {
            user_message.to_string()
        } else {
            format!("{context}{user_message}")
        };

        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(enriched)));

        let effective_model = self.classify_model(user_message);

        // Drive the ONE shared agentic loop (PR2). The Agent already keeps
        // structured `ConversationMessage` history, so it passes it (and its
        // dispatcher) straight through — no conversion. `approval_manager` is
        // `None` for the interactive agent (it gates via the shell relay, not a
        // Layer-A manager); the console SSE surface injects a manager + a
        // web-modal backend so non-read-only tools require an in-browser
        // decision. Streaming goes through `events`.
        let result = crate::agent::loop_::run_structured_loop(
            self.provider.as_ref(),
            &mut self.history,
            self.tool_dispatcher.as_ref(),
            &self.tools,
            self.observer.as_ref(),
            "agent",
            &effective_model,
            self.temperature,
            true,
            self.approval_manager.as_deref(),
            "cli",
            self.approval_backend.as_deref(),
            &crate::config::MultimodalConfig::default(),
            self.config.max_tool_iterations,
            cancel.cloned(),
            None,
            events.cloned(),
        )
        .await;

        self.trim_history();

        match result {
            Ok(text) => Ok(TurnResult {
                text,
                usage: empty_usage(&effective_model),
                cancelled: false,
            }),
            // The shared loop signals cancellation via `ToolLoopCancelled`.
            Err(e)
                if e.downcast_ref::<crate::agent::loop_::ToolLoopCancelled>()
                    .is_some() =>
            {
                Ok(TurnResult {
                    text: String::new(),
                    usage: empty_usage(&effective_model),
                    cancelled: true,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub async fn run_single(&mut self, message: &str) -> Result<String> {
        self.turn(message).await
    }

    pub async fn run_interactive(&mut self) -> Result<()> {
        println!("🦀 RantaiClaw Interactive Mode");
        println!("Type /quit to exit.\n");

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let cli = crate::channels::CliChannel::new();

        let listen_handle = tokio::spawn(async move {
            let _ = crate::channels::Channel::listen(
                &cli,
                tx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        });

        while let Some(msg) = rx.recv().await {
            let response = match self.turn(&msg.content).await {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("\nError: {e}\n");
                    continue;
                }
            };
            println!("\n{response}\n");
        }

        listen_handle.abort();
        Ok(())
    }
}

pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
) -> Result<()> {
    let start = Instant::now();

    let mut effective_config = config;
    if let Some(p) = provider_override {
        effective_config.default_provider = Some(p);
    }
    if let Some(m) = model_override {
        effective_config.default_model = Some(m);
    }
    effective_config.default_temperature = temperature;

    let mut agent = Agent::from_config(&effective_config).await?;

    let provider_name = effective_config
        .default_provider
        .as_deref()
        .unwrap_or("openrouter")
        .to_string();
    let model_name = effective_config
        .default_model
        .as_deref()
        .unwrap_or("anthropic/claude-sonnet-4-20250514")
        .to_string();

    agent.observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.clone(),
        model: model_name.clone(),
    });

    if let Some(msg) = message {
        let response = agent.run_single(&msg).await?;
        println!("{response}");
    } else {
        agent.run_interactive().await?;
    }

    agent.observer.record_event(&ObserverEvent::AgentEnd {
        provider: provider_name,
        model: model_name,
        duration: start.elapsed(),
        tokens_used: None,
        cost_usd: None,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    struct MockProvider {
        responses: Mutex<Vec<crate::providers::ChatResponse>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> Result<crate::providers::ChatResponse> {
            let mut guard = self.responses.lock();
            if guard.is_empty() {
                return Ok(crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                });
            }
            Ok(guard.remove(0))
        }
    }

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "echo"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "tool-out".into(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn turn_without_tools_returns_text() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![crate::providers::ChatResponse {
                text: Some("hello".into()),
                tool_calls: vec![],
            }]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "hello");
    }

    #[tokio::test]
    async fn turn_with_native_dispatcher_handles_tool_results_variant() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some(String::new()),
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "tc1".into(),
                        name: "echo".into(),
                        arguments: "{}".into(),
                    }],
                },
                crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                },
            ]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "done");
        assert!(agent
            .history()
            .iter()
            .any(|msg| matches!(msg, ConversationMessage::ToolResults(_))));
    }

    /// Build a minimal `Agent` whose mock provider returns a single text
    /// response with the given body. Shared by the streaming/delegation tests.
    fn build_test_agent(text: &str) -> Agent {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![crate::providers::ChatResponse {
                text: Some(text.to_string()),
                tool_calls: vec![],
            }]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config")
    }

    #[tokio::test]
    async fn turn_streaming_emits_done_with_final_text() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
        let mut agent = build_test_agent("hello");

        let result = agent
            .turn_streaming("hi", Some(events_tx), None)
            .await
            .unwrap();
        assert_eq!(result.text, "hello");
        assert!(!result.cancelled);

        // Drop the sender implicitly by letting it go out of scope — it was
        // moved into `turn_streaming` and released on return. Drain the rx
        // with try_recv until empty.
        let mut saw_done = false;
        let mut saw_usage_before_done = false;
        let mut saw_usage = false;
        while let Ok(ev) = events_rx.try_recv() {
            match ev {
                AgentEvent::Usage(_) => saw_usage = true,
                AgentEvent::Done {
                    final_text,
                    cancelled,
                } => {
                    assert_eq!(final_text, "hello");
                    assert!(!cancelled);
                    assert!(!saw_done, "Done must fire exactly once");
                    saw_usage_before_done = saw_usage;
                    saw_done = true;
                }
                _ => {}
            }
        }
        assert!(saw_done, "expected Done event");
        assert!(saw_usage_before_done, "Usage must precede Done on success");
    }

    #[tokio::test]
    async fn turn_delegates_to_turn_streaming() {
        let mut agent = build_test_agent("delegated");
        let text = agent.turn("hi").await.unwrap();
        assert_eq!(text, "delegated");
    }

    #[tokio::test]
    async fn turn_streaming_cancellation_yields_done_cancelled_true() {
        use tokio::time::{sleep, Duration};

        // SlowProvider hangs 200ms in chat() so cancellation has time to fire.
        struct SlowProvider;

        #[async_trait]
        impl Provider for SlowProvider {
            async fn chat_with_system(
                &self,
                _system_prompt: Option<&str>,
                _message: &str,
                _model: &str,
                _temperature: f64,
            ) -> Result<String> {
                Ok("slow".into())
            }

            async fn chat(
                &self,
                _request: ChatRequest<'_>,
                _model: &str,
                _temperature: f64,
            ) -> Result<crate::providers::ChatResponse> {
                sleep(Duration::from_millis(200)).await;
                Ok(crate::providers::ChatResponse {
                    text: Some("never delivered".into()),
                    tool_calls: vec![],
                })
            }
        }

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(Box::new(SlowProvider))
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config");

        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);

        // Fire cancel after 50ms (before provider delivers at 200ms).
        tokio::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let result = agent
            .turn_streaming("hi", Some(events_tx), Some(cancel))
            .await;
        let result = result.expect("turn_streaming returns Ok on cancel path");
        assert!(result.cancelled, "expected cancelled=true");
        assert!(
            result.text.is_empty(),
            "expected empty text on cancellation"
        );

        // Verify Done { cancelled: true } appeared in the event stream.
        let mut saw_cancelled_done = false;
        while let Ok(ev) = events_rx.try_recv() {
            if let AgentEvent::Done {
                cancelled: true, ..
            } = ev
            {
                saw_cancelled_done = true;
            }
        }
        assert!(
            saw_cancelled_done,
            "expected Done {{ cancelled: true }} event"
        );
    }
}
