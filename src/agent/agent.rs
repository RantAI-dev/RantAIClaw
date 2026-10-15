use crate::agent::dispatcher::{NativeToolDispatcher, ToolDispatcher, XmlToolDispatcher};
use crate::agent::events::{AgentEvent, AgentEventSender, TurnResult};
use crate::agent::memory_loader::{DefaultMemoryLoader, MemoryLoader};
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::config::Config;
use crate::cost::TokenUsage;
use crate::memory::{self, Memory};

#[cfg(test)]
use crate::memory::MemoryCategory;
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, ChatRequest, ConversationMessage, Provider};
use crate::runtime;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool, ToolSpec};
use anyhow::Result;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// The operator has not configured a model, so an agent cannot be built or run.
/// A typed error (rather than a bare `anyhow!`) so callers — notably the gateway
/// `/agent/chat` handler — can map it to a 400 (client must configure) instead of
/// a 500, and so the actionable message lives in exactly one place.
#[derive(Debug)]
pub struct NoModelConfigured;

impl std::fmt::Display for NoModelConfigured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no model is configured. Run `rantaiclaw setup provider`, or pass \
             `--model <id>` / set RANTAICLAW_MODEL. The agent does not assume a default model."
        )
    }
}

impl std::error::Error for NoModelConfigured {}

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
    /// Surface this agent serves, written as the audit `channel` (`tui`,
    /// `console`). `None` means the CLI, which keeps the `cli` value.
    surface: Option<&'static str>,
    /// The primary provider's name from config (`default_provider`). Keys the
    /// rig image allowlist, the same primary the vision gate reads. `None` for
    /// agents built from the bare builder (tests, embeds): no rig restriction.
    primary_provider: Option<String>,
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
    history: Vec<ConversationMessage>,
    classification_config: crate::config::QueryClassificationConfig,
    available_hints: Vec<String>,
    /// Conversation scope for the turn's memory writes. `None` (default)
    /// stores turn memory in the shared tier. When set, this agent's turn
    /// memory is stored under this conversation id. What a turn may read is
    /// the door's [`crate::memory::MemoryView`], not this id.
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
    /// The process's token ledger: the daily ceiling this agent's turns are
    /// checked against, and the operator's optional prices used to report money.
    /// `None` on bare-builder agents (tests, custom embeds); `from_config` sets
    /// it whenever `[cost] enabled` is on.
    ledger: Option<Arc<crate::cost::CostTracker>>,
    /// The `[multimodal]` config the image preparation runs with: the same
    /// config the loop gets, so a turn is judged here as the loop will judge it.
    /// Bare-builder agents keep the defaults; `from_config` sets the user's.
    multimodal: crate::config::MultimodalConfig,
}

/// A message refused before it entered the history, because one of its images
/// cannot be sent. Carried as the error of [`Agent::turn`], so a surface can tell
/// a refusal (nothing was recorded, the text can be edited and sent again) from
/// a turn that failed after it was pushed. `message` is the text the user sees.
#[derive(Debug)]
pub struct ImageRefused {
    pub message: String,
}

impl std::fmt::Display for ImageRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ImageRefused {}

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
    classification_config: Option<crate::config::QueryClassificationConfig>,
    available_hints: Option<Vec<String>>,
    conversation_id: Option<String>,
    security: Option<Arc<SecurityPolicy>>,
    approval_manager: Option<Arc<crate::approval::ApprovalManager>>,
    approval_backend: Option<Arc<dyn crate::approval::ApprovalBackend>>,
    ledger: Option<Arc<crate::cost::CostTracker>>,
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
            classification_config: None,
            available_hints: None,
            conversation_id: None,
            security: None,
            approval_manager: None,
            approval_backend: None,
            ledger: None,
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

    /// Attach a security policy. `from_config` always sets one; the bare
    /// builder does not.
    pub fn security(mut self, security: Arc<SecurityPolicy>) -> Self {
        self.security = Some(security);
        self
    }

    /// Store this agent's turn memory under a conversation id. Omit to store in
    /// the shared tier. What a turn may read is the door's memory view.
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
            security: self.security,
            // The builder carries no surface: the surfaces that name one call
            // [`Agent::set_surface`] after they build the agent.
            surface: None,
            primary_provider: None,
            mcp_health: Vec::new(),
            mcp_tools_by_server: std::collections::HashMap::new(),
            identity_config: self.identity_config.unwrap_or_default(),
            skills: self.skills.unwrap_or_default(),
            skills_prompt_mode: self.skills_prompt_mode.unwrap_or_default(),
            history: Vec::new(),
            classification_config: self.classification_config.unwrap_or_default(),
            available_hints: self.available_hints.unwrap_or_default(),
            conversation_id: self.conversation_id,
            approval_manager: self.approval_manager,
            approval_backend: self.approval_backend,
            ledger: self.ledger,
            multimodal: crate::config::MultimodalConfig::default(),
        })
    }
}

/// Build the turn's `TokenUsage` from what the provider reported, or `None`
/// when it reported nothing.
///
/// Priced only where the operator configured a price for this model. Without a
/// ledger — or without an entry for the model — `cost_usd` stays `None`, which
/// the surfaces render as "not reported". A `0.0` here would be a claim about
/// spend that nothing measured.
fn turn_usage(
    ledger: Option<&crate::cost::CostTracker>,
    model: &str,
    reported: Option<crate::providers::ProviderUsage>,
) -> Option<TokenUsage> {
    reported.map(|u| {
        ledger.map_or_else(
            || TokenUsage::new(model.to_string(), u.input_tokens, u.output_tokens, None),
            |l| l.usage_for(model, u.input_tokens, u.output_tokens),
        )
    })
}

/// Read `<policy_dir>/command_allowlist.toml` into a flat Vec of glob
/// patterns. Used by `build_system_prompt` to surface the pre-approved
/// command list to the model so it knows what will pass without
/// prompting. Returns `Err` only on TOML parse failure; missing file
/// → `Ok(empty)` so a fresh profile (no policy provisioned yet) gets
/// the generic safety section instead of a hard error.
///
/// NOTE: these globs are ADVISORY — shown to the model, not enforced. The
/// runtime shell gate matches `config.autonomy.allowed_commands` by BASENAME
/// (`SecurityPolicy::is_command_allowed`), so `git status` here is enforced as
/// "any `git`". Editing this file changes what the model is told, not what the
/// gate allows; mutating subcommands are instead gated by `command_risk_level`.
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
    pub async fn restore_history(&mut self, prior: &[(String, String)]) -> Result<()> {
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
        self.push_prior_turns(prior).await;
        Ok(())
    }

    /// Replace the history with the real system prompt followed by `turns`, in
    /// order. Unlike [`Agent::restore_history`], the prompt is built even when
    /// `turns` is empty, so the result is always the fresh-session shape
    /// `[system prompt, ...turns]`. A turn carrying role `system` is still
    /// sent as given, but the prompt is still built first: a summary is
    /// context, not a prompt.
    pub async fn reseed_history(&mut self, turns: &[(String, String)]) -> Result<()> {
        let system_prompt = self.build_system_prompt()?;
        self.history.clear();
        self.history
            .push(ConversationMessage::Chat(ChatMessage::system(
                system_prompt,
            )));
        self.push_prior_turns(turns).await;
        Ok(())
    }

    /// Carry the conversation of `previous` into this agent, which was just
    /// built from a new config. The history moves as it is (tool calls and
    /// results included), except the system prompt, which this agent's own
    /// prompt replaces: the previous agent's leading system message is dropped
    /// and everything after it, a compaction summary among it, is kept in order.
    pub fn adopt_history_from(&mut self, previous: &mut Agent) -> Result<()> {
        let system_prompt = self.build_system_prompt()?;
        let mut carried = std::mem::take(&mut previous.history);
        if matches!(
            carried.first(),
            Some(ConversationMessage::Chat(chat)) if chat.role == "system"
        ) {
            carried.remove(0);
        }
        self.history.clear();
        self.history
            .push(ConversationMessage::Chat(ChatMessage::system(
                system_prompt,
            )));
        self.history.extend(carried);
        Ok(())
    }

    /// Put the prior turns into the history. Each user turn goes through the
    /// same image preparation a new turn does, judged against the turns before
    /// it. A turn whose image can no longer be prepared keeps its text, and its
    /// marker becomes a placeholder naming the file; the reseed never fails.
    async fn push_prior_turns(&mut self, prior: &[(String, String)]) {
        for (role, content) in prior {
            let chat = match role.as_str() {
                "system" => ChatMessage::system(content),
                "assistant" => ChatMessage::assistant(content),
                _ => {
                    let prepared = self.prepare_prior_user_turn(content).await;
                    ChatMessage::user(prepared)
                }
            };
            self.history.push(ConversationMessage::Chat(chat));
        }
    }

    /// The content a stored user turn is put back with. Prepared exactly as a
    /// new turn would be, so the images are data URIs; a turn whose images
    /// cannot be prepared keeps its text with a placeholder per failed marker.
    async fn prepare_prior_user_turn(&self, content: &str) -> String {
        match self.prepare_new_turn(content).await {
            Ok(prepared) => prepared,
            Err(_) => {
                let (cleaned, refs) = crate::multimodal::parse_image_markers(content);
                let mut text = cleaned;
                for reference in refs {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    let _ = write!(text, "[image not sent: {reference}]");
                }
                text
            }
        }
    }

    /// Build an agent from config with a fresh observer derived from
    /// `config.observability`. Most callers (CLI, channels) want this.
    pub async fn from_config(config: &Config) -> Result<Self> {
        let observer: Arc<dyn Observer> =
            Arc::from(observability::create_observer(&config.observability));
        Self::from_config_with_observer(config, observer).await
    }

    /// Build an agent from config but with a caller-supplied observer. The
    /// gateway passes its shared observer so per-request metrics land in the
    /// same registry `/metrics` exposes, instead of a throwaway per-agent one.
    pub async fn from_config_with_observer(
        config: &Config,
        observer: Arc<dyn Observer>,
    ) -> Result<Self> {
        Self::build(config, observer, None).await
    }

    /// Build an agent whose MCP tools come from an already-connected pool
    /// instead of a fresh round of spawning.
    ///
    /// The gateway builds an agent per chat request; discovering MCP servers
    /// there meant every console turn paid spawn + handshake + `tools/list` for
    /// every server and then SIGKILLed them, losing any server-side state. The
    /// pool's lifetime belongs to the gateway (see [`crate::mcp::discover`]).
    /// Other callers — the TUI, the CLI — keep the owning-agent shape, so
    /// [`Self::from_config_with_observer`] is unchanged.
    pub async fn from_config_with_mcp_pool(
        config: &Config,
        observer: Arc<dyn Observer>,
        pool: &crate::mcp::discover::McpPool,
    ) -> Result<Self> {
        Self::build(config, observer, Some(pool)).await
    }

    async fn build(
        config: &Config,
        observer: Arc<dyn Observer>,
        pool: Option<&crate::mcp::discover::McpPool>,
    ) -> Result<Self> {
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

        // MCP discovery — spawn each configured server, query
        // `tools/list`, splice each tool into the registry as an
        // `McpTool`. Failures are non-fatal (logged); the agent
        // boots without the broken server's tools, and `/mcp`
        // surfaces what happened.
        let (mcp_tools, mcp_health, mcp_tools_by_server) = match pool {
            Some(pool) => (pool.tools(), pool.health().to_vec(), pool.tools_by_server()),
            None => {
                let discovery = crate::mcp::discover::discover_mcp_tools(&config.mcp_servers).await;
                // Build the per-server qualified-tool-name map before the
                // tools are moved into the registry.
                let mut by_server: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for t in &discovery.tools {
                    // Tool name is `mcp__<server>__<tool>` — split to find server.
                    let name = t.name();
                    if let Some(rest) = name.strip_prefix("mcp__") {
                        if let Some((server, _)) = rest.split_once("__") {
                            by_server
                                .entry(server.to_string())
                                .or_default()
                                .push(name.to_string());
                        }
                    }
                }
                (discovery.tools, discovery.health, by_server)
            }
        };
        if !mcp_tools.is_empty() {
            tracing::info!(
                target: "agent",
                count = mcp_tools.len(),
                servers = mcp_health.len(),
                pooled = pool.is_some(),
                "appending MCP tools to registry"
            );
            tools.extend(mcp_tools);
        }

        let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");

        // No model configured → refuse to guess one. A silent hardcoded
        // fallback here is exactly what let a keyless/modelless fresh install
        // look configured; fail fast with a fix-it hint instead.
        let model_name = config
            .default_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .ok_or_else(|| anyhow::Error::new(NoModelConfigured))?
            .to_string();

        // Resolve the key for THIS provider (not blindly the top-level api_key,
        // which belongs to `default_provider`) so a per-request provider
        // override doesn't reuse another provider's credential.
        let provider_credential = config.resolve_key_for_provider(provider_name);
        let provider: Box<dyn Provider> = providers::create_routed_provider(
            provider_name,
            provider_credential.as_deref(),
            config.api_url.as_deref(),
            &config.reliability,
            &config.model_routes,
            &model_name,
            &|name| config.resolve_key_for_provider(name),
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
            .build()
            .map(|mut agent| {
                agent.security = Some(security);
                agent.primary_provider = Some(provider_name.to_string());
                agent.mcp_health = mcp_health;
                agent.mcp_tools_by_server = mcp_tools_by_server;
                // Every agent built from a real config is subject to the daily
                // token ceiling. Bare-builder agents (tests, embeds) are not —
                // they have no workspace to keep a ledger in.
                agent.ledger = crate::cost::ledger_for(config);
                agent.multimodal = config.multimodal.clone();
                agent
            })
    }

    /// Name the surface this agent serves for the audit `channel`. Set after
    /// `from_config` by the surfaces that build their agent through it (TUI,
    /// console), which otherwise would write the CLI's `cli`.
    pub fn set_surface(&mut self, surface: &'static str) {
        self.surface = Some(surface);
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
    /// Point this agent's turn memory writes at a conversation.
    ///
    /// Set per request by surfaces that serve more than one conversation through
    /// one agent, or that only learn the identity after construction. `None`
    /// restores the shared tier.
    pub fn set_conversation_id(&mut self, conversation_id: Option<String>) {
        self.conversation_id = conversation_id;
    }

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
            // Cut forward past any tool result the count-based boundary would
            // have separated from its `AssistantToolCalls`.
            let drop_count = crate::agent::compaction::pairing_safe_cut(
                &other_messages,
                other_messages.len() - max,
            );
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
            // The interactive TUI / CLI / console / cron surface is the
            // owner's path; guest-scoping applies only to channel turns and
            // does not apply here.
            audience: crate::agent::prompt::PromptAudience::Owner,
            // The owner files are memory read into the prompt, so only a turn
            // that sees all of memory carries them. The door sets the view
            // before the turn starts, and this runs inside the turn.
            owner_files: crate::agent::prompt::OwnerFiles::for_view(
                crate::memory::current_memory_view().as_ref(),
            ),
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
    ///   * On a refusal ([`ImageRefused`]): `ImageRefused { message }` takes the
    ///     `Error` slot, and the same `Done` follows — nothing ran, but the
    ///     turn still ended.
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
                    // Only when the provider reported counts. No event means
                    // "not reported"; a zero-valued one would mean "measured
                    // zero", which is what this change exists to stop saying.
                    if let Some(ref usage) = tr.usage {
                        let _ = tx.send(AgentEvent::Usage(usage.clone())).await;
                    }
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
                    // A refusal is not a failure: nothing ran and nothing was
                    // recorded, so the surface is told which one this is by the
                    // event it gets, not by reading the text of an `Error`.
                    match err.downcast_ref::<ImageRefused>() {
                        Some(refused) => {
                            let _ = tx
                                .send(AgentEvent::ImageRefused {
                                    message: refused.message.clone(),
                                })
                                .await;
                        }
                        None => {
                            let _ = tx.send(AgentEvent::Error(err.to_string())).await;
                        }
                    }
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

    /// Prepare the images of a user message the loop is about to send, and
    /// return the message with them in the prepared form.
    ///
    /// The image preparation runs over the same set the loop prepares: the
    /// history as the provider will see it, plus this message. So the image
    /// count, the size limit and readability are judged as the loop will judge
    /// them, with the user's `[multimodal]` config. A refusal is an
    /// [`ImageRefused`]: the message is not pushed.
    ///
    /// Only a vision-capable provider reaches the loop's image step; any other
    /// provider keeps the loop's own refusal, so the message is returned as it
    /// is. The mime check is the rig allowlist for the primary provider, and
    /// runs on the PREPARED content, where every marker is already a data URI
    /// with its real MIME type.
    async fn prepare_new_turn(&self, content: &str) -> Result<String> {
        if !self.provider.supports_vision() {
            return Ok(content.to_string());
        }
        let mut messages = self.tool_dispatcher.to_provider_messages(&self.history);
        messages.push(ChatMessage::user(content));
        let prepared =
            match crate::multimodal::prepare_messages_for_provider(&messages, &self.multimodal)
                .await
            {
                Ok(prepared) => prepared,
                Err(err) => return Err(self.refusal_for(err, &messages).into()),
            };
        let prepared_content = prepared
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();

        if let Some(allowed) = self
            .primary_provider
            .as_deref()
            .and_then(crate::multimodal::rig_backend_mime_allowlist)
        {
            for mime in crate::multimodal::image_data_uri_mimes(&prepared_content) {
                if !allowed.contains(&mime.as_str()) {
                    let err = crate::multimodal::MultimodalError::UnsupportedMime {
                        input: format!("data:{mime};base64,…"),
                        mime,
                    };
                    return Err(self.refusal_for(err.into(), &messages).into());
                }
            }
        }
        Ok(prepared_content)
    }

    /// The text a refused turn carries. A full session says how many images it
    /// holds, the limit, and that `/new` starts over; any other refusal keeps
    /// the loop's own error text.
    fn refusal_for(&self, err: anyhow::Error, messages: &[ChatMessage]) -> ImageRefused {
        let limit_hit = err
            .downcast_ref::<crate::multimodal::MultimodalError>()
            .is_some_and(|e| matches!(e, crate::multimodal::MultimodalError::TooManyImages { .. }));
        let message = if limit_hit {
            let (max_images, _) = self.multimodal.effective_limits();
            // The session's count, this message included: what the loop counts.
            let held = crate::multimodal::count_image_markers(messages);
            format!(
                "This message was not sent: the session holds {held} images and the limit \
                 of {max_images} is reached. Start over with /new to send more images; \
                 your text is back in the input box."
            )
        } else {
            format!("This message was not sent: {err}. Your text is back in the input box.")
        };
        ImageRefused { message }
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

        // The conversation id (if any) is still on the agent, so a tool the model calls
        // can read it; the door's loader uses it as the per-turn read scope.
        let conversation_scope = self.conversation_id.as_deref();

        // The loader reads under the door's memory view; a turn with no view
        // recalls nothing.
        let context = self
            .memory_loader
            .load_context(self.memory.as_ref(), user_message, conversation_scope)
            .await
            .unwrap_or_default();

        let enriched = if context.is_empty() {
            user_message.to_string()
        } else {
            // Say that memory shaped this turn, and which memory. Emitted before
            // the first chunk so a surface can show it above the answer rather
            // than after it.
            if let Some(tx) = events {
                let _ = tx
                    .send(AgentEvent::MemoryRecalled {
                        keys: context.keys.clone(),
                    })
                    .await;
            }
            crate::memory::prepend_memory_block(&context.block, user_message)
        };

        // Prepare the message's images before it enters the history. A refusal
        // leaves the history as it was. The pushed message carries the prepared
        // content (images as data URIs), so a later turn never reads the file
        // again: a file removed after this turn cannot break the next one.
        let prepared = self.prepare_new_turn(&enriched).await?;

        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(prepared)));

        let effective_model = self.classify_model(user_message);

        // Drive the ONE shared agentic loop (PR2). The Agent already keeps
        // structured `ConversationMessage` history, so it passes it (and its
        // dispatcher) straight through — no conversion. `approval_manager` is
        // `None` for the interactive agent (it gates via the shell relay, not a
        // Layer-A manager); the console SSE surface injects a manager + a
        // web-modal backend so non-read-only tools require an in-browser
        // decision. Streaming goes through `events`.
        // The audit `channel` names the surface this agent serves. The
        // behaviour channel name stays `cli` below: only the audit line changes.
        let audit_actor = match self.surface {
            Some(name) => crate::security::AuditActor::named_surface(name),
            None => crate::security::AuditActor::surface(),
        };
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
            None, // interactive/console agent — no origin chat
            self.approval_backend.as_deref(),
            // No guest gate on the interactive/console agent — the local or
            // authenticated user is the owner.
            None,
            &self.multimodal,
            self.config.max_tool_iterations,
            cancel.cloned(),
            None,
            events.cloned(),
            self.ledger.as_deref(),
            &audit_actor,
            self.security.as_deref(),
        )
        .await;

        self.trim_history();

        match result {
            Ok((text, usage)) => {
                let usage = turn_usage(self.ledger.as_deref(), &effective_model, usage);
                // Record before returning: the ledger is what the next turn's
                // ceiling check reads, so a turn that is not recorded is a turn
                // the brake never sees.
                if let (Some(ledger), Some(u)) = (self.ledger.as_deref(), usage.as_ref()) {
                    if let Err(e) = ledger.record_usage(u.clone()) {
                        tracing::warn!("failed to record token usage: {e:#}");
                    }
                }
                Ok(TurnResult {
                    text,
                    usage,
                    cancelled: false,
                })
            }
            // The shared loop signals cancellation via `ToolLoopCancelled`.
            Err(e)
                if e.downcast_ref::<crate::agent::loop_::ToolLoopCancelled>()
                    .is_some() =>
            {
                Ok(TurnResult {
                    text: String::new(),
                    // A cancelled turn may still have burned tokens, but the
                    // loop's accumulator is gone with the error — reporting
                    // zero here would be the same lie this change removes.
                    usage: None,
                    cancelled: true,
                })
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SessionScope;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    /// The gateway shares one observer across requests so metrics reach a
    /// single `/metrics` registry. Verify the injected observer is the exact
    /// Arc the agent ends up holding (not a throwaway one built internally).
    #[tokio::test]
    async fn from_config_with_observer_holds_the_injected_observer() {
        let mut config = crate::config::Config::default();
        config.workspace_dir = std::env::temp_dir();
        config.memory.backend = "none".into();
        // A fresh default has no model now; a build requires one.
        config.default_model = Some("anthropic/claude-sonnet-4.6".into());

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let agent = Agent::from_config_with_observer(&config, observer.clone())
            .await
            .expect("from_config_with_observer should build");

        assert!(
            Arc::ptr_eq(&observer, &agent.observer),
            "agent must reuse the injected observer, not build its own"
        );
    }

    #[tokio::test]
    async fn from_config_refuses_to_build_without_a_configured_model() {
        let mut config = crate::config::Config::default();
        config.workspace_dir = std::env::temp_dir();
        config.memory.backend = "none".into();
        // A fresh install has no model; the agent must not guess one.
        assert_eq!(config.default_model, None);

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let err = Agent::from_config_with_observer(&config, observer)
            .await
            .err()
            .expect("a modelless config must not build an agent");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("no model") && msg.contains("setup provider"),
            "the error must name the problem and the fix: {msg}"
        );
    }

    /// The agent surface builds its prompt inside the turn, so the owner files
    /// follow the view the door set: `USER.md`, `MEMORY.md`, `BOOTSTRAP.md` and
    /// `TOOLS.md` are in under `All` and out under one conversation and with no
    /// view.
    #[tokio::test]
    async fn the_agent_prompt_carries_the_owner_files_only_under_the_all_view() {
        let workspace = tempfile::TempDir::new().unwrap();
        let canaries = [
            ("USER.md", "AGENT_PROFILE_CANARY_11"),
            ("MEMORY.md", "AGENT_NOTES_CANARY_22"),
            ("BOOTSTRAP.md", "AGENT_BOOTSTRAP_CANARY_33"),
            ("TOOLS.md", "AGENT_TOOLS_CANARY_44"),
        ];
        for (file, canary) in canaries {
            std::fs::write(workspace.path().join(file), canary).unwrap();
        }
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.memory.backend = "none".into();
        config.default_model = Some("anthropic/claude-sonnet-4.6".into());
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let agent = Agent::from_config_with_observer(&config, observer)
            .await
            .expect("a modelled config builds an agent");

        let under = |view: Option<crate::memory::MemoryView>| match view {
            Some(view) => {
                crate::memory::MEMORY_VIEW.sync_scope(view, || agent.build_system_prompt().unwrap())
            }
            None => agent.build_system_prompt().unwrap(),
        };

        let all = under(Some(crate::memory::MemoryView::All));
        for (file, canary) in canaries {
            assert!(all.contains(canary), "{file} missing under All:\n{all}");
        }
        for (label, view) in [
            (
                "one conversation",
                Some(crate::memory::MemoryView::Only("conversation-a".into())),
            ),
            ("no view", None),
        ] {
            let prompt = under(view);
            for (file, canary) in canaries {
                assert!(
                    !prompt.contains(canary),
                    "{file} reached the agent prompt under {label}:\n{prompt}"
                );
            }
        }
    }

    /// The TUI and the console chat build their prompt from the agent's own
    /// registry. The `## Tools` list names the tools that registry holds, each
    /// as the tool describes itself, and no other.
    #[tokio::test]
    async fn the_agent_prompt_lists_the_registry_tools_with_their_own_descriptions() {
        let workspace = tempfile::TempDir::new().unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_path_buf();
        config.memory.backend = "none".into();
        config.default_model = Some("anthropic/claude-sonnet-4.6".into());
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let agent = Agent::from_config_with_observer(&config, observer)
            .await
            .expect("a modelled config builds an agent");

        let prompt = agent.build_system_prompt().unwrap();

        crate::agent::door_test_support::assert_prompt_lists(
            "agent",
            &prompt,
            &crate::agent::door_test_support::held_by(&agent.tools),
        );
    }

    /// Build the smallest agent `trim_history` needs: it only reads
    /// `config.max_history_messages` and rewrites `history`.
    async fn trim_test_agent() -> Agent {
        let mut config = crate::config::Config::default();
        config.workspace_dir = std::env::temp_dir();
        config.memory.backend = "none".into();
        config.default_model = Some("anthropic/claude-sonnet-4.6".into());
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        Agent::from_config_with_observer(&config, observer)
            .await
            .expect("a modelled config builds an agent")
    }

    /// One tool iteration appends two separate history entries — the
    /// `AssistantToolCalls` and the `ToolResults` answering it. A cut by
    /// message count can land between them, leaving a tool result whose
    /// originating call is gone. OpenAI and Anthropic both reject that
    /// shape with a 400, so every later turn in the session fails.
    #[tokio::test]
    async fn trim_history_never_leaves_a_tool_result_without_its_call() {
        let mut agent = trim_test_agent().await;

        // Sweep the cap across every alignment of the three-entry cycle a
        // tool turn produces. A single cap would pass or fail by luck: it
        // only orphans a result when the cut happens to land on one.
        for max in 1..=12usize {
            agent.config.max_history_messages = max;
            agent.history.clear();
            for i in 0..10 {
                agent
                    .history
                    .push(ConversationMessage::Chat(ChatMessage::user(format!(
                        "ask {i}"
                    ))));
                agent.history.push(ConversationMessage::AssistantToolCalls {
                    text: None,
                    tool_calls: vec![crate::providers::ToolCall {
                        id: format!("call-{i}"),
                        name: "shell".into(),
                        arguments: "{}".into(),
                    }],
                });
                agent.history.push(ConversationMessage::ToolResults(vec![
                    crate::providers::ToolResultMessage {
                        tool_call_id: format!("call-{i}"),
                        content: "ok".into(),
                    },
                ]));
            }

            agent.trim_history();

            assert!(
                agent.history.len() <= max,
                "the cap still holds: {} kept, max {max}",
                agent.history.len()
            );

            let mut call_is_open = false;
            for (idx, msg) in agent.history.iter().enumerate() {
                match msg {
                    ConversationMessage::AssistantToolCalls { .. } => call_is_open = true,
                    ConversationMessage::ToolResults(_) => {
                        assert!(
                            call_is_open,
                            "max={max}: history[{idx}] is a tool result whose tool call was trimmed away"
                        );
                        call_is_open = false;
                    }
                    ConversationMessage::Chat(_) => call_is_open = false,
                }
            }
        }
    }

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
                    usage: None,
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
                usage: None,
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

    /// Records what actually reached the memory backend.
    #[derive(Default)]
    struct RecordingMemory {
        stored: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Memory for RecordingMemory {
        fn name(&self) -> &str {
            "recording"
        }
        async fn store(
            &self,
            _k: &str,
            content: &str,
            _cat: MemoryCategory,
            _sid: Option<&str>,
        ) -> Result<()> {
            self.stored.lock().push(content.to_string());
            Ok(())
        }
        async fn recall(
            &self,
            _q: &str,
            _l: usize,
            _s: SessionScope<'_>,
        ) -> Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn get(&self, _k: &str) -> Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _c: Option<&MemoryCategory>,
            _s: SessionScope<'_>,
        ) -> Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _k: &str) -> Result<bool> {
            Ok(false)
        }
        async fn count(&self, _scope: SessionScope<'_>) -> Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    /// Auto-save used to write the raw user message into the one store that
    /// is read back into a later prompt without anyone looking at it again.
    /// A credential typed into the TUI landed verbatim in `brain.db`, was
    /// returned by `memory_recall`, was served by `GET /api/v1/memory`, and
    /// travelled back to the provider on every later recall. With auto-save
    /// gone, the turn must leave the store empty — there is no place left for
    /// a credential to leak to.
    #[tokio::test]
    async fn turn_does_not_store_a_credential_at_all() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![crate::providers::ChatResponse {
                usage: None,
                text: Some("noted".into()),
                tool_calls: vec![],
            }]),
        });

        let mem = Arc::new(RecordingMemory::default());
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![])
            .memory(mem.clone() as Arc<dyn Memory>)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed");

        agent
            .turn("my key is api_key=sk-abcdefghijklmnopqrstuvwxyz012345 keep it safe")
            .await
            .unwrap();

        let stored = mem.stored.lock();
        assert!(
            stored.is_empty(),
            "the turn must not auto-save at all; got {} row(s): {stored:?}",
            stored.len()
        );
    }

    #[tokio::test]
    async fn turn_with_native_dispatcher_handles_tool_results_variant() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    usage: None,
                    text: Some(String::new()),
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "tc1".into(),
                        name: "echo".into(),
                        arguments: "{}".into(),
                    }],
                },
                crate::providers::ChatResponse {
                    usage: None,
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
    ///
    /// `reports_usage` selects which half of the usage contract the caller is
    /// testing: a provider that reports counts must produce exactly one `Usage`
    /// event before `Done`, and one that reports nothing must produce none.
    fn build_test_agent_reporting(text: &str, reports_usage: bool) -> Agent {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![crate::providers::ChatResponse {
                usage: if reports_usage {
                    crate::providers::ProviderUsage::from_counts(11, 7, 0)
                } else {
                    None
                },
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

    fn build_test_agent(text: &str) -> Agent {
        build_test_agent_reporting(text, true)
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
        assert!(
            saw_usage_before_done,
            "Usage must precede Done when the provider reported counts"
        );
    }

    /// The other half of the contract, and the reason the assertion above now
    /// says "when the provider reported counts": a backend that reports nothing
    /// must produce NO usage event. Emitting a zero-valued one is what made
    /// every surface render "0 tokens" as though it were a measurement, and the
    /// gateway grew a filter to hide it again.
    #[tokio::test]
    async fn turn_streaming_emits_no_usage_when_the_provider_reported_none() {
        let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
        let mut agent = build_test_agent_reporting("hello", false);

        let result = agent
            .turn_streaming("hi", Some(events_tx), None)
            .await
            .expect("turn completes");
        assert_eq!(result.text, "hello");
        assert!(result.usage.is_none(), "usage was {:?}", result.usage);

        let mut saw_done = false;
        while let Ok(ev) = events_rx.try_recv() {
            match ev {
                AgentEvent::Usage(u) => {
                    panic!("no provider reported usage, yet a Usage event was emitted: {u:?}")
                }
                AgentEvent::Done { .. } => saw_done = true,
                _ => {}
            }
        }
        assert!(saw_done, "expected Done event");
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
                    usage: None,
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

    // ── Audit `channel` names the surface ──────────────────────────────────

    /// Tool name only the audit-channel tests use, so each test can pick its
    /// own record out of a file other tests append to.
    const CHANNEL_PROBE_TOOL: &str = "audit_channel_probe";

    /// A tool that does nothing but exist, so a scripted tool call reaches the
    /// audit funnel through a real `Agent::turn`.
    struct ChannelProbeTool;

    #[async_trait]
    impl Tool for ChannelProbeTool {
        fn name(&self) -> &str {
            CHANNEL_PROBE_TOOL
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "ok".into(),
                error: None,
            })
        }
    }

    /// Drive one real turn in which the model calls the probe tool once, with
    /// the surface name set on the agent when `surface` is `Some`, and return
    /// the `channel` field of the audit record the funnel wrote for that call.
    async fn audit_channel_for_one_turn(surface: Option<&'static str>) -> String {
        let _lock = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let profile = crate::profile::ProfileManager::active().expect("profile tree");
        let log_path = profile.root.join("audit.log");
        let _audit = crate::test_env::EnvGuard::set(
            "RANTAICLAW_AUDIT_DIR_OVERRIDE",
            profile.root.as_os_str(),
        );

        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    usage: None,
                    text: Some(String::new()),
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "tc1".into(),
                        name: CHANNEL_PROBE_TOOL.into(),
                        arguments: "{}".into(),
                    }],
                },
                crate::providers::ChatResponse {
                    usage: None,
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
            .tools(vec![Box::new(ChannelProbeTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed");
        // The production surfaces name themselves with `set_surface` after
        // `from_config`; the test drives that same setter.
        if let Some(name) = surface {
            agent.set_surface(name);
        }

        let response = agent.turn("hi").await.expect("turn completes");
        assert_eq!(response, "done");

        for _ in 0..100 {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            let record = text
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|r| r["action"]["command"] == CHANNEL_PROBE_TOOL);
            if let Some(record) = record {
                return record["actor"]["channel"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no audit record for the probe tool was written");
    }

    /// The TUI's agent writes `tui` on the audit line, not the behaviour value
    /// `cli` it passes to the loop.
    #[tokio::test]
    async fn tui_agent_audit_line_names_the_tui_surface() {
        assert_eq!(audit_channel_for_one_turn(Some("tui")).await, "tui");
    }

    /// The console's agent writes `console`, the one name for both its
    /// streaming and non-streaming endpoints.
    #[tokio::test]
    async fn console_agent_audit_line_names_the_console_surface() {
        assert_eq!(audit_channel_for_one_turn(Some("console")).await, "console");
    }

    /// An agent with no surface name (the CLI path) keeps writing `cli`, as it
    /// does today.
    #[tokio::test]
    async fn agent_without_a_surface_still_writes_cli() {
        assert_eq!(audit_channel_for_one_turn(None).await, "cli");
    }

    /// A provider that declares vision and records the message bodies it was
    /// sent. Its replies are plain text, so a turn that reaches it completes.
    struct VisionMock {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for VisionMock {
        fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
            crate::providers::traits::ProviderCapabilities {
                native_tool_calling: false,
                vision: true,
            }
        }

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
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> Result<crate::providers::ChatResponse> {
            self.seen.lock().push(
                request
                    .messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            Ok(crate::providers::ChatResponse {
                usage: None,
                text: Some("seen".into()),
                tool_calls: vec![],
            })
        }
    }

    /// An agent whose primary is `provider_name`, backed by [`VisionMock`].
    fn vision_agent(provider_name: &str) -> (Agent, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::builder()
            .provider(Box::new(VisionMock { seen: seen.clone() }))
            .tools(vec![])
            .memory(Arc::from(
                crate::memory::create_memory(
                    &crate::config::MemoryConfig {
                        backend: "none".into(),
                        ..crate::config::MemoryConfig::default()
                    },
                    std::path::Path::new("/tmp"),
                    None,
                )
                .expect("memory"),
            ))
            .observer(Arc::from(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder");
        agent.primary_provider = Some(provider_name.to_string());
        (agent, seen)
    }

    #[tokio::test]
    async fn gif_on_gemini_is_refused_before_the_history_and_the_next_message_succeeds() {
        let (mut agent, _seen) = vision_agent("gemini");
        // The system prompt is seeded on the first turn; the baseline is what
        // a refusal must leave behind.
        agent.turn("hello first").await.expect("first plain turn");
        let before = agent.history.len();

        let gif = "data:image/gif;base64,R0lGODlh";
        let err = agent
            .turn(&format!("look [IMAGE:{gif}]"))
            .await
            .expect_err("gemini does not take gif");
        assert!(
            err.to_string().contains("MIME type is not allowed"),
            "same refusal text as an unreadable MIME: {err}"
        );
        assert_eq!(agent.history.len(), before, "refusal leaves history alone");

        agent
            .turn("plain text after the refusal")
            .await
            .expect("the next plain message succeeds");
    }

    #[tokio::test]
    async fn gif_on_anthropic_is_sent_as_an_image_not_refused() {
        let (mut agent, seen) = vision_agent("anthropic");
        let gif = "data:image/gif;base64,R0lGODlh";
        agent
            .turn(&format!("look [IMAGE:{gif}]"))
            .await
            .expect("anthropic takes gif");
        let sent = seen.lock().join("\n");
        assert!(sent.contains(gif), "the gif reached the provider: {sent}");
    }

    #[tokio::test]
    async fn bmp_on_openai_is_refused_before_the_history_and_the_next_message_succeeds() {
        let (mut agent, _seen) = vision_agent("openai");
        agent.turn("hello first").await.expect("first plain turn");
        let before = agent.history.len();

        let err = agent
            .turn("look [IMAGE:data:image/bmp;base64,Qk0=]")
            .await
            .expect_err("openai does not take bmp through rig");
        assert!(
            err.to_string().contains("MIME type is not allowed"),
            "{err}"
        );
        assert_eq!(agent.history.len(), before);

        agent
            .turn("plain")
            .await
            .expect("next plain message succeeds");
    }

    #[tokio::test]
    async fn missing_image_file_is_refused_with_todays_error_and_history_is_unchanged() {
        let (mut agent, _seen) = vision_agent("openai");
        agent.turn("hello first").await.expect("first plain turn");
        let before = agent.history.len();

        let err = agent
            .turn("[IMAGE:/tmp/missing-picture.png] missing picture")
            .await
            .expect_err("a missing file is refused");
        assert!(
            err.to_string()
                .contains("multimodal image source not found or unreadable"),
            "the loop's own error text: {err}"
        );
        assert_eq!(agent.history.len(), before, "refusal leaves history alone");

        agent
            .turn("plain text after the refusal")
            .await
            .expect("the next plain message succeeds");
    }

    /// Write a real file with `bytes` under a temp dir and return its path.
    fn temp_image(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write image");
        (dir, path)
    }

    #[tokio::test]
    async fn gif_file_on_gemini_is_refused_before_the_history_and_the_next_message_succeeds() {
        // A path marker: the mime is only known after the read, so the check
        // must run on the prepared content. The raw text has no data URI.
        let (_dir, gif) = temp_image("anim.gif", b"GIF89a\x01\x00\x01\x00");
        let (mut agent, _seen) = vision_agent("gemini");
        agent.turn("hello first").await.expect("first plain turn");
        let before = agent.history.len();

        let err = agent
            .turn(&format!("look [IMAGE:{}]", gif.display()))
            .await
            .expect_err("gemini does not take a gif file");
        assert!(
            err.to_string().contains("MIME type is not allowed")
                && err.to_string().contains("image/gif"),
            "the UnsupportedMime refusal names the gif: {err}"
        );
        assert_eq!(agent.history.len(), before, "no user message was pushed");

        agent
            .turn("plain text after the refusal")
            .await
            .expect("the next plain message succeeds");
    }

    #[tokio::test]
    async fn bmp_file_on_openai_is_refused_before_the_history_and_the_next_message_succeeds() {
        let (_dir, bmp) = temp_image("scan.bmp", b"BM\x1a\x00\x00\x00\x00\x00\x00\x00");
        let (mut agent, _seen) = vision_agent("openai");
        agent.turn("hello first").await.expect("first plain turn");
        let before = agent.history.len();

        let err = agent
            .turn(&format!("look [IMAGE:{}]", bmp.display()))
            .await
            .expect_err("openai does not take a bmp file through rig");
        assert!(
            err.to_string().contains("MIME type is not allowed")
                && err.to_string().contains("image/bmp"),
            "the UnsupportedMime refusal names the bmp: {err}"
        );
        assert_eq!(agent.history.len(), before, "no user message was pushed");

        agent
            .turn("plain text after the refusal")
            .await
            .expect("the next plain message succeeds");
    }

    #[tokio::test]
    async fn refused_image_surfaces_as_a_typed_refusal_then_done() {
        let (mut agent, _seen) = vision_agent("openai");
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);

        let result = agent
            .turn_streaming(
                "[IMAGE:/tmp/missing-picture.png] missing picture",
                Some(tx),
                None,
            )
            .await;
        assert!(result.is_err());

        let mut saw_refusal = false;
        let mut saw_done = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::ImageRefused { message } => {
                    assert!(
                        message.contains("multimodal image source not found or unreadable"),
                        "{message}"
                    );
                    saw_refusal = true;
                }
                AgentEvent::Error(msg) => panic!("a refusal is not a plain error: {msg}"),
                AgentEvent::Done { .. } => saw_done = true,
                _ => {}
            }
        }
        assert!(saw_refusal, "the TUI gets a typed refusal");
        assert!(
            saw_done,
            "the turn still ends, so turn bookkeeping is unchanged"
        );
    }

    /// A vision agent whose `[multimodal]` config is `multimodal`, the way
    /// `from_config` gives it the user's section.
    fn vision_agent_with_multimodal(
        provider_name: &str,
        multimodal: crate::config::MultimodalConfig,
    ) -> (Agent, Arc<Mutex<Vec<String>>>) {
        let (mut agent, seen) = vision_agent(provider_name);
        agent.multimodal = multimodal;
        (agent, seen)
    }

    /// A 1×1 PNG's bytes: enough for MIME detection, small enough to allow.
    const PNG_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

    #[tokio::test]
    async fn the_session_image_count_is_judged_across_the_history() {
        // Five turns of one image each, under the default limit of four. The
        // fifth image is the one that would take the session past the limit;
        // the loop counts every marker in the history, so the check must too.
        let (_dir, png) = temp_image("count.png", PNG_BYTES);
        let (mut agent, _seen) = vision_agent_with_multimodal(
            "openai",
            crate::config::MultimodalConfig {
                max_images: 4,
                ..crate::config::MultimodalConfig::default()
            },
        );
        for n in 1..=4 {
            agent
                .turn(&format!("picture {n} [IMAGE:{}]", png.display()))
                .await
                .unwrap_or_else(|e| panic!("image {n} of four is accepted: {e}"));
        }
        let before = agent.history.len();

        let err = agent
            .turn(&format!("picture 5 [IMAGE:{}]", png.display()))
            .await
            .expect_err("the fifth image of the session is refused");
        assert!(
            err.downcast_ref::<ImageRefused>().is_some(),
            "a refusal is typed, so the screen can tell it from a failure: {err}"
        );
        assert!(
            err.to_string().contains("holds 5 images")
                && err.to_string().contains("limit of 4")
                && err.to_string().contains("/new"),
            "the refusal says the count (this message included), the limit and /new: {err}"
        );
        assert_eq!(agent.history.len(), before, "refusal leaves history alone");

        agent
            .turn("plain text after the refusal")
            .await
            .expect("a plain turn after the refusal succeeds");
    }

    #[tokio::test]
    async fn a_prepared_image_is_not_read_again_on_a_later_turn() {
        // A good image turn, then the file is removed, then a plain turn. The
        // history carries the prepared data URI, so the plain turn does not
        // re-read the file and succeeds.
        let (dir, png) = temp_image("gone.png", PNG_BYTES);
        let (mut agent, seen) = vision_agent("openai");
        agent
            .turn(&format!("look [IMAGE:{}]", png.display()))
            .await
            .expect("the image turn is accepted");
        drop(dir);
        assert!(!png.exists(), "the file is gone before the next turn");

        agent
            .turn("plain text after the image")
            .await
            .expect("the plain turn succeeds with the image file gone");
        let sent = seen.lock().join("\n");
        assert!(
            sent.contains("data:image/png;base64,"),
            "the image went out as a data URI: {sent}"
        );
        assert!(
            !sent.contains(&png.display().to_string()),
            "the path marker did not go out again: {sent}"
        );
    }

    #[tokio::test]
    async fn the_users_size_limit_reaches_the_pre_push_check() {
        // A 2 MB image under the user's `max_image_size_mb = 1`: refused before
        // the push. Under the default of 5 MB it would be accepted.
        let big = vec![0u8; 2 * 1024 * 1024];
        let mut bytes = PNG_BYTES.to_vec();
        bytes.extend_from_slice(&big);
        let (_dir, png) = temp_image("big.png", &bytes);
        let (mut agent, _seen) = vision_agent_with_multimodal(
            "openai",
            crate::config::MultimodalConfig {
                max_image_size_mb: 1,
                ..crate::config::MultimodalConfig::default()
            },
        );
        agent.turn("hello first").await.expect("first plain turn");
        let before = agent.history.len();

        let err = agent
            .turn(&format!("look [IMAGE:{}]", png.display()))
            .await
            .expect_err("a 2 MB image is over the user's 1 MB limit");
        assert!(
            err.to_string()
                .contains("multimodal image size limit exceeded"),
            "{err}"
        );
        assert_eq!(agent.history.len(), before, "refusal leaves history alone");
    }

    #[tokio::test]
    async fn a_reseed_of_a_turn_whose_file_is_gone_keeps_the_text_and_never_fails() {
        // A turn restored from the store names an image whose file was removed
        // since. The reseed keeps the turn's text, puts a placeholder in place
        // of the marker, and does not fail: the session stays usable.
        let (dir, png) = temp_image("moved.png", PNG_BYTES);
        drop(dir);
        let (mut agent, seen) = vision_agent("openai");
        let turns = vec![
            (
                "user".to_string(),
                format!("look at this [IMAGE:{}]", png.display()),
            ),
            ("assistant".to_string(), "I see it".to_string()),
        ];

        agent
            .reseed_history(&turns)
            .await
            .expect("a reseed never fails on a missing image");

        let held: Vec<String> = agent
            .history
            .iter()
            .filter_map(|m| match m {
                ConversationMessage::Chat(c) if c.role == "user" => Some(c.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(held.len(), 1, "one user turn is held: {held:?}");
        assert!(
            held[0].contains("look at this"),
            "the turn's text is kept: {held:?}"
        );
        assert!(
            !held[0].contains("[IMAGE:"),
            "no image marker is held: {held:?}"
        );
        assert!(
            held[0].contains("moved.png"),
            "the placeholder names the file: {held:?}"
        );

        agent
            .turn("plain text after the reseed")
            .await
            .expect("the next turn succeeds after the reseed");
        assert!(seen
            .lock()
            .join("\n")
            .contains("plain text after the reseed"));
    }

    #[tokio::test]
    async fn vision_off_provider_keeps_the_loops_refusal() {
        // A provider without vision never reaches the pre-push check: the
        // loop refuses the marker with the capability error, as before.
        let mut agent = Agent::builder()
            .provider(Box::new(MockProvider {
                responses: Mutex::new(vec![]),
            }))
            .tools(vec![])
            .memory(Arc::from(
                crate::memory::create_memory(
                    &crate::config::MemoryConfig {
                        backend: "none".into(),
                        ..crate::config::MemoryConfig::default()
                    },
                    std::path::Path::new("/tmp"),
                    None,
                )
                .expect("memory"),
            ))
            .observer(Arc::from(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder");
        agent.primary_provider = Some("gemini".into());

        let err = agent
            .turn("[IMAGE:data:image/png;base64,iVBORw0KGgo=] look")
            .await
            .expect_err("a non-vision provider refuses the image");
        assert!(
            err.to_string().contains("does not support vision input"),
            "the capability refusal is unchanged: {err}"
        );
    }
}
