use crate::providers::{is_glm_alias, is_zai_alias};
use crate::security::AutonomyLevel;
use anyhow::{Context, Result};
use directories::UserDirs;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
#[cfg(unix)]
use tokio::fs::File;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

const SUPPORTED_PROXY_SERVICE_KEYS: &[&str] = &[
    "provider.anthropic",
    "provider.compatible",
    "provider.copilot",
    "provider.gemini",
    "provider.glm",
    "provider.ollama",
    "provider.openai",
    "provider.openrouter",
    "channel.dingtalk",
    "channel.discord",
    "channel.lark",
    "channel.matrix",
    "channel.mattermost",
    "channel.nextcloud_talk",
    "channel.qq",
    "channel.signal",
    "channel.slack",
    "channel.telegram",
    "channel.whatsapp",
    "tool.browser",
    "tool.composio",
    "tool.http_request",
    "tool.pushover",
    "memory.embeddings",
    "tunnel.custom",
];

const SUPPORTED_PROXY_SERVICE_SELECTORS: &[&str] =
    &["provider.*", "channel.*", "tool.*", "memory.*", "tunnel.*"];

static RUNTIME_PROXY_CONFIG: OnceLock<RwLock<ProxyConfig>> = OnceLock::new();
/// Proxy env vars THIS process authored via `apply_to_process_env`. On a config
/// reload, `apply_env_overrides` must not mistake a var it wrote itself for a
/// user-supplied proxy signal — re-reading the `HTTP_PROXY` it had just written
/// flipped a `[proxy] enabled = false` config back to `true` every reload.
static PROXY_AUTHORED_VARS: OnceLock<RwLock<HashSet<&'static str>>> = OnceLock::new();
static RUNTIME_PROXY_CLIENT_CACHE: OnceLock<RwLock<HashMap<String, reqwest::Client>>> =
    OnceLock::new();

// ── Top-level config ──────────────────────────────────────────────

/// Top-level RantaiClaw configuration, loaded from `config.toml`.
///
/// Resolution order: `RANTAICLAW_WORKSPACE` env → `active_workspace.toml` marker → `~/.rantaiclaw/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    /// On-disk schema version. Stamped by `config::migrations::migrate`
    /// at load time and round-tripped on every write so the next
    /// load can skip migrations that already ran. Absent on configs
    /// written by pre-v0.6.45 binaries — the migrator treats those
    /// as version `0` and stamps the current version.
    #[serde(default = "default_config_schema_version")]
    pub schema_version: u32,
    /// Workspace directory - computed from home, not serialized
    #[serde(skip)]
    pub workspace_dir: PathBuf,
    /// Path to config.toml - computed from home, not serialized
    #[serde(skip)]
    pub config_path: PathBuf,
    /// API key for the selected provider. Overridden by `RANTAICLAW_API_KEY` or `API_KEY` env vars.
    pub api_key: Option<String>,
    /// Per-provider API keys, keyed by canonical provider name (e.g. `"openai"`,
    /// `"minimax"`). Lets the console store a distinct key per provider so
    /// switching the active provider never reuses another provider's credential.
    /// Encrypted at rest like `api_key`. The top-level `api_key` remains the
    /// active (`default_provider`) provider's key for backward compatibility.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub provider_api_keys: HashMap<String, String>,
    /// Base URL override for provider API (e.g. "http://10.0.0.1:11434" for remote Ollama)
    pub api_url: Option<String>,
    /// Default provider ID or alias (e.g. `"openrouter"`, `"ollama"`, `"anthropic"`). Default: `"openrouter"`.
    pub default_provider: Option<String>,
    /// Default model routed through the selected provider (e.g. `"anthropic/claude-sonnet-4-6"`).
    pub default_model: Option<String>,
    /// Default model temperature (0.0–2.0). Default: `0.7`.
    #[serde(default = "default_temperature_value")]
    pub default_temperature: f64,

    /// Observability backend configuration (`[observability]`).
    #[serde(default)]
    pub observability: ObservabilityConfig,

    /// Autonomy and security policy configuration (`[autonomy]`).
    #[serde(default)]
    pub autonomy: AutonomyConfig,

    /// Runtime adapter configuration (`[runtime]`). Controls native vs Docker execution.
    #[serde(default)]
    pub runtime: RuntimeConfig,

    /// Reliability settings: retries, fallback providers, backoff (`[reliability]`).
    #[serde(default)]
    pub reliability: ReliabilityConfig,

    /// Scheduler configuration for periodic task execution (`[scheduler]`).
    #[serde(default)]
    pub scheduler: SchedulerConfig,

    /// Agent orchestration settings (`[agent]`).
    #[serde(default)]
    pub agent: AgentConfig,

    /// Skills loading and community repository behavior (`[skills]`).
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Model routing rules — route `hint:<name>` to specific provider+model combos.
    #[serde(default)]
    pub model_routes: Vec<ModelRouteConfig>,

    /// Embedding routing rules — route `hint:<name>` to specific provider+model combos.
    #[serde(default)]
    pub embedding_routes: Vec<EmbeddingRouteConfig>,

    /// Automatic query classification — maps user messages to model hints.
    #[serde(default)]
    pub query_classification: QueryClassificationConfig,

    /// Heartbeat configuration for periodic health pings (`[heartbeat]`).
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    /// Cron job configuration (`[cron]`).
    #[serde(default)]
    pub cron: CronConfig,

    /// Task engine configuration (`[tasks]`).
    #[serde(default)]
    pub tasks: TasksConfig,

    /// Channel configurations: Telegram, Discord, Slack, etc. (`[channels_config]`).
    #[serde(default)]
    pub channels_config: ChannelsConfig,

    /// Memory backend configuration: sqlite, markdown, embeddings (`[memory]`).
    #[serde(default)]
    pub memory: MemoryConfig,

    /// Tunnel configuration for exposing the gateway publicly (`[tunnel]`).
    #[serde(default)]
    pub tunnel: TunnelConfig,

    /// Gateway server configuration: host, port, pairing, rate limits (`[gateway]`).
    #[serde(default)]
    pub gateway: GatewayConfig,

    /// Web console (`[ui]`) settings: the `rantaiclaw ui start` bind address.
    #[serde(default)]
    pub ui: UiConfig,

    /// Composio managed OAuth tools integration (`[composio]`).
    #[serde(default)]
    pub composio: ComposioConfig,

    /// Knowledge Base credentials (see `KnowledgeConfig`).
    #[serde(default)]
    pub knowledge: KnowledgeConfig,

    /// Secrets encryption configuration (`[secrets]`).
    #[serde(default)]
    pub secrets: SecretsConfig,

    /// Browser automation configuration (`[browser]`).
    #[serde(default)]
    pub browser: BrowserConfig,

    /// HTTP request tool configuration (`[http_request]`).
    #[serde(default)]
    pub http_request: HttpRequestConfig,

    /// Multimodal (image) handling configuration (`[multimodal]`).
    #[serde(default)]
    pub multimodal: MultimodalConfig,

    /// Web search tool configuration (`[web_search]`).
    #[serde(default)]
    pub web_search: WebSearchConfig,

    /// Auto-managed external services (Docker containers, sidecars).
    /// Deny-by-default: each service must set `auto_launch = true` explicitly (`[services]`).
    #[serde(default)]
    pub services: ServicesConfig,

    /// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]`).
    #[serde(default)]
    pub proxy: ProxyConfig,

    /// Identity format configuration: OpenClaw or AIEOS (`[identity]`).
    #[serde(default)]
    pub identity: IdentityConfig,

    /// Cost tracking and budget enforcement configuration (`[cost]`).
    #[serde(default)]
    pub cost: CostConfig,

    /// Delegate agent configurations for multi-agent workflows.
    #[serde(default)]
    pub agents: HashMap<String, DelegateAgentConfig>,

    /// Gateway agent configurations for multi-agent gateway routing.
    /// Each entry defines a co-equal agent hosted by the gateway, routed via `X-Agent-Id` header.
    #[serde(default)]
    pub gateway_agents: HashMap<String, GatewayAgentConfig>,

    /// MCP servers managed by the runtime (`[mcp_servers.<name>]`).
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// What `apply_env_overrides` changed on this value, remembered so
    /// `save()` writes the operator's file rather than the environment this
    /// run happened to have. Never serialised, and absent from the schema.
    ///
    /// Written by `apply_env_overrides` and read by `save()`. It is `pub` only
    /// because `..Config::default()` outside this crate cannot see a private
    /// field; `EnvOverrideSnapshot` keeps its own fields private, so the only
    /// value an external caller can put here is `None`.
    #[serde(skip)]
    #[schemars(skip)]
    pub env_overrides: Option<Box<EnvOverrideSnapshot>>,
}

/// The config as it was before and after `apply_env_overrides` ran, as JSON so
/// the comparison is generic: every override present today is covered, and any
/// override added later is covered without touching `save()`.
#[derive(Debug, Clone)]
pub struct EnvOverrideSnapshot {
    before: serde_json::Value,
    after: serde_json::Value,
}

// ── MCP Servers ──────────────────────────────────────────────────

/// MCP server configuration for stdio-based servers.
// `PartialEq` so the gateway's MCP pool can tell whether a hot-reloaded config
// still describes the servers it has running. Kept off the doc comment: that
// text lands in the published JSON schema, and this is an implementation note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpServerConfig {
    /// Command to spawn (e.g., "npx", "node")
    pub command: String,
    /// Arguments (e.g., `["-y", "@modelcontextprotocol/server-github"]`)
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables passed to the process
    #[serde(default)]
    pub env: HashMap<String, String>,
}

// ── Delegate Agents ──────────────────────────────────────────────

/// Configuration for a delegate sub-agent used by the `delegate` tool.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DelegateAgentConfig {
    /// Provider name (e.g. "ollama", "openrouter", "anthropic")
    pub provider: String,
    /// Model name
    pub model: String,
    /// Optional system prompt for the sub-agent
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Optional API key override
    #[serde(default)]
    pub api_key: Option<String>,
    /// Temperature override
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Max recursion depth for nested delegation
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Enable agentic sub-agent mode (multi-turn tool-call loop).
    #[serde(default)]
    pub agentic: bool,
    /// Allowlist of tool names available to the sub-agent in agentic mode.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Maximum tool-call iterations in agentic mode.
    #[serde(default = "default_max_tool_iterations")]
    pub max_iterations: usize,
}

fn default_max_depth() -> u32 {
    3
}

fn default_max_tool_iterations() -> usize {
    10
}

// ── Gateway Agents ──────────────────────────────────────────────

/// Configuration for a co-equal agent hosted by the gateway.
/// Unlike `DelegateAgentConfig` (used for sub-agent delegation via the `delegate` tool),
/// gateway agents are independent agents routed by the `X-Agent-Id` HTTP header.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GatewayAgentConfig {
    /// Workspace directory for this agent (skills, memory, persona files).
    pub workspace_dir: PathBuf,
    /// Provider name override (falls back to root `default_provider`).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model name override (falls back to root `default_model`).
    #[serde(default)]
    pub model: Option<String>,
    /// System prompt override for this agent.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// API key override for this agent.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Temperature override (falls back to root `default_temperature`).
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Allowlist of tool names available to this agent (empty = all tools).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Maximum tool-call iterations per request.
    #[serde(default = "default_max_tool_iterations")]
    pub max_tool_iterations: usize,
    /// Mark this agent as the default (receives requests without `X-Agent-Id`).
    #[serde(default)]
    pub default: bool,
}

// ── Hardware Config (retired in v36) ──────────────────────────────────────

/// Agent orchestration configuration (`[agent]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentConfig {
    /// When true: bootstrap_max_chars=6000, rag_chunk_limit=2. Use for 13B or smaller models.
    #[serde(default)]
    pub compact_context: bool,
    /// Maximum tool-call loop turns per user message. Default: `50`.
    /// Setting to `0` falls back to the safe runtime cap of `10`.
    #[serde(default = "default_agent_max_tool_iterations")]
    pub max_tool_iterations: usize,
    /// Maximum conversation history messages retained per session. Default: `50`.
    #[serde(default = "default_agent_max_history_messages")]
    pub max_history_messages: usize,
    /// Tool dispatch strategy (e.g. `"auto"`). Default: `"auto"`.
    #[serde(default = "default_agent_tool_dispatcher")]
    pub tool_dispatcher: String,
}

fn default_agent_max_tool_iterations() -> usize {
    // Bumped from 10 to 25 in v0.6.50. Multi-step skills (stocks setup
    // + fetch, research-assistant, github sequences) routinely need
    // ~15 tool calls before producing a final answer; the 10-call cap
    // surfaced as "Agent exceeded maximum tool iterations" mid-skill
    // with an empty "[no response from model]" body. Cap is still
    // bounded — runaway loops bail at 50 instead of being unlimited.
    // Bumped 25 → 50 so long multi-tool turns finish instead of cutting off
    // with a "reached maximum tool calls" message mid-task.
    50
}

fn default_agent_max_history_messages() -> usize {
    50
}

fn default_agent_tool_dispatcher() -> String {
    "auto".into()
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            compact_context: false,
            max_tool_iterations: default_agent_max_tool_iterations(),
            max_history_messages: default_agent_max_history_messages(),
            tool_dispatcher: default_agent_tool_dispatcher(),
        }
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillsPromptInjectionMode {
    /// Inline full skill instructions and tool metadata into the system prompt.
    #[default]
    Full,
    /// Inline only compact skill metadata (name/description/location) and load details on demand.
    Compact,
}

fn parse_skills_prompt_injection_mode(raw: &str) -> Option<SkillsPromptInjectionMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "full" => Some(SkillsPromptInjectionMode::Full),
        "compact" => Some(SkillsPromptInjectionMode::Compact),
        _ => None,
    }
}

/// Skills loading configuration (`[skills]` section).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SkillsConfig {
    /// Enable loading and syncing the community open-skills repository.
    /// Default: `false` (opt-in).
    #[serde(default)]
    pub open_skills_enabled: bool,
    /// Optional path to a local open-skills repository.
    /// If unset, defaults to `$HOME/open-skills` when enabled.
    #[serde(default)]
    pub open_skills_dir: Option<String>,
    /// Pin the community open-skills repository to a specific commit SHA,
    /// tag, or branch instead of auto-advancing on every periodic
    /// `git pull --ff-only`. When unset, falls back to the binary's built-in
    /// default pin (unset by default — see plan 045). Setting this closes
    /// off a remote author landing new instructions upstream and having them
    /// silently picked up on the next sync.
    #[serde(default)]
    pub open_skills_ref: Option<String>,
    /// Controls how skills are injected into the system prompt.
    /// `full` preserves legacy behavior. `compact` keeps context small and loads skills on demand.
    #[serde(default)]
    pub prompt_injection_mode: SkillsPromptInjectionMode,
    /// Per-skill configuration entries keyed by slug. Mirrors OpenClaw's
    /// `skills.entries.<name>` JSON shape so the same skill works in
    /// either runtime with the same config block.
    ///
    /// ```toml
    /// [skills.entries.weather]
    /// enabled = true
    ///
    /// [skills.entries.image-lab]
    /// enabled = true
    /// [skills.entries.image-lab.api_key]
    /// source = "env"
    /// id = "GEMINI_API_KEY"
    /// [skills.entries.image-lab.config]
    /// endpoint = "https://example.com"
    /// ```
    ///
    /// Skills with `enabled = false` are excluded by the loader. Skills
    /// without an entry are loaded by default (enabled). The `api_key`,
    /// `env`, and `config` sub-tables are read by skill scripts (or
    /// surfaced in `skills inspect`) — the loader itself only consults
    /// `enabled`.
    #[serde(default)]
    pub entries: std::collections::HashMap<String, SkillEntryConfig>,
    /// Install-recipe selection knobs. OpenClaw exposes the equivalents
    /// at `skills.install.preferBrew` / `nodeManager` — same shape here
    /// so a user moving from OpenClaw can paste their preferences in
    /// and have them honoured.
    #[serde(default, alias = "install")]
    pub install: SkillsInstallConfig,
}

/// Recipe-selector overrides for `skills install-deps`. Defaults match the
/// hardcoded preference order shipped through v0.6.31 (brew first, npm as
/// the node-manager default), so this block is only needed when a user
/// wants to deviate.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillsInstallConfig {
    /// When true (default), the runner prefers `brew` recipes over
    /// `uv`/`npm`/`go` when brew is available on the host. Set to
    /// `false` to demote brew below uv. Has no effect on hosts without
    /// brew.
    #[serde(default = "default_true_install")]
    pub prefer_brew: bool,
    /// Which node-package-manager recipe wins when multiple are
    /// declared. Recognised values: `npm` (default), `pnpm`, `yarn`.
    /// Unknown values fall back to `npm`. The runner still skips a
    /// preferred recipe whose driver isn't on `$PATH`.
    #[serde(default = "default_node_manager")]
    pub node_manager: String,
}

impl Default for SkillsInstallConfig {
    fn default() -> Self {
        Self {
            prefer_brew: true,
            node_manager: default_node_manager(),
        }
    }
}

fn default_true_install() -> bool {
    true
}

fn default_node_manager() -> String {
    "npm".to_string()
}

/// Per-skill configuration entry — OpenClaw `skills.entries.<name>` parity.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillEntryConfig {
    /// Whether the skill is loaded into the agent's context. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional API-key wiring. `source = "env"` resolves at run time from
    /// the named env var; `source = "literal"` uses `value` directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SkillApiKey>,
    /// Extra environment variables to expose to the skill's scripts when
    /// the agent shells out. Mapped onto the child process at exec time.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    /// Free-form per-skill config. Skill-specific schema; rantaiclaw does
    /// not validate the keys. Stored as JSON values for schema-derive
    /// compatibility — TOML deserialises seamlessly into these.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub config: std::collections::HashMap<String, serde_json::Value>,
}

impl Default for SkillEntryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: None,
            env: std::collections::HashMap::new(),
            config: std::collections::HashMap::new(),
        }
    }
}

/// API-key resolution shape used inside `[skills.entries.<name>.api_key]`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SkillApiKey {
    /// `env` (recommended) or `literal`.
    pub source: String,
    /// When `source = "env"`, the env var name to read. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// When `source = "literal"`, the API key value. **Avoid** — prefer env.
    /// Encrypted at rest via the secret store when `secrets.encrypt = true`
    /// (the default), same as `config.api_key` and every other provider
    /// credential — see the encrypt/decrypt loops in `Config::save` /
    /// `Config::load_or_init`. `literal` is still accepted for compat with
    /// OpenClaw-style configs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Multimodal (image) handling configuration (`[multimodal]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MultimodalConfig {
    /// Maximum number of image attachments accepted per request.
    #[serde(default = "default_multimodal_max_images")]
    pub max_images: usize,
    /// Maximum image payload size in MiB before base64 encoding.
    #[serde(default = "default_multimodal_max_image_size_mb")]
    pub max_image_size_mb: usize,
    /// Allow fetching remote image URLs (http/https). Disabled by default.
    #[serde(default)]
    pub allow_remote_fetch: bool,
    /// Runtime-only workspace root used to confine local image-marker reads
    /// (`[IMAGE:/path]`) that arrive from remote channels/gateway. Populated at
    /// those entry points from the active workspace; `None` (the default — e.g.
    /// local CLI, library/tests) leaves local reads unconfined. Never
    /// serialized and absent from the JSON schema — not a config key.
    #[serde(skip)]
    #[schemars(skip)]
    pub runtime_workspace: Option<PathBuf>,
}

fn default_multimodal_max_images() -> usize {
    4
}

fn default_multimodal_max_image_size_mb() -> usize {
    5
}

impl MultimodalConfig {
    /// Clamp configured values to safe runtime bounds.
    pub fn effective_limits(&self) -> (usize, usize) {
        let max_images = self.max_images.clamp(1, 16);
        let max_image_size_mb = self.max_image_size_mb.clamp(1, 20);
        (max_images, max_image_size_mb)
    }

    /// Return a copy confined to `workspace`: local image-marker reads
    /// (`[IMAGE:/path]`) will be restricted to files under it. Applied at
    /// remote entry points (channels / gateway) to sandbox untrusted markers.
    #[must_use]
    pub fn with_runtime_workspace(mut self, workspace: PathBuf) -> Self {
        self.runtime_workspace = Some(workspace);
        self
    }
}

impl Default for MultimodalConfig {
    fn default() -> Self {
        Self {
            max_images: default_multimodal_max_images(),
            max_image_size_mb: default_multimodal_max_image_size_mb(),
            allow_remote_fetch: false,
            runtime_workspace: None,
        }
    }
}

// ── Identity (AIEOS / OpenClaw format) ──────────────────────────

/// Identity format configuration (`[identity]` section).
///
/// Supports `"openclaw"` (default) or `"aieos"` identity documents.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IdentityConfig {
    /// Identity format: "openclaw" (default) or "aieos"
    #[serde(default = "default_identity_format")]
    pub format: String,
    /// Path to AIEOS JSON file (relative to workspace)
    #[serde(default)]
    pub aieos_path: Option<String>,
    /// Inline AIEOS JSON (alternative to file path)
    #[serde(default)]
    pub aieos_inline: Option<String>,
}

fn default_identity_format() -> String {
    "openclaw".into()
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            format: default_identity_format(),
            aieos_path: None,
            aieos_inline: None,
        }
    }
}

// ── Cost tracking and budget enforcement ───────────────────────────

/// Token accounting and the daily ceiling (`[cost]` section).
///
/// **The ceiling is denominated in tokens, not money.** It exists to stop
/// unattended runaway — the heartbeat running a turn per task per tick, cron
/// running with nobody watching — and stopping that needs a ceiling on
/// something the product can count. After plan 306 steps 1-2 tokens are counted
/// exactly; dollars are not, because there is no price source unless an operator
/// supplies one (see [`CostConfig::prices`]).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CostConfig {
    /// Count token usage and enforce [`CostConfig::max_tokens_per_day`].
    ///
    /// **Default `true`.** It was `false`, which meant the only runaway brake in
    /// the product was off on every install — and a brake that is off by default
    /// is not a brake. Turning it off disables both the daily ceiling and the
    /// usage record it is computed from.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Daily ceiling in **tokens**, counted across every surface. `0` disables
    /// the ceiling while leaving accounting on.
    ///
    /// The default is deliberately generous — it is a runaway brake, not a
    /// budget. A supervised human session will not reach it; a heartbeat stuck
    /// in a loop will.
    #[serde(default = "default_max_tokens_per_day")]
    pub max_tokens_per_day: u64,

    /// Warn when usage reaches this percentage of the ceiling (default: 80)
    #[serde(default = "default_warn_percent")]
    pub warn_at_percent: u8,

    /// Optional per-model prices, supplied by the operator, used **only for
    /// reporting**. Nothing is enforced in money.
    ///
    /// Keyed by the model id as the provider reports it (e.g.
    /// `"anthropic/claude-sonnet-4"`). A model with no entry reports "not
    /// reported" rather than `0.00` — a wrong number is worse than none.
    ///
    /// This key existed before and was deleted as dead in schema v25 because
    /// nothing read it. It is back because it now has a reader, and because the
    /// alternative — a price table bundled with the binary — goes stale silently
    /// and then reports confidently wrong numbers.
    #[serde(default)]
    pub prices: std::collections::HashMap<String, ModelPrice>,
}

/// What one model costs, per million tokens, as the operator recorded it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct ModelPrice {
    /// USD per million input (prompt) tokens.
    pub input_per_million: f64,
    /// USD per million output (completion) tokens.
    pub output_per_million: f64,
}

/// 2,000,000 tokens/day. Roughly a full day of heavy supervised use on a large
/// model, and far below what a heartbeat loop reaches in an hour.
fn default_max_tokens_per_day() -> u64 {
    2_000_000
}

fn default_warn_percent() -> u8 {
    80
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens_per_day: default_max_tokens_per_day(),
            warn_at_percent: default_warn_percent(),
            prices: std::collections::HashMap::new(),
        }
    }
}

// ── Peripherals (retired in v36) ──────────────────────────────────────────

// ── Gateway security ─────────────────────────────────────────────

/// Optional single-operator login for the web console + TUI. Enabled by the
/// presence of `password_hash` (an argon2 PHC string). The username is not
/// secret and is never served by a public endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq)]
pub struct GatewayLoginConfig {
    /// Operator username, verified on login. Not secret.
    #[serde(default)]
    pub username: Option<String>,
    /// Argon2 PHC-string hash of the password (salt embedded). One-way; stored
    /// verbatim (NOT routed through the reversible secret-encryption pass).
    #[serde(default)]
    pub password_hash: Option<String>,
    /// Auto-lock after this many seconds without operator input. `0` (the
    /// default) disables it, preserving the historical behaviour where a
    /// session stays unlocked until the operator quits. The TUI re-arms its
    /// login gate; the web console expires its session cookie. Only meaningful
    /// alongside `password_hash` — with no credential there is nothing to
    /// unlock with, so the timeout is ignored.
    #[serde(default)]
    pub idle_timeout_secs: u64,
}

/// Gateway server configuration (`[gateway]` section).
///
/// Controls the HTTP gateway for webhook and pairing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GatewayConfig {
    /// Gateway port (default: 9393)
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Gateway host (default: 127.0.0.1)
    #[serde(default = "default_gateway_host")]
    pub host: String,
    /// Require pairing before accepting requests (default: true)
    #[serde(default = "default_true")]
    pub require_pairing: bool,
    /// Allow binding to non-localhost without a tunnel (default: false)
    #[serde(default)]
    pub allow_public_bind: bool,
    /// Paired bearer tokens (managed automatically, not user-edited)
    #[serde(default)]
    pub paired_tokens: Vec<String>,

    /// Max `/pair` requests per minute per client key.
    #[serde(default = "default_pair_rate_limit")]
    pub pair_rate_limit_per_minute: u32,

    /// Max `/webhook` requests per minute per client key.
    #[serde(default = "default_webhook_rate_limit")]
    pub webhook_rate_limit_per_minute: u32,

    /// Max `/api/v1/*` requests per minute per client key.
    ///
    /// Generous by default — the console polls `/status` every 15 s and
    /// refreshes several panels — but bounded, because `POST /api/v1/agent/chat`
    /// drives real provider inference and an unbounded caller is a direct
    /// cost-amplification path against the operator's provider billing.
    #[serde(default = "default_api_rate_limit")]
    pub api_rate_limit_per_minute: u32,

    /// Trust proxy-forwarded client IP headers (`X-Forwarded-For`, `X-Real-IP`).
    /// Disabled by default; enable only behind a trusted reverse proxy.
    #[serde(default)]
    pub trust_forwarded_headers: bool,

    /// Maximum distinct client keys tracked by gateway rate limiter maps.
    #[serde(default = "default_gateway_rate_limit_max_keys")]
    pub rate_limit_max_keys: usize,

    /// TTL for webhook idempotency keys.
    #[serde(default = "default_idempotency_ttl_secs")]
    pub idempotency_ttl_secs: u64,

    /// Maximum distinct idempotency keys retained in memory.
    #[serde(default = "default_gateway_idempotency_max_keys")]
    pub idempotency_max_keys: usize,

    /// Response deadline in seconds for `/api/v1/*`, `/api/v1/config`, and
    /// `/api/v1/cron` requests (default: 300, floored at 5). It bounds the
    /// response *future*, so it cuts the synchronous `POST /api/v1/agent/chat`
    /// but NOT a live streaming (SSE) chat body. Increase for workloads with
    /// long-running sync tool calls; prefer the streaming path for those.
    #[serde(default = "default_gateway_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Optional web console + TUI login credential (single operator).
    #[serde(default)]
    pub login: GatewayLoginConfig,
}

fn default_gateway_port() -> u16 {
    9393
}

fn default_gateway_host() -> String {
    "127.0.0.1".into()
}

/// Web console (`[ui]`) configuration.
///
/// The console served by `rantaiclaw ui start` binds `host` (default
/// `127.0.0.1`, loopback-only). Set `host = "0.0.0.0"` to reach it from other
/// devices on the LAN — the console is a full agent-control surface, so enable
/// a login first (`rantaiclaw setup login`) before exposing it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct UiConfig {
    /// Bind address for `rantaiclaw ui start` (default: `127.0.0.1`).
    #[serde(default = "default_ui_host")]
    pub host: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            host: default_ui_host(),
        }
    }
}

fn default_ui_host() -> String {
    "127.0.0.1".into()
}

fn default_pair_rate_limit() -> u32 {
    10
}

fn default_webhook_rate_limit() -> u32 {
    60
}

/// 600/min ≈ 10/s. The bundled console's own traffic (a 15 s `/status` poll
/// plus panel refreshes) sits far under this, so the limit only bites on
/// something looping.
fn default_api_rate_limit() -> u32 {
    600
}

fn default_idempotency_ttl_secs() -> u64 {
    300
}

fn default_gateway_rate_limit_max_keys() -> usize {
    10_000
}

fn default_gateway_request_timeout_secs() -> u64 {
    300
}

fn default_gateway_idempotency_max_keys() -> usize {
    10_000
}

fn default_true() -> bool {
    true
}

/// Serde default for `Config::default_temperature` — a required field otherwise,
/// so a config omitting it failed to load with "missing field".
fn default_temperature_value() -> f64 {
    0.7
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            port: default_gateway_port(),
            host: default_gateway_host(),
            require_pairing: true,
            allow_public_bind: false,
            paired_tokens: Vec::new(),
            pair_rate_limit_per_minute: default_pair_rate_limit(),
            webhook_rate_limit_per_minute: default_webhook_rate_limit(),
            api_rate_limit_per_minute: default_api_rate_limit(),
            trust_forwarded_headers: false,
            rate_limit_max_keys: default_gateway_rate_limit_max_keys(),
            idempotency_ttl_secs: default_idempotency_ttl_secs(),
            idempotency_max_keys: default_gateway_idempotency_max_keys(),
            request_timeout_secs: default_gateway_request_timeout_secs(),
            login: GatewayLoginConfig::default(),
        }
    }
}

// ── Composio (managed tool surface) ─────────────────────────────

/// Composio managed OAuth tools integration (`[composio]` section).
///
/// Provides access to 1000+ OAuth-connected tools via the Composio platform.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ComposioConfig {
    /// Enable Composio integration for 1000+ OAuth tools
    #[serde(default, alias = "enable")]
    pub enabled: bool,
    /// Composio API key (stored encrypted when secrets.encrypt = true)
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default entity ID for multi-user setups
    #[serde(default = "default_entity_id")]
    pub entity_id: String,
}

fn default_entity_id() -> String {
    "default".into()
}

impl Default for ComposioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            entity_id: default_entity_id(),
        }
    }
}

// ── Knowledge Base (encrypted credentials) ──────────────────────

/// Knowledge Base credentials (`[knowledge]`). Keys are encrypted at rest, like
/// `api_key`. Env vars `KB_EMBEDDING_API_KEY` / `KB_EXTRACT_VISION_API_KEY`
/// override these at load (see `apply_env_overrides`), matching api_key
/// precedence (env wins). Not feature-gated — always present so the schema is
/// stable across feature sets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct KnowledgeConfig {
    /// Whether the Knowledge Base is active. `false` (the default) means the
    /// agent is not told the KB exists, the `/api/v1/kb/*` routes report
    /// `kb_disabled`, and the console shows an activation screen. Turning it
    /// off does NOT clear the credentials below — reactivation is one click.
    /// Deleting a key is a separate, explicit action. (Named `enabled`, not
    /// login/logout — `login` is already a provisioner name.)
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub embedding_api_key: Option<String>,
    #[serde(default)]
    pub vision_api_key: Option<String>,
}

// ── Secrets (encrypted credential store) ────────────────────────

/// Secrets encryption configuration (`[secrets]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretsConfig {
    /// Enable encryption for API keys and tokens in config.toml
    #[serde(default = "default_true")]
    pub encrypt: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self { encrypt: true }
    }
}

// ── Browser (friendly-service browsing only) ───────────────────

/// Computer-use sidecar configuration (`[browser.computer_use]` section).
///
/// Delegates OS-level mouse, keyboard, and screenshot actions to a local sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BrowserComputerUseConfig {
    /// Sidecar endpoint for computer-use actions (OS-level mouse/keyboard/screenshot)
    #[serde(default = "default_browser_computer_use_endpoint")]
    pub endpoint: String,
    /// Optional bearer token for computer-use sidecar
    #[serde(default)]
    pub api_key: Option<String>,
    /// Per-action request timeout in milliseconds
    #[serde(default = "default_browser_computer_use_timeout_ms")]
    pub timeout_ms: u64,
    /// Allow remote/public endpoint for computer-use sidecar (default: false)
    #[serde(default)]
    pub allow_remote_endpoint: bool,
    /// Optional window title/process allowlist forwarded to sidecar policy
    #[serde(default)]
    pub window_allowlist: Vec<String>,
    /// Optional X-axis boundary for coordinate-based actions
    #[serde(default)]
    pub max_coordinate_x: Option<i64>,
    /// Optional Y-axis boundary for coordinate-based actions
    #[serde(default)]
    pub max_coordinate_y: Option<i64>,
}

fn default_browser_computer_use_endpoint() -> String {
    "http://127.0.0.1:8787/v1/actions".into()
}

fn default_browser_computer_use_timeout_ms() -> u64 {
    15_000
}

impl Default for BrowserComputerUseConfig {
    fn default() -> Self {
        Self {
            endpoint: default_browser_computer_use_endpoint(),
            api_key: None,
            timeout_ms: default_browser_computer_use_timeout_ms(),
            allow_remote_endpoint: false,
            window_allowlist: Vec::new(),
            max_coordinate_x: None,
            max_coordinate_y: None,
        }
    }
}

/// Browser automation configuration (`[browser]` section).
///
/// Controls the `browser_open` tool and browser automation backends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BrowserConfig {
    /// Enable `browser_open` tool (opens URLs in Brave without scraping).
    /// Default `true` (usable-by-default) — must match `impl Default`, which is
    /// the intended contract declared by the v8→v9 migration.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for `browser_open` (exact or subdomain match)
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Browser session name (for agent-browser automation)
    #[serde(default)]
    pub session_name: Option<String>,
    /// Browser automation backend: "agent_browser" | "computer_use" | "auto"
    #[serde(default = "default_browser_backend")]
    pub backend: String,
    /// Computer-use sidecar configuration
    #[serde(default)]
    pub computer_use: BrowserComputerUseConfig,
}

fn default_browser_backend() -> String {
    "agent_browser".into()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_domains: Vec::new(),
            session_name: None,
            backend: default_browser_backend(),
            computer_use: BrowserComputerUseConfig::default(),
        }
    }
}

// ── HTTP request tool ───────────────────────────────────────────

/// HTTP request tool configuration (`[http_request]` section).
///
/// Easy-mode default: enabled with `allowed_domains = ["*"]` (allow-all wildcard).
/// If `enabled` is true but `allowed_domains` is empty, all HTTP requests are
/// rejected (the guard protects users who enable without configuring a list).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HttpRequestConfig {
    /// Enable `http_request` tool for API interactions. Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allowed domains for HTTP requests (exact or subdomain match). Default
    /// `["*"]` (allow-all wildcard) — omitting it must NOT yield `[]`, which
    /// rejects every request.
    #[serde(default = "default_wildcard_domains")]
    pub allowed_domains: Vec<String>,
    /// Maximum response size in bytes (default: 5 MiB)
    #[serde(default = "default_http_max_response_size")]
    pub max_response_size: usize,
    /// Request timeout in seconds (default: 20)
    #[serde(default = "default_http_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_wildcard_domains() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_http_max_response_size() -> usize {
    5_242_880 // 5 MiB
}

fn default_http_timeout_secs() -> u64 {
    20
}

impl Default for HttpRequestConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            allowed_domains: default_wildcard_domains(),
            max_response_size: default_http_max_response_size(),
            timeout_secs: default_http_timeout_secs(),
        }
    }
}

// ── Web search ───────────────────────────────────────────────────

/// Web search tool configuration (`[web_search]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebSearchConfig {
    /// Enable `web_search_tool` for web searches. Default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Search provider: "duckduckgo" (free, no API key), "brave" (requires API key),
    /// or "searxng" (auto-launched via `[services.searxng]` or pointed at a custom URL).
    #[serde(default = "default_web_search_provider")]
    pub provider: String,
    /// Brave Search API key (required if provider is "brave")
    #[serde(default)]
    pub brave_api_key: Option<String>,
    /// SearXNG endpoint URL — only consulted when `provider = "searxng"` and
    /// `[services.searxng] auto_launch = false`. When auto-launch is on, the
    /// supervised container's URL takes precedence over this field.
    #[serde(default)]
    pub searxng_url: Option<String>,
    /// Maximum results per search (1-10)
    #[serde(default = "default_web_search_max_results")]
    pub max_results: usize,
    /// Request timeout in seconds
    #[serde(default = "default_web_search_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_web_search_provider() -> String {
    "duckduckgo".into()
}

fn default_web_search_max_results() -> usize {
    5
}

fn default_web_search_timeout_secs() -> u64 {
    15
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: default_web_search_provider(),
            brave_api_key: None,
            searxng_url: None,
            max_results: default_web_search_max_results(),
            timeout_secs: default_web_search_timeout_secs(),
        }
    }
}

// ── Auto-managed external services ──────────────────────────────

/// Top-level container for opt-in auto-managed dependencies.
/// Each service is `Option<...>` and absent by default — the daemon only spawns
/// services that the user has explicitly opted into with `auto_launch = true`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ServicesConfig {
    /// SearXNG meta-search engine, run as a local Docker container.
    /// Powers `[web_search] provider = "searxng"` without the user pasting a URL.
    #[serde(default)]
    pub searxng: Option<SearxngServiceConfig>,
}

/// SearXNG service launcher.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearxngServiceConfig {
    /// Spawn the container at daemon boot. Defaults to false — must be opted into.
    #[serde(default)]
    pub auto_launch: bool,
    /// Local port on 127.0.0.1 to bind. Default 8888.
    #[serde(default = "default_searxng_port")]
    pub port: u16,
    /// Docker image to run. Default `searxng/searxng:latest`.
    #[serde(default = "default_searxng_image")]
    pub image: String,
}

fn default_searxng_port() -> u16 {
    8888
}

fn default_searxng_image() -> String {
    "searxng/searxng:latest".into()
}

impl Default for SearxngServiceConfig {
    fn default() -> Self {
        Self {
            auto_launch: false,
            port: default_searxng_port(),
            image: default_searxng_image(),
        }
    }
}

// ── Proxy ───────────────────────────────────────────────────────

/// Proxy application scope — determines which outbound traffic uses the proxy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProxyScope {
    /// Use system environment proxy variables only.
    Environment,
    /// Apply proxy to all RantaiClaw-managed HTTP traffic (default).
    #[default]
    Rantaiclaw,
    /// Apply proxy only to explicitly listed service selectors.
    Services,
}

/// Proxy configuration for outbound HTTP/HTTPS/SOCKS5 traffic (`[proxy]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProxyConfig {
    /// Enable proxy support for selected scope.
    #[serde(default)]
    pub enabled: bool,
    /// Proxy URL for HTTP requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub http_proxy: Option<String>,
    /// Proxy URL for HTTPS requests (supports http, https, socks5, socks5h).
    #[serde(default)]
    pub https_proxy: Option<String>,
    /// Fallback proxy URL for all schemes.
    #[serde(default)]
    pub all_proxy: Option<String>,
    /// No-proxy bypass list. Same format as NO_PROXY.
    #[serde(default)]
    pub no_proxy: Vec<String>,
    /// Proxy application scope.
    #[serde(default)]
    pub scope: ProxyScope,
    /// Service selectors used when scope = "services".
    #[serde(default)]
    pub services: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: Vec::new(),
            scope: ProxyScope::Rantaiclaw,
            services: Vec::new(),
        }
    }
}

impl ProxyConfig {
    pub fn supported_service_keys() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_KEYS
    }

    pub fn supported_service_selectors() -> &'static [&'static str] {
        SUPPORTED_PROXY_SERVICE_SELECTORS
    }

    pub fn has_any_proxy_url(&self) -> bool {
        normalize_proxy_url_option(self.http_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.https_proxy.as_deref()).is_some()
            || normalize_proxy_url_option(self.all_proxy.as_deref()).is_some()
    }

    pub fn normalized_services(&self) -> Vec<String> {
        normalize_service_list(self.services.clone())
    }

    pub fn normalized_no_proxy(&self) -> Vec<String> {
        normalize_no_proxy_list(self.no_proxy.clone())
    }

    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("http_proxy", self.http_proxy.as_deref()),
            ("https_proxy", self.https_proxy.as_deref()),
            ("all_proxy", self.all_proxy.as_deref()),
        ] {
            if let Some(url) = normalize_proxy_url_option(value) {
                validate_proxy_url(field, &url)?;
            }
        }

        for selector in self.normalized_services() {
            if !is_supported_proxy_service_selector(&selector) {
                anyhow::bail!(
                    "Unsupported proxy service selector '{selector}'. Use tool `proxy_config` action `list_services` for valid values"
                );
            }
        }

        if self.enabled && !self.has_any_proxy_url() {
            anyhow::bail!(
                "Proxy is enabled but no proxy URL is configured. Set at least one of http_proxy, https_proxy, or all_proxy"
            );
        }

        if self.enabled
            && self.scope == ProxyScope::Services
            && self.normalized_services().is_empty()
        {
            anyhow::bail!(
                "proxy.scope='services' requires a non-empty proxy.services list when proxy is enabled"
            );
        }

        Ok(())
    }

    pub fn should_apply_to_service(&self, service_key: &str) -> bool {
        if !self.enabled {
            return false;
        }

        match self.scope {
            ProxyScope::Environment => false,
            ProxyScope::Rantaiclaw => true,
            ProxyScope::Services => {
                let service_key = service_key.trim().to_ascii_lowercase();
                if service_key.is_empty() {
                    return false;
                }

                self.normalized_services()
                    .iter()
                    .any(|selector| service_selector_matches(selector, &service_key))
            }
        }
    }

    /// Every proxy this configuration wants applied for `service_key`.
    ///
    /// Extracted so the async and blocking client builders below cannot drift:
    /// `reqwest::Proxy` is the same type for both, so the decision — which URLs,
    /// which `no_proxy` — is made once here and only the `.proxy()` call differs.
    /// WhatsApp Web needs a blocking client for its streaming media download,
    /// and a second hand-rolled copy of this logic is how that channel would
    /// quietly stop honouring `[proxy]` again.
    fn proxies_for(&self, service_key: &str) -> Vec<reqwest::Proxy> {
        if !self.should_apply_to_service(service_key) {
            return Vec::new();
        }

        let no_proxy = self.no_proxy_value();
        let mut proxies = Vec::new();

        for (raw, kind, build) in [
            (
                self.all_proxy.as_deref(),
                "all_proxy",
                (|u: &str| reqwest::Proxy::all(u)) as fn(&str) -> reqwest::Result<reqwest::Proxy>,
            ),
            (
                self.http_proxy.as_deref(),
                "http_proxy",
                (|u: &str| reqwest::Proxy::http(u)) as fn(&str) -> reqwest::Result<reqwest::Proxy>,
            ),
            (
                self.https_proxy.as_deref(),
                "https_proxy",
                (|u: &str| reqwest::Proxy::https(u)) as fn(&str) -> reqwest::Result<reqwest::Proxy>,
            ),
        ] {
            let Some(url) = normalize_proxy_url_option(raw) else {
                continue;
            };
            match build(&url) {
                Ok(proxy) => proxies.push(apply_no_proxy(proxy, no_proxy.clone())),
                Err(error) => tracing::warn!(
                    proxy_url = %url,
                    service_key,
                    "Ignoring invalid {kind} URL: {error}"
                ),
            }
        }

        proxies
    }

    pub fn apply_to_reqwest_builder(
        &self,
        mut builder: reqwest::ClientBuilder,
        service_key: &str,
    ) -> reqwest::ClientBuilder {
        for proxy in self.proxies_for(service_key) {
            builder = builder.proxy(proxy);
        }
        builder
    }

    /// The blocking counterpart, for a caller that needs a synchronous reader
    /// over the response body. WhatsApp Web's media download is the live case:
    /// `wa-rs` streams the encrypted body straight into a writer from inside
    /// `spawn_blocking`, so buffering it to satisfy an async client would trade
    /// the proxy fix for a memory regression on large media.
    pub fn apply_to_blocking_reqwest_builder(
        &self,
        mut builder: reqwest::blocking::ClientBuilder,
        service_key: &str,
    ) -> reqwest::blocking::ClientBuilder {
        for proxy in self.proxies_for(service_key) {
            builder = builder.proxy(proxy);
        }
        builder
    }

    /// Publish this configuration to the process environment.
    ///
    /// **`NO_PROXY` first, deliberately.** These are process-global and read by
    /// every HTTP client in the process, including ones already constructed and
    /// ones other threads build while this runs. Setting the proxies first left
    /// a window in which traffic that should bypass them did not — briefly, but
    /// on a live process that window is real requests. The exemption list has to
    /// be in place before the thing it exempts from.
    ///
    /// The same ordering makes it safe to call repeatedly: a config that widens
    /// `no_proxy` never has a moment where the old, narrower list is paired with
    /// the new proxies.
    pub fn apply_to_process_env(&self) {
        for (key, value) in self.process_env_assignments() {
            set_proxy_env_pair(key, value.as_deref());
            // Track which vars we wrote so a reload does not read them back as a
            // user proxy signal, and so `clear_authored_process_env` can undo
            // exactly our own writes without touching a user's HTTP_PROXY.
            mark_proxy_var_authored(key, value.is_some());
        }
    }

    /// Clear ONLY the proxy env vars this process previously authored (via
    /// [`apply_to_process_env`]). Used when the proxy is disabled or its scope
    /// moves away from `Environment`, so a disabled proxy stops proxying without
    /// wiping a proxy var the operator set in their own shell.
    pub fn clear_authored_process_env() {
        let authored: Vec<&'static str> = {
            let lock = proxy_authored_vars();
            let mut guard = lock
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.drain().collect()
        };
        for key in authored {
            clear_proxy_env_pair(key);
        }
    }

    /// The variables [`apply_to_process_env`](Self::apply_to_process_env) writes,
    /// **in the order it writes them**.
    ///
    /// Returned rather than applied inline so the ordering is a value a test can
    /// assert. It is a temporal property otherwise — observable only by racing
    /// another thread against it, which is the kind of test that flakes instead
    /// of failing.
    fn process_env_assignments(&self) -> Vec<(&'static str, Option<String>)> {
        let no_proxy_joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };

        vec![
            // First, always. See the doc comment above.
            ("NO_PROXY", no_proxy_joined),
            ("HTTP_PROXY", self.http_proxy.clone()),
            ("HTTPS_PROXY", self.https_proxy.clone()),
            ("ALL_PROXY", self.all_proxy.clone()),
        ]
    }

    pub fn clear_process_env() {
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
            clear_proxy_env_pair(key);
            mark_proxy_var_authored(key, false);
        }
    }

    fn no_proxy_value(&self) -> Option<reqwest::NoProxy> {
        let joined = {
            let list = self.normalized_no_proxy();
            (!list.is_empty()).then(|| list.join(","))
        };
        joined.as_deref().and_then(reqwest::NoProxy::from_string)
    }
}

fn apply_no_proxy(proxy: reqwest::Proxy, no_proxy: Option<reqwest::NoProxy>) -> reqwest::Proxy {
    proxy.no_proxy(no_proxy)
}

fn normalize_proxy_url_option(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn normalize_no_proxy_list(values: Vec<String>) -> Vec<String> {
    normalize_comma_values(values)
}

fn normalize_service_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = normalize_comma_values(values)
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_comma_values(values: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for value in values {
        for part in value.split(',') {
            let normalized = part.trim();
            if normalized.is_empty() {
                continue;
            }
            output.push(normalized.to_string());
        }
    }
    output.sort_unstable();
    output.dedup();
    output
}

fn is_supported_proxy_service_selector(selector: &str) -> bool {
    if SUPPORTED_PROXY_SERVICE_KEYS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
    {
        return true;
    }

    SUPPORTED_PROXY_SERVICE_SELECTORS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(selector))
}

fn service_selector_matches(selector: &str, service_key: &str) -> bool {
    if selector == service_key {
        return true;
    }

    if let Some(prefix) = selector.strip_suffix(".*") {
        return service_key.starts_with(prefix)
            && service_key
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('.'));
    }

    false
}

fn validate_proxy_url(field: &str, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .with_context(|| format!("Invalid {field} URL: '{url}' is not a valid URL"))?;

    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        scheme => {
            anyhow::bail!(
                "Invalid {field} URL scheme '{scheme}'. Allowed: http, https, socks5, socks5h"
            );
        }
    }

    if parsed.host_str().is_none() {
        anyhow::bail!("Invalid {field} URL: host is required");
    }

    Ok(())
}

fn set_proxy_env_pair(key: &str, value: Option<&str>) {
    let lowercase_key = key.to_ascii_lowercase();
    if let Some(value) = value.and_then(|candidate| normalize_proxy_url_option(Some(candidate))) {
        std::env::set_var(key, &value);
        std::env::set_var(lowercase_key, value);
    } else {
        std::env::remove_var(key);
        std::env::remove_var(lowercase_key);
    }
}

fn clear_proxy_env_pair(key: &str) {
    std::env::remove_var(key);
    std::env::remove_var(key.to_ascii_lowercase());
}

fn proxy_authored_vars() -> &'static RwLock<HashSet<&'static str>> {
    PROXY_AUTHORED_VARS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Record (or clear) that this process authored the proxy env var `key`.
fn mark_proxy_var_authored(key: &'static str, authored: bool) {
    let lock = proxy_authored_vars();
    let mut guard = lock
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if authored {
        guard.insert(key);
    } else {
        guard.remove(key);
    }
}

/// Whether the generic proxy env var `key` currently holds a value THIS process
/// wrote (so it must not be read back as a user-supplied proxy signal).
fn proxy_var_was_authored(key: &str) -> bool {
    proxy_authored_vars()
        .read()
        .map(|set| set.contains(key))
        .unwrap_or(false)
}

/// Read a proxy URL from the explicit `RANTAICLAW_*` var, else the generic var —
/// but skip the generic var when we authored it ourselves, so a self-written
/// value cannot be mistaken for a user signal that resurrects a disabled proxy.
fn read_user_proxy_var(explicit: &str, generic: &'static str) -> Option<String> {
    if let Ok(value) = std::env::var(explicit) {
        return Some(value);
    }
    if proxy_var_was_authored(generic) {
        return None;
    }
    std::env::var(generic).ok()
}

fn runtime_proxy_state() -> &'static RwLock<ProxyConfig> {
    RUNTIME_PROXY_CONFIG.get_or_init(|| RwLock::new(ProxyConfig::default()))
}

fn runtime_proxy_client_cache() -> &'static RwLock<HashMap<String, reqwest::Client>> {
    RUNTIME_PROXY_CLIENT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn clear_runtime_proxy_client_cache() {
    match runtime_proxy_client_cache().write() {
        Ok(mut guard) => {
            guard.clear();
        }
        Err(poisoned) => {
            poisoned.into_inner().clear();
        }
    }
}

fn runtime_proxy_cache_key(
    service_key: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> String {
    format!(
        "{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
}

fn runtime_proxy_cached_client(cache_key: &str) -> Option<reqwest::Client> {
    match runtime_proxy_client_cache().read() {
        Ok(guard) => guard.get(cache_key).cloned(),
        Err(poisoned) => poisoned.into_inner().get(cache_key).cloned(),
    }
}

fn set_runtime_proxy_cached_client(cache_key: String, client: reqwest::Client) {
    match runtime_proxy_client_cache().write() {
        Ok(mut guard) => {
            guard.insert(cache_key, client);
        }
        Err(poisoned) => {
            poisoned.into_inner().insert(cache_key, client);
        }
    }
}

pub fn set_runtime_proxy_config(config: ProxyConfig) {
    match runtime_proxy_state().write() {
        Ok(mut guard) => {
            *guard = config;
        }
        Err(poisoned) => {
            *poisoned.into_inner() = config;
        }
    }

    clear_runtime_proxy_client_cache();
}

pub fn runtime_proxy_config() -> ProxyConfig {
    match runtime_proxy_state().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

pub fn apply_runtime_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
) -> reqwest::ClientBuilder {
    runtime_proxy_config().apply_to_reqwest_builder(builder, service_key)
}

pub fn apply_runtime_proxy_to_blocking_builder(
    builder: reqwest::blocking::ClientBuilder,
    service_key: &str,
) -> reqwest::blocking::ClientBuilder {
    runtime_proxy_config().apply_to_blocking_reqwest_builder(builder, service_key)
}

/// Default per-request timeout (seconds) for the unparameterised
/// [`build_runtime_proxy_client`]. 120 matches the provider HTTP clients
/// (`openai`, `anthropic`, `copilot`, `openrouter`, `bedrock`, `glm`,
/// `gemini`, `compatible`) — the channel callers below (`telegram`,
/// `discord`, `slack`, `mattermost`, `lark`, `dingtalk`, `qq`, `whatsapp`,
/// `whatsapp_http`, `tunnel.custom`, `tool.browser`, `memory.embeddings`)
/// each send one request at a time and would not benefit from a shorter
/// bound, while a hung upstream must not hold the dispatch loop forever.
/// A previous version of this builder left the timeout unbounded, so a
/// silent peer could pin the listener indefinitely.
pub(crate) const DEFAULT_PROXY_REQUEST_TIMEOUT_SECS: u64 = 120;

/// Default connect timeout (seconds). 10s matches the providers above and
/// leaves room for a cold TLS handshake before the per-request timer
/// starts ticking.
pub(crate) const DEFAULT_PROXY_CONNECT_TIMEOUT_SECS: u64 = 10;

pub fn build_runtime_proxy_client(service_key: &str) -> reqwest::Client {
    build_runtime_proxy_client_with_timeouts(
        service_key,
        DEFAULT_PROXY_REQUEST_TIMEOUT_SECS,
        DEFAULT_PROXY_CONNECT_TIMEOUT_SECS,
    )
}

pub fn build_runtime_proxy_client_with_timeouts(
    service_key: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    let cache_key =
        runtime_proxy_cache_key(service_key, Some(timeout_secs), Some(connect_timeout_secs));
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs));
    let builder = apply_runtime_proxy_to_builder(builder, service_key);
    let client = builder.build().unwrap_or_else(|error| {
        tracing::warn!(
            service_key,
            "Failed to build proxied timeout client: {error}"
        );
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

/// Like `build_runtime_proxy_client_with_timeouts`, but also disables
/// redirect-following (`Policy::none()`), for callers that need SSRF-safe
/// no-redirect behavior (e.g. `tool.http_request`).
///
/// The cache key appends a `|redirect=none` discriminator on top of the
/// existing `service_key|timeout|connect_timeout` key so a no-redirect
/// client never aliases with a normal client built for the same
/// service/timeouts via `build_runtime_proxy_client_with_timeouts`.
pub fn build_runtime_proxy_client_no_redirect(
    service_key: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    let cache_key = format!(
        "{}|redirect=none",
        runtime_proxy_cache_key(service_key, Some(timeout_secs), Some(connect_timeout_secs))
    );
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .redirect(reqwest::redirect::Policy::none());
    let builder = apply_runtime_proxy_to_builder(builder, service_key);
    let client = builder.build().unwrap_or_else(|error| {
        tracing::warn!(
            service_key,
            "Failed to build proxied no-redirect client: {error}"
        );
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

fn parse_proxy_scope(raw: &str) -> Option<ProxyScope> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "environment" | "env" => Some(ProxyScope::Environment),
        "rantaiclaw" | "internal" | "core" => Some(ProxyScope::Rantaiclaw),
        "services" | "service" => Some(ProxyScope::Services),
        _ => None,
    }
}

/// Parse an env-var boolean, tolerant of the common spellings. Returns `None`
/// for anything unrecognized so callers can WARN and keep the config value —
/// the old inline `val == "1" || val == "true"` silently turned `yes`/`on` into
/// `false`, so `WEB_SEARCH_ENABLED=yes` DISABLED web search.
fn parse_env_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
// ── Memory ───────────────────────────────────────────────────

/// Memory backend configuration (`[memory]` section).
///
/// Controls conversation memory storage, embeddings, hybrid search, response caching,
/// and memory snapshot/hydration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)]
// A partial `[memory]` section (e.g. only `backend = "markdown"`) fills the
// remaining fields from `impl Default` instead of failing with "missing field".
#[serde(default)]
pub struct MemoryConfig {
    /// "sqlite" | "none" (`none` = explicit no-op memory)
    pub backend: String,
    /// Auto-save user-stated conversation input to memory (assistant output is excluded)
    pub auto_save: bool,
    /// Run memory/session hygiene (archiving + retention cleanup)
    #[serde(default = "default_hygiene_enabled")]
    pub hygiene_enabled: bool,
    /// Archive session files older than this many days (daily memory files are left alone)
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u32,
    /// Purge archived session files older than this many days
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u32,
    /// For sqlite backend: prune conversation rows older than this many days
    #[serde(default = "default_conversation_retention_days")]
    pub conversation_retention_days: u32,
    /// Embedding provider: "none" | "openai" | "custom:URL"
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Embedding model name (e.g. "text-embedding-3-small")
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Embedding vector dimensions
    #[serde(default = "default_embedding_dims")]
    pub embedding_dimensions: usize,
    /// Weight for vector similarity in hybrid search (0.0–1.0)
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    /// Weight for keyword BM25 in hybrid search (0.0–1.0)
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    /// Minimum score (0.0–1.0) for a memory to be included in context. The
    /// score is absolute: the share of the question's meaningful words (whole
    /// words, stopwords dropped) that the memory contains, blended with the
    /// vector score when embeddings are on. Memories scoring below this
    /// threshold are dropped to prevent irrelevant context from bleeding into
    /// conversations. Default: 0.6
    #[serde(default = "default_min_relevance_score")]
    pub min_relevance_score: f64,
    /// Max embedding cache entries before LRU eviction
    #[serde(default = "default_cache_size")]
    pub embedding_cache_size: usize,

    // ── Response Cache (saves tokens on repeated prompts) ──────
    /// Enable LLM response caching to avoid paying for duplicate prompts
    /// TTL in minutes for cached responses (default: 60)
    /// Max number of cached responses before LRU eviction (default: 5000)

    // ── Memory Snapshot (soul backup to Markdown) ─────────────
    /// Enable periodic export of core memories to MEMORY_SNAPSHOT.md
    #[serde(default)]
    pub snapshot_enabled: bool,
    /// Run snapshot during hygiene passes (heartbeat-driven)
    #[serde(default)]
    pub snapshot_on_hygiene: bool,
    /// Auto-hydrate from MEMORY_SNAPSHOT.md when brain.db is missing
    #[serde(default = "default_true")]
    pub auto_hydrate: bool,

    // ── SQLite backend options ─────────────────────────────────
    /// For sqlite backend: max seconds to wait when opening the DB (e.g. file locked).
    /// None = wait indefinitely (default). Recommended max: 300.
    #[serde(default)]
    pub sqlite_open_timeout_secs: Option<u64>,
}

fn default_embedding_provider() -> String {
    "none".into()
}
fn default_hygiene_enabled() -> bool {
    true
}
fn default_archive_after_days() -> u32 {
    7
}
fn default_purge_after_days() -> u32 {
    30
}
fn default_conversation_retention_days() -> u32 {
    30
}
fn default_embedding_model() -> String {
    "text-embedding-3-small".into()
}
fn default_embedding_dims() -> usize {
    1536
}
fn default_vector_weight() -> f64 {
    0.7
}
fn default_keyword_weight() -> f64 {
    0.3
}
fn default_min_relevance_score() -> f64 {
    0.6
}
fn default_cache_size() -> usize {
    10_000
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "sqlite".into(),
            auto_save: true,
            hygiene_enabled: default_hygiene_enabled(),
            archive_after_days: default_archive_after_days(),
            purge_after_days: default_purge_after_days(),
            conversation_retention_days: default_conversation_retention_days(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_dimensions: default_embedding_dims(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            min_relevance_score: default_min_relevance_score(),
            embedding_cache_size: default_cache_size(),
            snapshot_enabled: false,
            snapshot_on_hygiene: false,
            auto_hydrate: true,
            sqlite_open_timeout_secs: None,
        }
    }
}

// ── Observability ─────────────────────────────────────────────────

/// Observability backend configuration (`[observability]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
// Partial `[observability]` fills missing fields from `impl Default`.
#[serde(default)]
pub struct ObservabilityConfig {
    /// "none" | "log" | "prometheus" | "otel"
    pub backend: String,

    /// OTLP endpoint (e.g. "http://localhost:4318"). Only used when backend = "otel".
    #[serde(default)]
    pub otel_endpoint: Option<String>,

    /// Service name reported to the OTel collector. Defaults to "rantaiclaw".
    #[serde(default)]
    pub otel_service_name: Option<String>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            backend: "none".into(),
            otel_endpoint: None,
            otel_service_name: None,
        }
    }
}

// ── Autonomy / Security ──────────────────────────────────────────

/// Autonomy and security policy configuration (`[autonomy]` section).
///
/// Controls what the agent is allowed to do: shell commands, filesystem access,
/// risk approval gates, and per-policy budgets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
// Struct-level default so a PARTIAL `[autonomy]` section (e.g. only
// `level = "full"`, exactly what the docs teach) fills the remaining fields
// from `impl Default` instead of failing the whole load with "missing field".
#[serde(default)]
pub struct AutonomyConfig {
    /// Autonomy level: `readonly`, `supervised` (default), or `full`.
    pub level: AutonomyLevel,
    /// Restrict file writes and command paths to the workspace directory. Default: `true`.
    pub workspace_only: bool,
    /// Allowlist of executable names permitted for shell execution.
    pub allowed_commands: Vec<String>,
    /// Explicit path denylist. Default includes system-critical paths.
    pub forbidden_paths: Vec<String>,
    /// Maximum tool actions per hour per policy — the primary runaway guard
    /// actually enforced in the agent loop. Default: `200`.
    pub max_actions_per_hour: u32,
    /// Require explicit approval for medium-risk shell commands.
    #[serde(default = "default_true")]
    pub require_approval_for_medium_risk: bool,

    /// Block high-risk shell commands even if allowlisted. Default `false`
    /// (usable-by-default local shell; the v8→v9 migration declared this intent)
    /// — inherits the struct-level `#[serde(default)]`, which resolves to
    /// `impl Default`'s `false`, instead of the stale `default_true`.
    pub block_high_risk_commands: bool,

    /// Tools that never require approval (e.g. read-only tools).
    #[serde(default = "default_auto_approve")]
    pub auto_approve: Vec<String>,

    /// Tools that always require interactive approval, even after "Always".
    #[serde(default = "default_always_ask")]
    pub always_ask: Vec<String>,
}

fn default_auto_approve() -> Vec<String> {
    vec!["file_read".into(), "memory_recall".into()]
}

fn default_always_ask() -> Vec<String> {
    // High-blast-radius remote-install tools always prompt, even after a
    // session "Always" (no effect when the remote-install feature is off).
    vec!["ssh".to_string(), "pty".to_string()]
}

impl Default for AutonomyConfig {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Supervised,
            workspace_only: true,
            allowed_commands: vec![
                "git".into(),
                "npm".into(),
                "cargo".into(),
                "ls".into(),
                "cat".into(),
                "grep".into(),
                "find".into(),
                "echo".into(),
                "pwd".into(),
                "wc".into(),
                "head".into(),
                "tail".into(),
            ],
            forbidden_paths: vec![
                "/etc".into(),
                "/root".into(),
                "/home".into(),
                "/usr".into(),
                "/bin".into(),
                "/sbin".into(),
                "/lib".into(),
                "/opt".into(),
                "/boot".into(),
                "/dev".into(),
                "/proc".into(),
                "/sys".into(),
                "/var".into(),
                "/tmp".into(),
                "~/.ssh".into(),
                "~/.gnupg".into(),
                "~/.aws".into(),
                "~/.config".into(),
            ],
            max_actions_per_hour: 200,
            require_approval_for_medium_risk: true,
            block_high_risk_commands: false,
            auto_approve: default_auto_approve(),
            always_ask: default_always_ask(),
        }
    }
}

// ── Runtime ──────────────────────────────────────────────────────

/// Runtime adapter configuration (`[runtime]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeConfig {
    /// Runtime kind (`native` | `docker`).
    #[serde(default = "default_runtime_kind")]
    pub kind: String,

    /// Docker runtime settings (used when `kind = "docker"`).
    #[serde(default)]
    pub docker: DockerRuntimeConfig,

    /// Global reasoning override for providers that expose explicit controls.
    /// - `None`: provider default behavior
    /// - `Some(true)`: request reasoning/thinking when supported
    /// - `Some(false)`: disable reasoning/thinking when supported
    #[serde(default)]
    pub reasoning_enabled: Option<bool>,
}

/// Docker runtime configuration (`[runtime.docker]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DockerRuntimeConfig {
    /// Runtime image used to execute shell commands.
    #[serde(default = "default_docker_image")]
    pub image: String,

    /// Docker network mode (`none`, `bridge`, etc.).
    #[serde(default = "default_docker_network")]
    pub network: String,

    /// Optional memory limit in MB (`None` = no explicit limit).
    #[serde(default = "default_docker_memory_limit_mb")]
    pub memory_limit_mb: Option<u64>,

    /// Optional CPU limit (`None` = no explicit limit).
    #[serde(default = "default_docker_cpu_limit")]
    pub cpu_limit: Option<f64>,

    /// Mount root filesystem as read-only.
    #[serde(default = "default_true")]
    pub read_only_rootfs: bool,

    /// Mount configured workspace into `/workspace`.
    #[serde(default = "default_true")]
    pub mount_workspace: bool,

    /// Optional workspace root allowlist for Docker mount validation.
    #[serde(default)]
    pub allowed_workspace_roots: Vec<String>,
}

fn default_runtime_kind() -> String {
    "native".into()
}

fn default_docker_image() -> String {
    "alpine:3.20".into()
}

fn default_docker_network() -> String {
    "none".into()
}

fn default_docker_memory_limit_mb() -> Option<u64> {
    Some(512)
}

fn default_docker_cpu_limit() -> Option<f64> {
    Some(1.0)
}

impl Default for DockerRuntimeConfig {
    fn default() -> Self {
        Self {
            image: default_docker_image(),
            network: default_docker_network(),
            memory_limit_mb: default_docker_memory_limit_mb(),
            cpu_limit: default_docker_cpu_limit(),
            read_only_rootfs: true,
            mount_workspace: true,
            allowed_workspace_roots: Vec::new(),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            kind: default_runtime_kind(),
            docker: DockerRuntimeConfig::default(),
            reasoning_enabled: None,
        }
    }
}

// ── Reliability / supervision ────────────────────────────────────

/// Reliability and supervision configuration (`[reliability]` section).
///
/// Controls provider retries, fallback chains, API key rotation, and channel restart backoff.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReliabilityConfig {
    /// Retries per provider before failing over.
    #[serde(default = "default_provider_retries")]
    pub provider_retries: u32,
    /// Base backoff (ms) for provider retry delay.
    #[serde(default = "default_provider_backoff_ms")]
    pub provider_backoff_ms: u64,
    /// Fallback provider chain (e.g. `["anthropic", "openai"]`).
    #[serde(default)]
    pub fallback_providers: Vec<String>,
    /// Per-model fallback chains. When a model fails, try these alternatives in order.
    /// Example: `{ "claude-opus-4-20250514" = ["claude-sonnet-4-20250514", "gpt-4o"] }`
    #[serde(default)]
    pub model_fallbacks: std::collections::HashMap<String, Vec<String>>,
    /// Initial backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_secs")]
    pub channel_initial_backoff_secs: u64,
    /// Max backoff for channel/daemon restarts.
    #[serde(default = "default_channel_backoff_max_secs")]
    pub channel_max_backoff_secs: u64,
    /// Scheduler polling cadence in seconds.
    #[serde(default = "default_scheduler_poll_secs")]
    pub scheduler_poll_secs: u64,
    /// Max retries for cron job execution attempts.
    #[serde(default = "default_scheduler_retries")]
    pub scheduler_retries: u32,
}

fn default_provider_retries() -> u32 {
    // Bumped 2 → 3 so a transient provider/network blip is less likely to fail
    // the whole turn before a response is produced.
    3
}

fn default_provider_backoff_ms() -> u64 {
    500
}

fn default_channel_backoff_secs() -> u64 {
    2
}

fn default_channel_backoff_max_secs() -> u64 {
    60
}

fn default_scheduler_poll_secs() -> u64 {
    15
}

fn default_scheduler_retries() -> u32 {
    2
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            provider_retries: default_provider_retries(),
            provider_backoff_ms: default_provider_backoff_ms(),
            fallback_providers: Vec::new(),
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: default_channel_backoff_secs(),
            channel_max_backoff_secs: default_channel_backoff_max_secs(),
            scheduler_poll_secs: default_scheduler_poll_secs(),
            scheduler_retries: default_scheduler_retries(),
        }
    }
}

// ── Scheduler ────────────────────────────────────────────────────

/// Scheduler configuration for periodic task execution (`[scheduler]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SchedulerConfig {
    /// Enable the built-in scheduler loop.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    /// Maximum number of persisted scheduled tasks.
    #[serde(default = "default_scheduler_max_tasks")]
    pub max_tasks: usize,
    /// Maximum tasks executed per scheduler polling cycle.
    #[serde(default = "default_scheduler_max_concurrent")]
    pub max_concurrent: usize,
}

fn default_scheduler_enabled() -> bool {
    true
}

fn default_scheduler_max_tasks() -> usize {
    64
}

fn default_scheduler_max_concurrent() -> usize {
    4
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduler_enabled(),
            max_tasks: default_scheduler_max_tasks(),
            max_concurrent: default_scheduler_max_concurrent(),
        }
    }
}

// ── Model routing ────────────────────────────────────────────────

/// Route a task hint to a specific provider + model.
///
/// ```toml
/// [[model_routes]]
/// hint = "reasoning"
/// provider = "openrouter"
/// model = "anthropic/claude-opus-4-20250514"
///
/// [[model_routes]]
/// hint = "fast"
/// provider = "groq"
/// model = "llama-3.3-70b-versatile"
/// ```
///
/// Usage: pass `hint:reasoning` as the model parameter to route the request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelRouteConfig {
    /// Task hint name (e.g. "reasoning", "fast", "code", "summarize")
    pub hint: String,
    /// Provider to route to (must match a known provider name)
    pub provider: String,
    /// Model to use with that provider
    pub model: String,
    /// Optional API key override for this route's provider
    #[serde(default)]
    pub api_key: Option<String>,
}

// ── Embedding routing ───────────────────────────────────────────

/// Route an embedding hint to a specific provider + model.
///
/// ```toml
/// [[embedding_routes]]
/// hint = "semantic"
/// provider = "openai"
/// model = "text-embedding-3-small"
/// dimensions = 1536
///
/// [memory]
/// embedding_model = "hint:semantic"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmbeddingRouteConfig {
    /// Route hint name (e.g. "semantic", "archive", "faq")
    pub hint: String,
    /// Embedding provider (`none`, `openai`, or `custom:<url>`)
    pub provider: String,
    /// Embedding model to use with that provider
    pub model: String,
    /// Optional embedding dimension override for this route
    #[serde(default)]
    pub dimensions: Option<usize>,
    /// Optional API key override for this route's provider
    #[serde(default)]
    pub api_key: Option<String>,
}

// ── Query Classification ─────────────────────────────────────────

/// Automatic query classification — classifies user messages by keyword/pattern
/// and routes to the appropriate model hint. Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct QueryClassificationConfig {
    /// Enable automatic query classification. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Classification rules evaluated in priority order.
    #[serde(default)]
    pub rules: Vec<ClassificationRule>,
}

/// A single classification rule mapping message patterns to a model hint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
pub struct ClassificationRule {
    /// Must match a `[[model_routes]]` hint value.
    pub hint: String,
    /// Case-insensitive substring matches.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Case-sensitive literal matches (for "```", "fn ", etc.).
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Only match if message length >= N chars.
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Only match if message length <= N chars.
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Higher priority rules are checked first.
    #[serde(default)]
    pub priority: i32,
}

// ── Heartbeat ────────────────────────────────────────────────────

/// Heartbeat configuration for periodic health pings (`[heartbeat]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
// Partial `[heartbeat]` fills missing fields from `impl Default`.
#[serde(default)]
pub struct HeartbeatConfig {
    /// Enable periodic heartbeat pings. Default: `false`.
    pub enabled: bool,
    /// Interval in minutes between heartbeat pings. Default: `30`.
    pub interval_minutes: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 30,
        }
    }
}

// ── Cron ────────────────────────────────────────────────────────

/// Cron job configuration (`[cron]` section).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CronConfig {
    /// Enable the cron subsystem. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum number of historical cron run records to retain. Default: `50`.
    #[serde(default = "default_max_run_history")]
    pub max_run_history: u32,
    /// Skip (and log) a scheduled run whose `next_run` is older than this many
    /// seconds instead of firing it "late" on restart. The job is not lost — a
    /// recurring schedule re-anchors to its next future occurrence; a one-shot
    /// `at` job is disabled. Firing that does happen is always coalesced to a
    /// single run (missed occurrences are never replayed). `0` disables the gate
    /// (a due job always fires once). Default: `86400` (1 day).
    #[serde(default = "default_max_catchup_age_secs")]
    pub max_catchup_age_secs: u64,
}

fn default_max_run_history() -> u32 {
    50
}

fn default_max_catchup_age_secs() -> u64 {
    86_400 // 1 day
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_run_history: default_max_run_history(),
            max_catchup_age_secs: default_max_catchup_age_secs(),
        }
    }
}

// ── Tasks ──────────────────────────────────────────────────────

/// Task engine configuration (`[tasks]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TasksConfig {
    /// Enable the task engine — the store and the agent's nine task tools.
    /// Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Serve the nine `/tasks*` HTTP routes. Default: **false**.
    ///
    /// Separate from [`Self::enabled`] on purpose: the routes are undocumented,
    /// sit outside the `/api/v1` rate limiter and have no consumer, so they are
    /// not served by default — but turning the engine off to close them would
    /// take the agent's task tools with it.
    #[serde(default)]
    pub api_enabled: bool,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            api_enabled: false,
        }
    }
}

// ── Tunnel ──────────────────────────────────────────────────────

/// Tunnel configuration for exposing the gateway publicly (`[tunnel]` section).
///
/// Supported providers: `"none"` (default), `"cloudflare"`, `"tailscale"`, `"ngrok"`, `"custom"`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TunnelConfig {
    /// Tunnel provider: `"none"`, `"cloudflare"`, `"tailscale"`, `"ngrok"`, or `"custom"`. Default: `"none"`.
    pub provider: String,

    /// Cloudflare Tunnel configuration (used when `provider = "cloudflare"`).
    #[serde(default)]
    pub cloudflare: Option<CloudflareTunnelConfig>,

    /// Tailscale Funnel/Serve configuration (used when `provider = "tailscale"`).
    #[serde(default)]
    pub tailscale: Option<TailscaleTunnelConfig>,

    /// ngrok tunnel configuration (used when `provider = "ngrok"`).
    #[serde(default)]
    pub ngrok: Option<NgrokTunnelConfig>,

    /// Custom tunnel command configuration (used when `provider = "custom"`).
    #[serde(default)]
    pub custom: Option<CustomTunnelConfig>,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            cloudflare: None,
            tailscale: None,
            ngrok: None,
            custom: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloudflareTunnelConfig {
    /// Cloudflare Tunnel token (from Zero Trust dashboard)
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TailscaleTunnelConfig {
    /// Use Tailscale Funnel (public internet) vs Serve (tailnet only)
    #[serde(default)]
    pub funnel: bool,
    /// Optional hostname override
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NgrokTunnelConfig {
    /// ngrok auth token
    pub auth_token: String,
    /// Optional custom domain
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CustomTunnelConfig {
    /// Command template to start the tunnel. Use {port} and {host} placeholders.
    /// Example: "bore local {port} --to bore.pub"
    pub start_command: String,
    /// Optional URL to check tunnel health
    pub health_url: Option<String>,
    /// Optional regex to extract public URL from command stdout
    pub url_pattern: Option<String>,
}

// ── Channels ─────────────────────────────────────────────────────

/// Top-level channel configurations (`[channels_config]` section).
///
/// Each channel sub-section (e.g. `telegram`, `discord`) is optional;
/// setting it to `Some(...)` enables that channel.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
// Partial `[channels_config]` fills missing fields (e.g. `cli`) from
// `impl Default` instead of failing to load.
#[serde(default)]
pub struct ChannelsConfig {
    /// Enable the CLI interactive channel. Default: `true`.
    pub cli: bool,
    /// Telegram bot channel configuration. Support: **supported**. Verification: **verified**.
    pub telegram: Option<TelegramConfig>,
    /// Discord bot channel configuration. Support: **supported**. Verification: **verified**.
    pub discord: Option<DiscordConfig>,
    /// Slack bot channel configuration. Support: **supported**. Verification: **verified**.
    pub slack: Option<SlackConfig>,
    /// Mattermost bot channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub mattermost: Option<MattermostConfig>,
    /// Webhook channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub webhook: Option<WebhookConfig>,
    /// iMessage channel configuration (macOS only). Support: **under development**. Verification: **not yet verified**.
    pub imessage: Option<IMessageConfig>,
    /// Matrix channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub matrix: Option<MatrixConfig>,
    /// Signal channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub signal: Option<SignalConfig>,
    /// WhatsApp Cloud API channel configuration. Support: **supported**. Verification: **not yet verified**.
    pub whatsapp: Option<WhatsAppConfig>,
    /// WhatsApp Web channel configuration. Support: **supported**. Verification: **verified**.
    pub whatsapp_web: Option<WhatsAppWebConfig>,
    /// Linq Partner API channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub linq: Option<LinqConfig>,
    /// Nextcloud Talk bot channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub nextcloud_talk: Option<NextcloudTalkConfig>,
    /// Email channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub email: Option<crate::channels::email_channel::EmailConfig>,
    /// IRC channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub irc: Option<IrcConfig>,
    /// Lark/Feishu channel configuration. Support: **supported**. Verification: **verified**.
    pub lark: Option<LarkConfig>,
    /// DingTalk channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub dingtalk: Option<DingTalkConfig>,
    /// QQ Official Bot channel configuration. Support: **under development**. Verification: **not yet verified**.
    pub qq: Option<QQConfig>,
    /// Base timeout in seconds for processing a single channel message (LLM + tools).
    /// Runtime uses this as a per-turn budget that scales with tool-loop depth
    /// (up to 4x, capped) so one slow/retried model call does not consume the
    /// entire conversation budget.
    /// Default: 300s for on-device LLMs (Ollama) which are slower than cloud APIs.
    #[serde(default = "default_channel_message_timeout_secs")]
    pub message_timeout_secs: u64,
    /// Allow the agent to run tools **unattended** when answering messages that
    /// arrive over the gateway's channel webhooks (Telegram, WhatsApp, Linq,
    /// Nextcloud Talk). There is no interactive approval prompt on a channel, so
    /// by default (`false`) any tool that needs approval at the current autonomy
    /// level is auto-denied. Set `true` to let channel messages execute tools
    /// without prompting — equivalent to how `rantaiclaw channels` (polling)
    /// already behaves. The `shell` tool still enforces its own command
    /// allowlist (`[autonomy]`/SecurityPolicy), so dangerous commands remain
    /// gated even when this is on.
    #[serde(default)]
    pub autonomous_tools: bool,
    /// Reply in-thread where the platform supports it (default `true`).
    ///
    /// Threading changes **where** a reply appears, so an operator must be able
    /// to turn it off without turning off the channel. A per-channel key of the
    /// same name (currently `[channels_config.mattermost]`) overrides this.
    ///
    /// Enforced once, in the inbound dispatch loop, which clears both the
    /// message's thread (`thread_ts`) and the message its reply would quote
    /// (`reply_anchor`) before the agent sees it. Channels do not read this
    /// flag, so a channel added later cannot forget to honour it.
    #[serde(default = "default_thread_replies")]
    pub thread_replies: bool,
    /// Sender ids authorized to **approve** tool calls over a channel (the
    /// people whose `Y` / `A` reply to an in-chat approval prompt is honored).
    ///
    /// This is a SEPARATE, deliberately smaller allowlist than each channel's
    /// `allowed_users` (who may chat with the bot): being able to talk to the
    /// bot does not make you able to approve a privileged tool call. Mirrors
    /// OpenClaw's separate command/owner gate.
    ///
    /// Default empty ⇒ **no one** can approve over a channel, so approval-
    /// required tools stay auto-denied (secure-by-default). `"*"` lets any
    /// sender approve — insecure, opt-in only. Ignored when
    /// `autonomous_tools = true` (which skips the approval gate entirely).
    #[serde(default)]
    pub approval_owners: Vec<String>,
    /// Tools a **normal user** (allowed to chat but NOT in `approval_owners`)
    /// may have the agent use on their behalf. Owners always get the full
    /// toolset; this is the capability ceiling for everyone else.
    ///
    /// Empty (default) ⇒ the agent calls no tool on a guest's behalf; the
    /// guest can still chat. List specific tool names (e.g. `"shell"`,
    /// `"web_search_tool"`) to widen what guests may use. The owner's
    /// `autonomy.auto_approve` list is intentionally **not** unioned in — an
    /// operator who wants a guest to be able to read files or recall memory
    /// must list those tools here.
    #[serde(default)]
    pub guest_allowed_tools: Vec<String>,
    /// Shell-command glob patterns a **normal user** may have the agent run
    /// (only relevant when `"shell"` is in `guest_allowed_tools`). Same glob
    /// matcher as the autonomy command allowlist. Acts as a HARD ceiling for
    /// guests: a command that doesn't match is denied outright (never escalated
    /// to an owner). Owners are unaffected.
    ///
    /// Example: `["kubectl get *", "kubectl describe *", "ls *"]`. Empty
    /// (default) ⇒ guests may run no shell commands even if `shell` is allowed.
    #[serde(default)]
    pub guest_allowed_commands: Vec<String>,
}

fn default_thread_replies() -> bool {
    true
}

fn default_channel_message_timeout_secs() -> u64 {
    // Bumped 300 → 600 so slow models + tool loops have room to finish a turn
    // before the channel drops the response. Still scales up to 4x with depth.
    600
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: None,
            matrix: None,
            signal: None,
            whatsapp: None,
            whatsapp_web: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            message_timeout_secs: default_channel_message_timeout_secs(),
            autonomous_tools: false,
            thread_replies: default_thread_replies(),
            approval_owners: Vec::new(),
            guest_allowed_tools: Vec::new(),
            guest_allowed_commands: Vec::new(),
        }
    }
}

/// Streaming mode for channels that support progressive message updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    /// No streaming -- send the complete response as a single message (default).
    #[default]
    Off,
    /// Update a draft message with every flush interval.
    Partial,
}

fn default_draft_update_interval_ms() -> u64 {
    1000
}

/// Telegram bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TelegramConfig {
    /// Telegram Bot API token (from @BotFather).
    pub bot_token: String,
    /// Allowed Telegram user IDs or usernames. Empty = deny all.
    pub allowed_users: Vec<String>,
    /// Streaming mode for progressive response delivery via message edits.
    #[serde(default)]
    pub stream_mode: StreamMode,
    /// Minimum interval (ms) between draft message edits to avoid rate limits.
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// When true, a newer Telegram message from the same sender in the same chat
    /// cancels the in-flight request and starts a fresh response with preserved history.
    #[serde(default)]
    pub interrupt_on_new_message: bool,
    /// When true, only respond to messages that @-mention the bot in groups.
    /// Direct messages are always processed.
    #[serde(default = "default_true")]
    pub mention_only: bool,
}

/// Discord bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiscordConfig {
    /// Discord bot token (from Discord Developer Portal).
    pub bot_token: String,
    /// Optional guild (server) ID to restrict the bot to a single guild.
    pub guild_id: Option<String>,
    /// Allowed Discord user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true, process messages from other bots (not just humans).
    /// The bot still ignores its own messages to prevent feedback loops.
    #[serde(default)]
    pub listen_to_bots: bool,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the guild are silently ignored.
    #[serde(default = "default_true")]
    pub mention_only: bool,
}

/// Slack bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SlackConfig {
    /// Slack bot OAuth token (xoxb-...).
    pub bot_token: String,
    /// Slack app-level token for Socket Mode (`xapp-...`, scope
    /// `connections:write`).
    ///
    /// With it the channel opens one WebSocket and receives events for every
    /// conversation the bot is in — channels, threads and direct messages.
    /// Without it the channel falls back to polling a single
    /// `conversations.history` page, which sees neither DMs nor thread replies.
    pub app_token: Option<String>,
    /// Channel ID.
    ///
    /// Under Socket Mode this is an optional filter: leave it empty to accept
    /// every conversation. Under the polling fallback it is **required** — that
    /// transport has exactly one conversation to read, and `listen` fails
    /// without it.
    pub channel_id: Option<String>,
    /// Allowed Slack user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

/// Mattermost bot channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MattermostConfig {
    /// Mattermost server URL (e.g. `"https://mattermost.example.com"`).
    pub url: String,
    /// Mattermost bot access token.
    pub bot_token: String,
    /// Optional channel ID to restrict the bot to a single channel.
    pub channel_id: Option<String>,
    /// Allowed Mattermost user IDs. Empty = deny all.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// When true (default), replies thread on the original post.
    /// When false, replies go to the channel root.
    #[serde(default)]
    pub thread_replies: Option<bool>,
    /// When true, only respond to messages that @-mention the bot.
    /// Other messages in the channel are silently ignored.
    #[serde(default)]
    pub mention_only: Option<bool>,
}

/// Webhook channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebhookConfig {
    /// Optional shared secret for webhook signature verification.
    pub secret: Option<String>,
}

/// iMessage channel configuration (macOS only).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IMessageConfig {
    /// Allowed iMessage contacts (phone numbers or email addresses). Empty = deny all.
    pub allowed_contacts: Vec<String>,
}

/// Matrix channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MatrixConfig {
    /// Matrix homeserver URL (e.g. `"https://matrix.org"`).
    pub homeserver: String,
    /// Matrix access token for the bot account.
    pub access_token: String,
    /// Optional Matrix user ID (e.g. `"@bot:matrix.org"`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Optional Matrix device ID.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Matrix room ID to listen in (e.g. `"!abc123:matrix.org"`).
    pub room_id: String,
    /// Allowed Matrix user IDs. Empty = deny all.
    pub allowed_users: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignalConfig {
    /// Base URL for the signal-cli HTTP daemon (e.g. "http://127.0.0.1:8686").
    pub http_url: String,
    /// E.164 phone number of the signal-cli account (e.g. "+1234567890").
    pub account: String,
    /// Optional group ID to filter messages.
    /// - `None` or omitted: accept all messages (DMs and groups)
    /// - `"dm"`: only accept direct messages
    /// - Specific group ID: only accept messages from that group
    #[serde(default)]
    pub group_id: Option<String>,
    /// Allowed sender phone numbers (E.164) or "*" for all.
    #[serde(default)]
    pub allowed_from: Vec<String>,
    /// Skip messages that are attachment-only (no text body).
    #[serde(default)]
    pub ignore_attachments: bool,
    /// Skip incoming story messages.
    #[serde(default)]
    pub ignore_stories: bool,
}

/// WhatsApp Cloud API channel configuration.
///
/// Web mode moved to [`WhatsAppWebConfig`] in schema v32. Before that both
/// transports shared this table and the mode was inferred from which keys
/// happened to be filled, which meant the product guessed at something the
/// operator was never asked to state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WhatsAppConfig {
    /// Access token from Meta Business Suite
    #[serde(default)]
    pub access_token: Option<String>,
    /// Phone number ID from Meta Business API
    #[serde(default)]
    pub phone_number_id: Option<String>,
    /// Webhook verify token (you define this, Meta sends it back for verification)
    #[serde(default)]
    pub verify_token: Option<String>,
    /// App secret from Meta Business Suite (for webhook signature verification)
    /// Can also be set via `RANTAICLAW_WHATSAPP_APP_SECRET` environment variable
    #[serde(default)]
    pub app_secret: Option<String>,
    /// Allowed phone numbers (E.164 format: +1234567890) or "*" for all
    #[serde(default)]
    pub allowed_numbers: Vec<String>,
}

/// WhatsApp Web channel configuration (native client, QR or pair-code linking).
///
/// Split out of [`WhatsAppConfig`] in schema v32. Writing this table is how an
/// operator selects Web mode; there is no mode key and nothing is inferred.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WhatsAppWebConfig {
    /// Session database path for the WhatsApp Web client
    pub session_path: String,
    /// Phone number for pair code linking (optional)
    /// Format: country code + number (e.g., "15551234567")
    /// If not set, QR code pairing will be used
    #[serde(default)]
    pub pair_phone: Option<String>,
    /// Custom pair code for linking (optional)
    /// Leave empty to let WhatsApp generate one
    #[serde(default)]
    pub pair_code: Option<String>,
    /// Allowed phone numbers (E.164 format: +1234567890) or "*" for all
    #[serde(default)]
    pub allowed_numbers: Vec<String>,
}

impl WhatsAppWebConfig {
    /// The `+` form of a number, which is how WhatsApp Web compares a sender
    /// with `allowed_numbers`.
    ///
    /// The channel applies it to the senders and recipients it checks, and the
    /// gateway to the numbers an operator saves, so what is saved is what gets
    /// compared.
    pub fn plus_form(number: &str) -> String {
        let number = number.trim();
        if number.starts_with('+') {
            number.to_string()
        } else {
            format!("+{number}")
        }
    }

    /// One `allowed_numbers` entry as the runtime compares it.
    ///
    /// `*` stays as it is, and so does `lid:<digits>`, the name the channel
    /// gives a sender whose number it cannot see and saves when that sender
    /// pairs. A number gets its `+` form, so `15551234567` typed without the
    /// `+` still matches that sender. A leading `0` is the local-format
    /// trunk prefix; the runtime writes `+E.164`, so `0812…` would never
    /// match and is refused with a sentence that asks for the country code.
    /// Anything else could never match anyone, and is refused with a
    /// sentence that names it.
    pub fn allowlist_entry(entry: &str) -> Result<String, String> {
        let entry = entry.trim();
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if entry == "*" || entry.strip_prefix("lid:").is_some_and(digits) {
            return Ok(entry.to_string());
        }
        let body = entry.strip_prefix('+').unwrap_or(entry);
        if digits(body) {
            if body.starts_with('0') {
                return Err(format!(
                    "`{entry}` starts with 0 — that's the local trunk prefix, not \
                     the country code. Use the country code, like +6281234567890"
                ));
            }
            return Ok(Self::plus_form(entry));
        }
        Err(format!(
            "`{entry}` is not a phone number: use digits with an optional leading +, \
             like +15551234567, or * for everyone"
        ))
    }
}

#[cfg(test)]
mod whatsapp_web_config_tests {
    use super::WhatsAppWebConfig;

    /// F-11. A leading `0` is a local-format digit, not the country code the
    /// runtime normalises to. Saving it locks the channel into deny-all.
    #[test]
    fn allowlist_entry_refuses_a_leading_zero() {
        let err = WhatsAppWebConfig::allowlist_entry("081234567890").expect_err("must refuse");
        assert!(
            err.contains("081234567890"),
            "the bad entry is named: {err}"
        );
        assert!(
            err.contains("country code"),
            "the sentence names the fix: {err}"
        );
    }

    /// F-11. A bare country-code-without-`+` form is the one operators actually
    /// type, and it saves as `62812…` next to the runtime's `+62…`. `plus_form`
    /// normalises it so the saved value matches what the channel compares.
    #[test]
    fn allowlist_entry_normalises_a_bare_country_code() {
        let saved = WhatsAppWebConfig::allowlist_entry("628123456789").expect("must accept");
        assert_eq!(saved, "+628123456789");
    }

    /// The wildcard is a documented exemption, not a phone number.
    #[test]
    fn allowlist_entry_keeps_a_wildcard() {
        assert_eq!(
            WhatsAppWebConfig::allowlist_entry("*").expect("wildcard must pass"),
            "*"
        );
    }

    /// `lid:` is the form the channel writes for an unmapped-LID sender the
    /// operator adds by hand. The helper has to keep it intact or the runtime
    /// cannot match the next inbound from the same LID.
    #[test]
    fn allowlist_entry_keeps_a_lid_marker() {
        assert_eq!(
            WhatsAppWebConfig::allowlist_entry("lid:99887766").expect("lid marker must pass"),
            "lid:99887766"
        );
    }

    /// A `+`-prefixed number passes through with its `+` and its digits.
    #[test]
    fn allowlist_entry_keeps_an_explicit_plus() {
        assert_eq!(
            WhatsAppWebConfig::allowlist_entry("+15551234567").expect("must accept"),
            "+15551234567"
        );
    }

    /// F-11. `plus_form` is what every entry that has only digits goes through.
    #[test]
    fn plus_form_prepends_plus_to_bare_digits() {
        assert_eq!(WhatsAppWebConfig::plus_form("15551234567"), "+15551234567");
    }

    #[test]
    fn plus_form_keeps_an_existing_plus() {
        assert_eq!(WhatsAppWebConfig::plus_form("+15551234567"), "+15551234567");
    }

    #[test]
    fn plus_form_trims_surrounding_whitespace() {
        assert_eq!(
            WhatsAppWebConfig::plus_form("  +15551234567  "),
            "+15551234567"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinqConfig {
    /// Linq Partner API token (Bearer auth)
    pub api_token: String,
    /// Phone number to send from (E.164 format)
    pub from_phone: String,
    /// Webhook signing secret for signature verification
    #[serde(default)]
    pub signing_secret: Option<String>,
    /// Allowed sender handles (phone numbers) or "*" for all
    #[serde(default)]
    pub allowed_senders: Vec<String>,
}

/// Nextcloud Talk bot configuration (webhook receive + OCS send API).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NextcloudTalkConfig {
    /// Nextcloud base URL (e.g. "https://cloud.example.com").
    pub base_url: String,
    /// Bot app token used for OCS API bearer auth.
    pub app_token: String,
    /// Shared secret for webhook signature verification.
    ///
    /// Can also be set via `RANTAICLAW_NEXTCLOUD_TALK_WEBHOOK_SECRET`.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// Allowed Nextcloud actor IDs (`[]` = deny all, `"*"` = allow all).
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl WhatsAppConfig {
    /// Detect which backend to use based on config fields.
    /// Returns "cloud" if phone_number_id is set, "web" if session_path is set.
    /// Is this Cloud API table usable?
    ///
    /// Unchanged by the v32 split: the webhook path has nothing to talk to
    /// without all three.
    pub fn is_cloud_config(&self) -> bool {
        self.phone_number_id.is_some() && self.access_token.is_some() && self.verify_token.is_some()
    }
}

impl ChannelsConfig {
    /// The runtime name of the WhatsApp transport this config runs, if any.
    ///
    /// The same choice `channels::factory::build_configured_channels` makes, so
    /// a caller with no channel runtime can ask it: the Cloud API when its table
    /// is usable (`is_cloud_config`); otherwise WhatsApp Web when its table names
    /// a session and this build has the `whatsapp-web` feature; otherwise none.
    /// Only one WhatsApp ever runs, and a pairing code is accepted only by the
    /// channel whose runtime name matches its surface, so this is also the one
    /// WhatsApp surface a code can be claimed on.
    ///
    /// A test in `channels::factory` holds this to the factory's own answer.
    pub fn running_whatsapp_surface(&self) -> Option<&'static str> {
        if self
            .whatsapp
            .as_ref()
            .is_some_and(WhatsAppConfig::is_cloud_config)
        {
            return Some("whatsapp");
        }
        let web_runs = cfg!(feature = "whatsapp-web")
            && self
                .whatsapp_web
                .as_ref()
                .is_some_and(|web| !web.session_path.trim().is_empty());
        web_runs.then_some("whatsapp_web")
    }
}

/// IRC channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IrcConfig {
    /// IRC server hostname
    pub server: String,
    /// IRC server port (default: 6697 for TLS)
    #[serde(default = "default_irc_port")]
    pub port: u16,
    /// Bot nickname
    pub nickname: String,
    /// Username (defaults to nickname if not set)
    pub username: Option<String>,
    /// Channels to join on connect
    #[serde(default)]
    pub channels: Vec<String>,
    /// Allowed nicknames (case-insensitive) or "*" for all
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Server password (for bouncers like ZNC)
    pub server_password: Option<String>,
    /// NickServ IDENTIFY password
    pub nickserv_password: Option<String>,
    /// SASL PLAIN password (IRCv3)
    pub sasl_password: Option<String>,
    /// Verify TLS certificate (default: true)
    pub verify_tls: Option<bool>,
    /// Allow `verify_tls = false` together with a configured password.
    ///
    /// Off by default, and the channel refuses to start on that combination:
    /// SASL PLAIN is reversible base64 and NickServ IDENTIFY is plaintext, so
    /// a link with no peer authentication hands the credential to whoever
    /// answered the connection. Set this only if that is understood and
    /// intended (a lab bouncer with a self-signed certificate, say).
    #[serde(default)]
    pub allow_insecure_tls_with_password: bool,
}

fn default_irc_port() -> u16 {
    6697
}

/// How RantaiClaw receives events from Feishu / Lark.
///
/// - `websocket` (default) — persistent WSS long-connection; no public URL required.
/// - `webhook`             — HTTP callback server; requires a public HTTPS endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LarkReceiveMode {
    #[default]
    Websocket,
    Webhook,
}

/// Lark/Feishu configuration for messaging integration.
/// Lark is the international version; Feishu is the Chinese version.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LarkConfig {
    /// App ID from Lark/Feishu developer console
    pub app_id: String,
    /// App Secret from Lark/Feishu developer console
    pub app_secret: String,
    /// Encrypt key for webhook message decryption (optional)
    #[serde(default)]
    pub encrypt_key: Option<String>,
    /// Verification token for webhook validation (optional)
    #[serde(default)]
    pub verification_token: Option<String>,
    /// Allowed user IDs or union IDs (empty = deny all, "*" = allow all)
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Whether to use the Feishu (Chinese) endpoint instead of Lark (International)
    #[serde(default)]
    pub use_feishu: bool,
    /// Event receive mode: "websocket" (default) or "webhook"
    #[serde(default)]
    pub receive_mode: LarkReceiveMode,
    /// HTTP port for webhook mode only. Must be set when receive_mode = "webhook".
    /// Not required (and ignored) for websocket mode.
    #[serde(default)]
    pub port: Option<u16>,
}

// ── Audit logging ─────────────────────────────────────────────────

/// Audit logging configuration.
///
/// The audit-trail struct itself is real and in use (wired in #723): the
/// gateway's config-API change logger and the runtime `AuditLogger` both
/// build one with `AuditConfig::default()`. The container that used to hold
/// this alongside the now-deleted `sandbox` and `resources` sections was
/// removed; the `[security.*]` block stays an unknown top-level config key so
/// `warn_on_unknown_top_level_config_keys` keeps reporting it at startup. That
/// is the point: the audit trail runs on defaults today, and making the
/// section parse would replace the unknown-key warning with silence. The
/// `security_is_not_a_known_top_level_key` test exists to pin that.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuditConfig {
    /// Enable audit logging
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,

    /// Path to audit log file (relative to rantaiclaw dir)
    #[serde(default = "default_audit_log_path")]
    pub log_path: String,

    /// Maximum log size in MB before rotation
    #[serde(default = "default_audit_max_size_mb")]
    pub max_size_mb: u32,

    /// Sign events with HMAC for tamper evidence
    #[serde(default)]
    pub sign_events: bool,
}

fn default_audit_enabled() -> bool {
    true
}

fn default_audit_log_path() -> String {
    "audit.log".to_string()
}

fn default_audit_max_size_mb() -> u32 {
    100
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            log_path: default_audit_log_path(),
            max_size_mb: default_audit_max_size_mb(),
            sign_events: false,
        }
    }
}

/// DingTalk configuration for Stream Mode messaging
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DingTalkConfig {
    /// Client ID (AppKey) from DingTalk developer console
    pub client_id: String,
    /// Client Secret (AppSecret) from DingTalk developer console
    pub client_secret: String,
    /// Allowed user IDs (staff IDs). Empty = deny all, "*" = allow all
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

/// QQ Official Bot configuration (Tencent QQ Bot SDK)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QQConfig {
    /// App ID from QQ Bot developer console
    pub app_id: String,
    /// App Secret from QQ Bot developer console
    pub app_secret: String,
    /// Allowed user IDs. Empty = deny all, "*" = allow all
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

// ── Config impl ──────────────────────────────────────────────────

/// Default for the `schema_version` field on `Config`. Returned when
/// a config.toml has no such field (pre-v0.6.45 binaries didn't write
/// it). The migrator stamps the actual version on read.
pub(crate) fn default_config_schema_version() -> u32 {
    crate::config::migrations::CURRENT_VERSION
}

/// Copy back, in `target`, every value that differs between `before` and
/// `after` — but only where `target` still holds the `after` value.
///
/// Walks nested objects so a change deep in the tree (`gateway.host`) is
/// restored as precisely as a top-level one.
fn restore_env_overridden(
    before: &serde_json::Value,
    after: &serde_json::Value,
    target: &mut serde_json::Value,
) {
    let (Some(before), Some(after), Some(target)) = (
        before.as_object(),
        after.as_object(),
        target.as_object_mut(),
    ) else {
        return;
    };

    for (key, after_value) in after {
        let Some(before_value) = before.get(key) else {
            continue;
        };
        if before_value == after_value {
            continue;
        }
        let Some(target_value) = target.get_mut(key) else {
            continue;
        };

        if before_value.is_object() && after_value.is_object() {
            restore_env_overridden(before_value, after_value, target_value);
        } else if target_value == after_value {
            *target_value = before_value.clone();
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let home =
            UserDirs::new().map_or_else(|| PathBuf::from("."), |u| u.home_dir().to_path_buf());
        let rantaiclaw_dir = crate::profile::paths::root_for_home(&home);

        Self {
            env_overrides: None,
            ui: UiConfig::default(),
            schema_version: default_config_schema_version(),
            workspace_dir: rantaiclaw_dir.join("workspace"),
            config_path: rantaiclaw_dir.join("config.toml"),
            api_key: None,
            provider_api_keys: HashMap::new(),
            api_url: None,
            default_provider: Some("openrouter".to_string()),
            // Empty by default: a fresh install has NO model until the operator
            // runs setup. The agent refuses to guess one (see
            // Agent::from_config_with_observer), and the TUI/console show the
            // model field blank rather than a baked-in placeholder.
            default_model: None,
            default_temperature: 0.7,
            observability: ObservabilityConfig::default(),
            autonomy: AutonomyConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            agent: AgentConfig::default(),
            skills: SkillsConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            heartbeat: HeartbeatConfig::default(),
            cron: CronConfig::default(),
            tasks: TasksConfig::default(),
            channels_config: ChannelsConfig::default(),
            memory: MemoryConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            composio: ComposioConfig::default(),
            knowledge: KnowledgeConfig::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_search: WebSearchConfig::default(),
            services: ServicesConfig::default(),
            proxy: ProxyConfig::default(),
            identity: IdentityConfig::default(),
            cost: CostConfig::default(),
            agents: HashMap::new(),
            gateway_agents: HashMap::new(),
            query_classification: QueryClassificationConfig::default(),
            mcp_servers: HashMap::new(),
        }
    }
}

fn default_config_and_workspace_dirs() -> Result<(PathBuf, PathBuf)> {
    let config_dir = default_config_dir()?;
    Ok((config_dir.clone(), config_dir.join("workspace")))
}

/// Profile-aware default that the runtime resolver falls back to when no
/// `RANTAICLAW_CONFIG_DIR`, `RANTAICLAW_WORKSPACE`, or `active_workspace.toml`
/// override is present. The active_workspace marker still lives at the
/// flat root so existing per-workspace overrides continue to work; only the
/// final fallback now points at `~/.rantaiclaw/profiles/<active>/`.
fn profile_default_config_and_workspace_dirs() -> Option<(PathBuf, PathBuf)> {
    let profile = crate::profile::ProfileManager::active().ok()?;
    let workspace = profile.workspace_dir();
    Some((profile.root, workspace))
}

const ACTIVE_WORKSPACE_STATE_FILE: &str = "active_workspace.toml";

#[derive(Debug, Serialize, Deserialize)]
struct ActiveWorkspaceState {
    config_dir: String,
}

fn default_config_dir() -> Result<PathBuf> {
    let home = UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .context("Could not find home directory")?;
    Ok(crate::profile::paths::root_for_home(&home))
}

fn active_workspace_state_path(default_dir: &Path) -> PathBuf {
    default_dir.join(ACTIVE_WORKSPACE_STATE_FILE)
}

/// Whether an active-workspace marker's `config_dir` looks like a leaked
/// ephemeral workspace: it points under the OS temp dir while the real default
/// config dir does not. A genuine install never keeps its workspace under the
/// temp dir, but a non-hermetic test can leave a marker in the real
/// `~/.rantaiclaw` pointing at a since-deleted tempdir — honoring it silently
/// shadows the real config (a split-brain where owner/config edits "don't
/// apply" until the marker is removed). Pure for testing; production passes
/// `std::env::temp_dir()`. Test setups (default dir also under temp) are exempt.
fn active_workspace_marker_is_temp_leak(
    config_dir: &Path,
    default_config_dir: &Path,
    temp_dir: &Path,
) -> bool {
    config_dir.starts_with(temp_dir) && !default_config_dir.starts_with(temp_dir)
}

/// Whether an active-workspace marker names the `profiles/<name>` directory
/// of a *different* config root, while the root that holds the marker has
/// its own `config.toml` for that profile. That is the shape of a marker
/// that was faithfully copied or moved along with the tree (a backup
/// archive, a home directory that moved to another machine, a `rollback`
/// snapshot restored under a different path). Honoring it would have the
/// run follow the marker back to the original location and migrate a
/// config the operator never asked to touch; setting it aside lets the
/// local profile win.
///
/// Returns `Some(local_profile_dir)` when the guard fires, where
/// `local_profile_dir` is `default_config_dir/profiles/<name>`. The caller
/// logs a single warning per process and falls back to the
/// default-resolution branch, which lands on the active profile under the
/// local root.
///
/// Takes only paths and consults the filesystem once, for the profile
/// `config.toml` existence check.
///
/// Three cases must not fire (they keep today's behavior):
/// - marker naming a directory inside its own root (case (a));
/// - marker naming a custom directory that is not shaped like
///   `<some root>/profiles/<name>` (case (b));
/// - marker naming another root's profile when the local root has no
///   `config.toml` for that profile (case (c)).
fn active_workspace_marker_names_another_roots_profile(
    default_config_dir: &Path,
    marker_config_dir: &Path,
) -> Option<PathBuf> {
    let profile_name = marker_config_dir.file_name()?;
    let profiles_dir = marker_config_dir.parent()?.file_name()?;
    if profiles_dir != "profiles" {
        return None;
    }
    // Case (a): the marker points inside the default config root (a relative
    // marker dir resolved earlier, or an absolute path under the default
    // config dir). Honor it as today.
    if marker_config_dir.starts_with(default_config_dir) {
        return None;
    }
    let local_profile = default_config_dir.join("profiles").join(profile_name);
    // Case (c): the local root has no config of its own for that profile,
    // so the marker is the only way to reach it. Honor it.
    if !local_profile.join("config.toml").exists() {
        return None;
    }
    Some(local_profile)
}

/// Warn-once latch for the cross-root-marker guard: the daemon reloads its
/// config on a 15-second tick, so the warning must not repeat every reload.
static CROSS_ROOT_MARKER_WARN: OnceLock<()> = OnceLock::new();

async fn load_persisted_workspace_dirs(
    default_config_dir: &Path,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let state_path = active_workspace_state_path(default_config_dir);
    if !state_path.exists() {
        return Ok(None);
    }

    let contents = match fs::read_to_string(&state_path).await {
        Ok(contents) => contents,
        Err(error) => {
            tracing::warn!(
                "Failed to read active workspace marker {}: {error}",
                state_path.display()
            );
            return Ok(None);
        }
    };

    let state: ActiveWorkspaceState = match toml::from_str(&contents) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                "Failed to parse active workspace marker {}: {error}",
                state_path.display()
            );
            return Ok(None);
        }
    };

    let raw_config_dir = state.config_dir.trim();
    if raw_config_dir.is_empty() {
        tracing::warn!(
            "Ignoring active workspace marker {} because config_dir is empty",
            state_path.display()
        );
        return Ok(None);
    }

    let parsed_dir = PathBuf::from(raw_config_dir);
    let config_dir = if parsed_dir.is_absolute() {
        parsed_dir
    } else {
        default_config_dir.join(parsed_dir)
    };

    // The marker was faithfully copied or moved along with the tree and now
    // names another root's profile; the local root has its own config for
    // that profile. Setting the marker aside lets the local profile win.
    // Logged once per process because the daemon reloads its config on a
    // 15-second tick and would otherwise warn on every reload.
    if let Some(local_profile) =
        active_workspace_marker_names_another_roots_profile(default_config_dir, &config_dir)
    {
        if CROSS_ROOT_MARKER_WARN.set(()).is_ok() {
            tracing::warn!(
                "Ignoring active workspace marker {} because {} names another root's profile; \
                 using {} under the local root {} instead. The marker was copied or moved along \
                 with the tree; set RANTAICLAW_CONFIG_DIR to point at a specific directory to \
                 keep the original location in use.",
                state_path.display(),
                config_dir.display(),
                local_profile.display(),
                default_config_dir.display()
            );
        }
        return Ok(None);
    }

    if active_workspace_marker_is_temp_leak(&config_dir, default_config_dir, &std::env::temp_dir())
    {
        tracing::warn!(
            "Ignoring active workspace marker {} because {} is under the OS temp dir — this \
             usually means a leaked ephemeral/test workspace. Falling back to the default profile.",
            state_path.display(),
            config_dir.display()
        );
        return Ok(None);
    }

    Ok(Some((config_dir.clone(), config_dir.join("workspace"))))
}

pub(crate) async fn persist_active_workspace_config_dir(config_dir: &Path) -> Result<()> {
    let default_config_dir = default_config_dir()?;
    let state_path = active_workspace_state_path(&default_config_dir);

    if config_dir == default_config_dir {
        if state_path.exists() {
            fs::remove_file(&state_path).await.with_context(|| {
                format!(
                    "Failed to clear active workspace marker: {}",
                    state_path.display()
                )
            })?;
        }
        return Ok(());
    }

    fs::create_dir_all(&default_config_dir)
        .await
        .with_context(|| {
            format!(
                "Failed to create default config directory: {}",
                default_config_dir.display()
            )
        })?;

    let state = ActiveWorkspaceState {
        config_dir: config_dir.to_string_lossy().into_owned(),
    };
    let serialized =
        toml::to_string_pretty(&state).context("Failed to serialize active workspace marker")?;

    let temp_path = default_config_dir.join(format!(
        ".{ACTIVE_WORKSPACE_STATE_FILE}.tmp-{}",
        uuid::Uuid::new_v4()
    ));
    fs::write(&temp_path, serialized).await.with_context(|| {
        format!(
            "Failed to write temporary active workspace marker: {}",
            temp_path.display()
        )
    })?;

    if let Err(error) = fs::rename(&temp_path, &state_path).await {
        let _ = fs::remove_file(&temp_path).await;
        anyhow::bail!(
            "Failed to atomically persist active workspace marker {}: {error}",
            state_path.display()
        );
    }

    sync_directory(&default_config_dir).await?;
    Ok(())
}

fn resolve_config_dir_for_workspace(workspace_dir: &Path) -> (PathBuf, PathBuf) {
    let workspace_config_dir = workspace_dir.to_path_buf();
    if workspace_config_dir.join("config.toml").exists() {
        return (
            workspace_config_dir.clone(),
            workspace_config_dir.join("workspace"),
        );
    }

    let legacy_config_dir = workspace_dir
        .parent()
        .map(|parent| parent.join(".rantaiclaw"));
    if let Some(legacy_dir) = legacy_config_dir {
        if legacy_dir.join("config.toml").exists() {
            return (legacy_dir, workspace_config_dir);
        }

        if workspace_dir
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new("workspace"))
        {
            return (legacy_dir, workspace_config_dir);
        }
    }

    (
        workspace_config_dir.clone(),
        workspace_config_dir.join("workspace"),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigResolutionSource {
    EnvConfigDir,
    EnvWorkspace,
    ActiveWorkspaceMarker,
    DefaultConfigDir,
}

impl ConfigResolutionSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EnvConfigDir => "RANTAICLAW_CONFIG_DIR",
            Self::EnvWorkspace => "RANTAICLAW_WORKSPACE",
            Self::ActiveWorkspaceMarker => "active_workspace.toml",
            Self::DefaultConfigDir => "default",
        }
    }
}

async fn resolve_runtime_config_dirs(
    default_rantaiclaw_dir: &Path,
    default_workspace_dir: &Path,
) -> Result<(PathBuf, PathBuf, ConfigResolutionSource)> {
    if let Ok(custom_config_dir) = std::env::var("RANTAICLAW_CONFIG_DIR") {
        let custom_config_dir = custom_config_dir.trim();
        if !custom_config_dir.is_empty() {
            let rantaiclaw_dir = PathBuf::from(custom_config_dir);
            return Ok((
                rantaiclaw_dir.clone(),
                rantaiclaw_dir.join("workspace"),
                ConfigResolutionSource::EnvConfigDir,
            ));
        }
    }

    if let Ok(custom_workspace) = std::env::var("RANTAICLAW_WORKSPACE") {
        if !custom_workspace.is_empty() {
            let (rantaiclaw_dir, workspace_dir) =
                resolve_config_dir_for_workspace(&PathBuf::from(custom_workspace));
            return Ok((
                rantaiclaw_dir,
                workspace_dir,
                ConfigResolutionSource::EnvWorkspace,
            ));
        }
    }

    // Debug-build guard: refuse to fall back to a path derived from the
    // developer's real `$HOME` (or XDG) so a missing override fails loudly
    // instead of silently migrating the operator's config. A unit test must
    // always pin an override. Any other debug build (`cargo run`, an
    // integration test, a spawned binary) is refused only when the default dir
    // is outside the temp dir. That cannot happen today: every caller passes
    // `default_config_dir()`, which the root redirect already moved under the
    // temp dir. The second half of the condition is a backstop in case a
    // future caller bypasses the redirect.
    // Compiled out of release builds; behaviour above this point is unchanged.
    #[cfg(any(test, debug_assertions))]
    {
        if !crate::profile::dev_guard::allow_real_config_dir()
            && (cfg!(test) || !crate::profile::dev_guard::is_under_temp_dir(default_rantaiclaw_dir))
        {
            anyhow::bail!(
                "test config isolation: neither RANTAICLAW_CONFIG_DIR nor RANTAICLAW_WORKSPACE \
                 is set, so this test would read and write the developer's real \
                 ~/.rantaiclaw. Set RANTAICLAW_CONFIG_DIR to a tempdir (see \
                 crate::test_env::EnvGuard) or, if this test really must exercise \
                 default resolution, set RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1. \
                 This guard also fires for `cargo run` and spawned debug binaries."
            );
        }
    }

    if let Some((rantaiclaw_dir, workspace_dir)) =
        load_persisted_workspace_dirs(default_rantaiclaw_dir).await?
    {
        return Ok((
            rantaiclaw_dir,
            workspace_dir,
            ConfigResolutionSource::ActiveWorkspaceMarker,
        ));
    }

    // v0.5.0+: if no explicit override and no per-workspace marker,
    // resolve to the active profile's directory. Falls back to the flat
    // layout if profile resolution fails (defensive — should be unreachable
    // because ProfileManager::active() auto-creates `default`).
    if let Some((profile_root, profile_workspace)) = profile_default_config_and_workspace_dirs() {
        return Ok((
            profile_root,
            profile_workspace,
            ConfigResolutionSource::DefaultConfigDir,
        ));
    }

    Ok((
        default_rantaiclaw_dir.to_path_buf(),
        default_workspace_dir.to_path_buf(),
        ConfigResolutionSource::DefaultConfigDir,
    ))
}

/// Debug-build guard for [`Config::save`]: refuse to write anywhere other than
/// under `std::env::temp_dir()`, so a stray `config_path` pointing at the
/// developer's real `$HOME/.rantaiclaw` cannot write the operator's real
/// config. Mirrors the guard in `resolve_runtime_config_dirs`, including the
/// same opt-out.
#[cfg(any(test, debug_assertions))]
fn config_path_is_test_safe(config_path: &Path) -> bool {
    crate::profile::dev_guard::allow_real_config_dir()
        || crate::profile::dev_guard::is_under_temp_dir(config_path)
}

/// Encrypt every plaintext credential in a raw (not yet deserialised) config,
/// in place. Returns how many values it changed.
///
/// This is the raw-`toml::Value` counterpart to [`decrypt_config_secrets`] and
/// covers the same field list. It exists for the two moments that hold a config
/// as text rather than as a `Config`:
///
/// - the v27 → v28 upgrade, which runs before deserialisation, and
/// - the profile importer, which copies a foreign config it must not reshape.
///
/// **Keep the list below in step with `decrypt_config_secrets`.**
/// `raw_and_typed_secret_lists_cover_the_same_fields` fails if they diverge:
/// it encrypts through this function and decrypts through that one.
///
/// Best-effort per value: a failure is logged and leaves that value plaintext
/// rather than aborting the load or the import.
pub(crate) fn encrypt_config_secrets_in_raw(raw: &mut toml::Value, rantaiclaw_dir: &Path) -> usize {
    // Honour the operator's own `[secrets] encrypt` setting; `SecretStore`
    // returns the plaintext unchanged when encryption is disabled.
    let encrypt_enabled = raw
        .get("secrets")
        .and_then(|s| s.get("encrypt"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    if !encrypt_enabled {
        return 0;
    }
    let store = crate::security::SecretStore::new(rantaiclaw_dir, true);
    let mut changed = 0usize;

    for path in [
        &["api_key"][..],
        &["knowledge", "embedding_api_key"],
        &["knowledge", "vision_api_key"],
        &["composio", "api_key"],
        &["browser", "computer_use", "api_key"],
        &["web_search", "brave_api_key"],
        &["channels_config", "telegram", "bot_token"],
    ] {
        changed += encrypt_raw_at_path(raw, path, &store);
    }

    // Maps whose every value is a credential.
    changed += encrypt_raw_map_values(raw, &["provider_api_keys"], &store);
    // Maps of tables with one credential field each.
    for (table, field) in [("agents", "api_key"), ("gateway_agents", "api_key")] {
        if let Some(entries) = raw.get_mut(table).and_then(toml::Value::as_table_mut) {
            for (_, entry) in entries.iter_mut() {
                changed += encrypt_raw_at_path(entry, &[field], &store);
            }
        }
    }

    // MCP server env: every value, not a name-shaped guess. A heuristic would
    // miss `DATABASE_URL` and `PGPASSWORD` — the same gap the config API's
    // redaction had to work around.
    if let Some(servers) = raw
        .get_mut("mcp_servers")
        .and_then(toml::Value::as_table_mut)
    {
        for (_, server) in servers.iter_mut() {
            changed += encrypt_raw_map_values(server, &["env"], &store);
        }
    }

    // Skill keys, but only `source = "literal"` ones — a reference to a key
    // held elsewhere is not itself a secret.
    if let Some(entries) = raw
        .get_mut("skills")
        .and_then(|s| s.get_mut("entries"))
        .and_then(toml::Value::as_table_mut)
    {
        for (_, entry) in entries.iter_mut() {
            let is_literal = entry
                .get("api_key")
                .and_then(|k| k.get("source"))
                .and_then(toml::Value::as_str)
                == Some("literal");
            if is_literal {
                changed += encrypt_raw_at_path(entry, &["api_key", "value"], &store);
            }
        }
    }

    changed
}

/// Encrypt the string at `path` if it is present and not already ciphertext.
fn encrypt_raw_at_path(
    raw: &mut toml::Value,
    path: &[&str],
    store: &crate::security::SecretStore,
) -> usize {
    let mut cursor = raw;
    let (last, parents) = match path.split_last() {
        Some(split) => split,
        None => return 0,
    };
    for key in parents {
        match cursor.get_mut(*key) {
            Some(next) => cursor = next,
            None => return 0,
        }
    }
    let Some(slot) = cursor.get_mut(*last) else {
        return 0;
    };
    encrypt_raw_slot(slot, store)
}

/// Encrypt every string value of the table at `path`.
fn encrypt_raw_map_values(
    raw: &mut toml::Value,
    path: &[&str],
    store: &crate::security::SecretStore,
) -> usize {
    let mut cursor = raw;
    for key in path {
        match cursor.get_mut(*key) {
            Some(next) => cursor = next,
            None => return 0,
        }
    }
    let Some(table) = cursor.as_table_mut() else {
        return 0;
    };
    let mut changed = 0;
    for (_, value) in table.iter_mut() {
        changed += encrypt_raw_slot(value, store);
    }
    changed
}

fn encrypt_raw_slot(slot: &mut toml::Value, store: &crate::security::SecretStore) -> usize {
    let Some(plain) = slot.as_str() else {
        return 0;
    };
    if plain.is_empty() || crate::security::SecretStore::is_encrypted(plain) {
        return 0;
    }
    match store.encrypt(plain) {
        Ok(ciphertext) => {
            *slot = toml::Value::String(ciphertext);
            1
        }
        Err(e) => {
            tracing::warn!("could not encrypt a config credential at rest: {e:#}");
            0
        }
    }
}

pub(crate) fn decrypt_optional_secret(
    store: &crate::security::SecretStore,
    value: &mut Option<String>,
    field_name: &str,
) -> Result<()> {
    if let Some(raw) = value.clone() {
        if crate::security::SecretStore::is_encrypted(&raw) {
            *value = Some(
                store
                    .decrypt(&raw)
                    .with_context(|| format!("Failed to decrypt {field_name}"))?,
            );
        }
    }
    Ok(())
}

/// Decrypt every at-rest-encrypted secret in `config`, in place.
///
/// The single authority on WHICH fields are encrypted. `Config::load_or_init`
/// and the TUI's `reload_config` both call this; before it existed each kept
/// a hand-copy of the list, and the copies drifted twice (KB keys, then
/// `provider_api_keys` — the latter answered 401 on every provider call
/// after a config-watcher reload until #565). Add new encrypted fields HERE
/// and in `save()`'s encrypt side, nowhere else.
pub(crate) fn decrypt_config_secrets(
    store: &crate::security::SecretStore,
    config: &mut Config,
) -> Result<()> {
    decrypt_optional_secret(store, &mut config.api_key, "config.api_key")?;
    decrypt_optional_secret(
        store,
        &mut config.knowledge.embedding_api_key,
        "config.knowledge.embedding_api_key",
    )?;
    decrypt_optional_secret(
        store,
        &mut config.knowledge.vision_api_key,
        "config.knowledge.vision_api_key",
    )?;
    decrypt_optional_secret(
        store,
        &mut config.composio.api_key,
        "config.composio.api_key",
    )?;

    decrypt_optional_secret(
        store,
        &mut config.browser.computer_use.api_key,
        "config.browser.computer_use.api_key",
    )?;

    decrypt_optional_secret(
        store,
        &mut config.web_search.brave_api_key,
        "config.web_search.brave_api_key",
    )?;

    for agent in config.agents.values_mut() {
        decrypt_optional_secret(store, &mut agent.api_key, "config.agents.*.api_key")?;
    }
    for agent in config.gateway_agents.values_mut() {
        decrypt_optional_secret(store, &mut agent.api_key, "config.gateway_agents.*.api_key")?;
    }
    for key in config.provider_api_keys.values_mut() {
        let mut wrapped = Some(std::mem::take(key));
        decrypt_optional_secret(store, &mut wrapped, "config.provider_api_keys.*")?;
        *key = wrapped.unwrap_or_default();
    }
    // Decrypt the Telegram bot token symmetrically with `save()` so the
    // running channel receives the plaintext token.
    if let Some(tg) = config.channels_config.telegram.as_mut() {
        let mut wrapped = Some(std::mem::take(&mut tg.bot_token));
        decrypt_optional_secret(
            store,
            &mut wrapped,
            "config.channels_config.telegram.bot_token",
        )?;
        tg.bot_token = wrapped.unwrap_or_default();
    }
    // An MCP server's `env` holds the token it authenticates with. Every
    // value is treated as a secret rather than guessing from the variable
    // name: a name-shaped heuristic would miss `DATABASE_URL` and
    // `PGPASSWORD`, which is the same gap the config API's redaction had to
    // work around. Decrypting symmetrically with `save()` is what lets
    // `McpClient::connect` still receive the plaintext it spawns with.
    for (name, server) in &mut config.mcp_servers {
        for (var, value) in &mut server.env {
            let mut wrapped = Some(std::mem::take(value));
            decrypt_optional_secret(
                store,
                &mut wrapped,
                &format!("config.mcp_servers.{name}.env.{var}"),
            )?;
            *value = wrapped.unwrap_or_default();
        }
    }
    // Decrypt skill literal API keys symmetrically with `save()` so
    // `src/tools/mod.rs` still reads a plaintext value in memory.
    for (name, entry) in &mut config.skills.entries {
        if let Some(api_key) = entry.api_key.as_mut() {
            if api_key.source == "literal" {
                decrypt_optional_secret(
                    store,
                    &mut api_key.value,
                    &format!("config.skills.entries.{name}.api_key.value"),
                )?;
            }
        }
    }
    Ok(())
}

fn encrypt_optional_secret(
    store: &crate::security::SecretStore,
    value: &mut Option<String>,
    field_name: &str,
) -> Result<()> {
    if let Some(raw) = value.clone() {
        if !crate::security::SecretStore::is_encrypted(&raw) {
            *value = Some(
                store
                    .encrypt(&raw)
                    .with_context(|| format!("Failed to encrypt {field_name}"))?,
            );
        }
    }
    Ok(())
}

/// Top-level config keys the schema recognizes, derived from the generated
/// JSON schema so it can never drift from the struct. Computed paths that are
/// skipped during (de)serialization (`config_path`, `workspace_dir`) are
/// already absent from the schema's `properties`, so they are not treated as
/// writable keys.
fn known_top_level_config_keys() -> std::collections::HashSet<String> {
    let schema = schemars::schema_for!(Config);
    serde_json::to_value(&schema)
        .ok()
        .as_ref()
        .and_then(|json| json.get("properties"))
        .and_then(serde_json::Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

/// Case-insensitive Levenshtein distance, used only to suggest a near miss for
/// an unrecognized key. Bounded by the two key lengths and runs at most once
/// per unknown key, so cost is negligible.
fn key_edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_ascii_lowercase().chars().collect();
    let b: Vec<char> = b.to_ascii_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Nearest known key within a small edit distance, if any — a typo like
/// `defualt_provider` resolves to `default_provider`; an entirely foreign key
/// gets no misleading suggestion.
fn nearest_known_key<'a>(
    unknown: &str,
    known: &'a std::collections::HashSet<String>,
) -> Option<&'a str> {
    let threshold = (unknown.len() / 3).max(2);
    known
        .iter()
        .map(|k| (key_edit_distance(unknown, k), k))
        .filter(|(dist, _)| *dist <= threshold)
        .min_by_key(|(dist, _)| *dist)
        .map(|(_, k)| k.as_str())
}

/// Warn (never fail) on top-level config keys the schema does not recognize.
/// serde ignores unknown fields, so a mistyped section or key silently loads
/// as its default; this restores a signal without a hard `deny_unknown_fields`
/// that would reject forward-compat keys outright.
fn warn_on_unknown_top_level_config_keys(raw: &toml::Value, config_path: &Path) {
    let Some(table) = raw.as_table() else {
        return;
    };
    let known = known_top_level_config_keys();
    if known.is_empty() {
        return;
    }
    for key in table.keys() {
        if known.contains(key.as_str()) {
            continue;
        }
        let suggestion = nearest_known_key(key, &known)
            .map(|hint| format!(" (did you mean `{hint}`?)"))
            .unwrap_or_default();
        tracing::warn!(
            "unknown config key `{key}` in {} was ignored{suggestion}",
            config_path.display()
        );
    }
}

fn config_dir_creation_error(path: &Path) -> String {
    format!(
        "Failed to create config directory: {}. If running as an OpenRC service, \
         ensure this path is writable by user 'rantaiclaw'.",
        path.display()
    )
}

fn is_local_ollama_endpoint(api_url: Option<&str>) -> bool {
    let Some(raw) = api_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };

    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_ascii_lowercase()))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "0.0.0.0"))
}

fn has_ollama_cloud_credential(config_api_key: Option<&str>) -> bool {
    let config_key_present = config_api_key
        .map(str::trim)
        .is_some_and(|value| !value.is_empty());
    if config_key_present {
        return true;
    }

    ["OLLAMA_API_KEY", "RANTAICLAW_API_KEY", "API_KEY"]
        .iter()
        .any(|name| {
            std::env::var(name)
                .ok()
                .is_some_and(|value| !value.trim().is_empty())
        })
}

impl Config {
    /// Resolve the API key to use for a specific provider.
    ///
    /// Resolution order:
    /// 1. `provider_api_keys` (keyed by canonical provider name, then the raw
    ///    name) — the per-provider store written by the console.
    /// 2. the top-level `api_key`, but **only** when `provider` is the active
    ///    `default_provider` (the top-level key is that provider's key).
    /// 3. otherwise `None`, so [`crate::providers::resolve_provider_credential`]
    ///    falls back to the provider-specific env var instead of sending the
    ///    wrong provider's key — which is what caused 401s after switching
    ///    providers in the console.
    pub fn resolve_key_for_provider(&self, provider: &str) -> Option<String> {
        let canonical = crate::providers::normalize_provider_name(provider);
        for candidate in [canonical.as_str(), provider] {
            if let Some(k) = self.provider_api_keys.get(candidate) {
                let k = k.trim();
                if !k.is_empty() {
                    return Some(k.to_string());
                }
            }
        }
        let default_canonical = self
            .default_provider
            .as_deref()
            .map(crate::providers::normalize_provider_name);
        if default_canonical.as_deref() == Some(canonical.as_str()) {
            if let Some(k) = self.api_key.as_deref() {
                let k = k.trim();
                if !k.is_empty() {
                    return Some(k.to_string());
                }
            }
        }
        None
    }

    /// Resolve the active `config.toml` path and workspace dir the same way
    /// [`Config::load_or_init`] does — honoring `RANTAICLAW_CONFIG_DIR`,
    /// `RANTAICLAW_WORKSPACE`, the `active_workspace.toml` marker, and the active
    /// profile — WITHOUT reading, parsing, or applying env-value overrides.
    ///
    /// For callers that must load config without env-value overrides (e.g. the
    /// Telegram owner-persist path) yet still target the daemon's real config
    /// file, not a hardcoded legacy location.
    pub async fn resolve_active_paths() -> Result<(PathBuf, PathBuf)> {
        let (default_dir, default_ws) = default_config_and_workspace_dirs()?;
        let (rantaiclaw_dir, workspace_dir, _src) =
            resolve_runtime_config_dirs(&default_dir, &default_ws).await?;
        Ok((rantaiclaw_dir.join("config.toml"), workspace_dir))
    }

    pub async fn load_or_init() -> Result<Self> {
        // v0.5.0 introduces a profile-aware storage layout
        // (~/.rantaiclaw/profiles/<name>/...). On first run after upgrading
        // from v0.4.x, atomically move the flat layout into profiles/default/.
        // No-op on fresh installs and on already-migrated systems.
        // See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md` §7.1.
        if let Err(e) = crate::profile::migration::maybe_migrate_legacy_layout() {
            tracing::warn!("legacy-layout migration failed: {e:#}; continuing with current layout");
        }

        // v0.7.x: sessions.db used to leak to a single global XDG data dir
        // shared by every profile. Move it into profiles/default/ so history
        // is per-profile. No-op on fresh installs and once already moved.
        if let Err(e) = crate::profile::migration::maybe_migrate_global_sessions_db() {
            tracing::warn!("sessions.db migration failed: {e:#}; continuing with current layout");
        }

        // Same story for the knowledge-base db — move the global kb.db into
        // profiles/default/ so each profile owns its own corpus.
        if let Err(e) = crate::profile::migration::maybe_migrate_global_kb_db() {
            tracing::warn!("kb.db migration failed: {e:#}; continuing with current layout");
        }

        let (default_rantaiclaw_dir, default_workspace_dir) = default_config_and_workspace_dirs()?;

        let (rantaiclaw_dir, workspace_dir, resolution_source) =
            resolve_runtime_config_dirs(&default_rantaiclaw_dir, &default_workspace_dir).await?;

        let config_path = rantaiclaw_dir.join("config.toml");

        fs::create_dir_all(&rantaiclaw_dir)
            .await
            .with_context(|| config_dir_creation_error(&rantaiclaw_dir))?;
        fs::create_dir_all(&workspace_dir)
            .await
            .context("Failed to create workspace directory")?;

        if config_path.exists() {
            // Warn if config file is world-readable (may contain API keys)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&config_path).await {
                    if meta.permissions().mode() & 0o004 != 0 {
                        tracing::warn!(
                            "Config file {:?} is world-readable (mode {:o}). \
                             Consider restricting with: chmod 600 {:?}",
                            config_path,
                            meta.permissions().mode() & 0o777,
                            config_path,
                        );
                    }
                }
            }

            let contents = fs::read_to_string(&config_path)
                .await
                .context("Failed to read config file")?;
            // Run versioned config migrations between read and parse.
            // Old configs (pre-v0.6.45) lack the `schema_version`
            // field; the migrator treats them as v0 and stamps the
            // current version. Future renames / re-typings of fields
            // plug in as `migrate_vN` arms in `config::migrations`.
            // If anything changed, persist the migrated form to disk
            // so subsequent loads skip the work.
            let mut raw: toml::Value = toml::from_str(&contents)
                .with_context(|| format!("Failed to parse {} as TOML", config_path.display()))?;
            // Capture the pre-migration memory backend so the v34 markdown
            // import can be triggered after migration stamps the new version.
            // `migrate_v34` rewrites `markdown` → `sqlite`; reading the value
            // before the call is the only point at which we know it was the
            // retired backend.
            let markdown_pre_migration = raw_memory_or_storage_was_markdown(&raw);
            // Same trick for the v35 postgres warning: `migrate_v35` rewrites
            // `postgres` → `sqlite` (and strips `[storage]`), so the value
            // here is the only signal we have to tell the operator that their
            // notes stayed in Postgres and the local sqlite store starts empty.
            let postgres_pre_migration = raw_memory_backend_was_postgres(&raw);
            let migrated = crate::config::migrations::migrate(&mut raw)
                .context("Failed to migrate config schema")?;
            // A credential that ended up in `api_url` sits on disk in plaintext
            // (unlike `api_key`, which is encrypted) and is echoed back to the
            // console's base-URL field. The gateway has rejected such writes
            // since v0.18.0, but that guard is write-only — configs written
            // before it kept the value. Drop it here, between read and parse, so
            // the write-back below also takes it off disk.
            let stripped_credential = crate::config::api_url::strip_credential_api_url(&mut raw);
            if stripped_credential {
                tracing::warn!(
                    "removed an API key that was stored in `api_url` in {}. \
                     That value was held in plaintext and shown in the web console's \
                     base-URL field — rotate the key with your provider, then set it \
                     as the API key rather than the base URL.",
                    config_path.display()
                );
            }
            // Values written before v28 are on disk in plaintext. The migration
            // runner cannot encrypt them — it is a pure `toml::Value` transform
            // with no access to the profile's secret key — so the one-time pass
            // happens here, inside the write-back the bump already triggers.
            // Gated on `migrated` so this is an upgrade step, not a rewrite of
            // the operator's file on every load: a hand-added plaintext value
            // is encrypted by the next `save()`, exactly like `api_key`.
            if migrated {
                if let Some(dir) = config_path.parent() {
                    let encrypted = encrypt_config_secrets_in_raw(&mut raw, dir);
                    if encrypted > 0 {
                        tracing::info!(
                            "encrypted {encrypted} credential(s) at rest in {}",
                            config_path.display()
                        );
                    }
                }
            }
            // The v33 → v34 bump retires the `markdown` memory backend. If
            // the operator's pre-migration config said `markdown` (on either
            // `[memory].backend` or `[storage.provider.config].provider`,
            // which overrides at runtime) and the bump ran on this load,
            // back up the markdown notes now, BEFORE the migrated config is
            // ever written to disk. Only the backup happens here — the
            // actual import (rows + the `MEMORY.md` projection) is left to
            // `retry_unimported_markdown_imports`, called once below after
            // `config` is built, which treats a backup made here exactly
            // like one left over from an earlier failed run. One function
            // does the importing and warns on failure, not two.
            //
            // If the backup fails, `should_persist_migration` below stays
            // false, so the write-back is skipped and the config on disk
            // keeps naming `markdown`. The next load's `migrate()` then sees
            // that unchanged value and retries the whole bump — backup
            // included — from scratch, the same way an ordinary write-back
            // failure already retries a few lines down. That reuses the
            // existing on-disk-state retry path instead of a new config key.
            // The load still proceeds with the migrated config in memory for
            // this run: failing the whole daemon start over a backup hiccup
            // would be worse than one session that re-attempts on restart.
            // A complete backup already sitting under `memory/migrations/`
            // (imported or still pending) means an earlier load already did
            // this step — most often because the write-back below keeps
            // failing, so the on-disk schema never advances off `markdown`
            // and this branch keeps firing on every start. Making another
            // complete backup here would give the sweep another fresh,
            // never-imported directory to import on every such start,
            // re-applying markdown-wins and resurrecting deleted rows.
            //
            // A failed backup also leaves a `PENDING` marker under
            // `memory/migrations/`. The in-memory config is already sqlite,
            // so any save in this session (pairing, `permissions add`, the
            // TUI) writes it to disk, and the next start would no longer see
            // `markdown` and would forget the notes. The marker survives that
            // save: `retry_unimported_markdown_imports` retries the backup and
            // the import while it exists, whatever the config says.
            let mut markdown_backup_failed = false;
            if migrated && markdown_pre_migration {
                match newest_complete_markdown_backup(&workspace_dir) {
                    None => {
                        if let Err(e) = crate::migration::backup_markdown_memory(&workspace_dir) {
                            markdown_backup_failed = true;
                            tracing::warn!(
                                error = %format!("{e:#}"),
                                workspace = %workspace_dir.display(),
                                "failed to back up markdown memory before migrating the config \
                                 to sqlite; the schema upgrade and the import both retry on the \
                                 next start."
                            );
                            if let Err(e) =
                                crate::migration::record_pending_markdown_import(&workspace_dir)
                            {
                                tracing::warn!(
                                    error = %format!("{e:#}"),
                                    workspace = %workspace_dir.display(),
                                    "could not record the pending markdown memory import; if \
                                     the config is saved before the next start, the notes are \
                                     not imported"
                                );
                            }
                        }
                    }
                    Some(existing) => {
                        // An earlier load already backed up (and maybe imported)
                        // the markdown notes, so this load makes no new backup.
                        // Notes written to the markdown files after that backup,
                        // for instance on the release the operator rolled back
                        // to, are in no backup and would be lost silently.
                        if live_markdown_newer_than_backup(&workspace_dir, &existing) {
                            tracing::warn!(
                                backup = %existing.display(),
                                "markdown memory files hold notes written after this backup, \
                                 which an earlier start made; the notes written since were not \
                                 imported"
                            );
                        }
                    }
                }
            }
            // Whether this load's changes get persisted to disk at all. A
            // markdown backup failure holds back the credential strip too,
            // not only the schema bump: `raw` here is already fully
            // migrated (current schema, sqlite) regardless of which
            // condition triggers the write, so writing it while the backup
            // failed would make the on-disk schema advance anyway — closing
            // the only signal (`markdown_pre_migration` on the next read)
            // that makes the gate above retry, and leaving the operator's
            // markdown notes imported never. The credential-removed WARN
            // fires here directly in that case, since the write-back is
            // skipped rather than attempted-and-failed.
            if markdown_backup_failed && stripped_credential {
                tracing::warn!(
                    "the API key removed from `api_url` is still present in {} because the \
                     config was not written back this start (a markdown memory backup failed \
                     first) — remove the `api_url` line by hand, or wait for a start where the \
                     backup succeeds.",
                    config_path.display()
                );
            }
            let should_persist_migration = migrated && !markdown_backup_failed;
            let should_persist_credential_strip = stripped_credential && !markdown_backup_failed;
            if should_persist_migration || should_persist_credential_strip {
                let serialized =
                    toml::to_string_pretty(&raw).context("Failed to serialise migrated config")?;
                // Atomic write with a retained `.bak`: this is the one write that
                // touches every config on every upgrade, and a truncating fs::write
                // that failed mid-way left no recovery point. keep_backup=true makes
                // a bad migrate_vN reversible.
                if let Err(e) = atomic_write_config(&config_path, serialized.as_bytes(), true).await
                {
                    // Non-fatal: we still have the migrated value in
                    // memory. Warn so the user knows the next load
                    // will redo the work.
                    tracing::warn!(
                        "config updated in memory but write-back to {} failed: {e:#}",
                        config_path.display()
                    );
                    if stripped_credential {
                        // The in-memory drop still holds, but the plaintext key
                        // is on disk until a later save succeeds. Say so plainly
                        // — the operator may be relying on the warning above.
                        tracing::warn!(
                            "the API key removed from `api_url` is still present in {} \
                             because the write-back failed — remove the `api_url` line by hand.",
                            config_path.display()
                        );
                    }
                } else if should_persist_migration {
                    tracing::info!(
                        "config schema migrated to v{} ({})",
                        crate::config::migrations::CURRENT_VERSION,
                        config_path.display()
                    );
                }
            }
            // Flag top-level keys the schema does not recognize. A mistyped
            // section or key (`[gatway]`, `defualt_provider`) otherwise
            // deserializes silently to the default and no-ops with no signal.
            // Runs AFTER migration, so keys a past migration legitimately
            // removed are already gone and never warned about.
            warn_on_unknown_top_level_config_keys(&raw, &config_path);
            // The v34 → v35 bump retires the `postgres` memory backend and
            // the entire `[storage]` section. `migrate_v35` rewrites the
            // backend name on disk and strips the storage table (including
            // any encrypted `db_url`); the operator's notes stay in Postgres,
            // the local sqlite store starts empty. Gated on `migrated` so
            // re-running on an already-current config never re-warns. The
            // message intentionally does NOT include the URL or any storage
            // field — the secret is gone from the on-disk surface as soon as
            // migration stamps, and echoing it here would defeat the
            // redaction that `src/gateway/config_api.rs` already performs.
            if migrated && postgres_pre_migration {
                tracing::warn!(
                    "postgres memory was retired; your notes stay in Postgres and \
                     the local sqlite store starts empty"
                );
            }
            let mut config: Config = raw.try_into().with_context(|| {
                format!(
                    "Failed to deserialise (post-migration) config at {}",
                    config_path.display()
                )
            })?;
            // Set computed paths that are skipped during serialization
            config.config_path = config_path.clone();
            config.workspace_dir = workspace_dir;
            let store = crate::security::SecretStore::new(&rantaiclaw_dir, config.secrets.encrypt);
            decrypt_config_secrets(&store, &mut config)?;
            config.apply_env_overrides();
            config.validate()?;
            // Retry any markdown import that has not finished — including a
            // backup made above and any left over from an earlier failed
            // run of this or another process. Runs on every load, not only
            // `migrated` ones: a failure can happen on a run after the
            // config was already written as sqlite, and the marker file —
            // not a config key — is what tells that later run to retry.
            // Skipped when the current backend is not sqlite: importing
            // into a store the config no longer uses would write brain.db
            // and rewrite MEMORY.md for no reason anything reads. The one
            // exception is a `PENDING` marker, which is the operator's
            // notes still waiting for their first import. It is retried
            // whatever backend the config names.
            if config.memory.backend.trim().eq_ignore_ascii_case("sqlite")
                || crate::migration::pending_markdown_import_marker(&config.workspace_dir).exists()
            {
                retry_unimported_markdown_imports(&config.workspace_dir, markdown_backup_failed);
            }
            tracing::info!(
                path = %config.config_path.display(),
                workspace = %config.workspace_dir.display(),
                source = resolution_source.as_str(),
                initialized = false,
                "Config loaded"
            );
            Ok(config)
        } else {
            let mut config = Config::default();
            config.config_path = config_path.clone();
            config.workspace_dir = workspace_dir;
            config.save().await?;

            // Restrict permissions on newly created config file (may contain API keys)
            #[cfg(unix)]
            {
                use std::{fs::Permissions, os::unix::fs::PermissionsExt};
                let _ = fs::set_permissions(&config_path, Permissions::from_mode(0o600)).await;
            }

            config.apply_env_overrides();
            config.validate()?;
            tracing::info!(
                path = %config.config_path.display(),
                workspace = %config.workspace_dir.display(),
                source = resolution_source.as_str(),
                initialized = true,
                "Config loaded"
            );
            Ok(config)
        }
    }

    /// Read a config from a SPECIFIC path with no side effects: read → migrate
    /// (in memory, no write-back) → parse → decrypt every secret → validate.
    ///
    /// Unlike [`Self::load_or_init`] it does NOT re-resolve the path from HOME/env,
    /// run profile migrations, write anything to disk, or apply env overrides
    /// (callers that want env precedence apply it themselves). Use it wherever a
    /// caller must operate on the config it MANAGES — the channel runtime reload
    /// and the pairing-token writer — rather than a re-resolved default profile.
    pub async fn load_from_path(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read config file {}", path.display()))?;
        let mut raw: toml::Value = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse {} as TOML", path.display()))?;
        crate::config::migrations::migrate(&mut raw).context("Failed to migrate config schema")?;
        crate::config::api_url::strip_credential_api_url(&mut raw);
        let mut config: Config = raw
            .try_into()
            .with_context(|| format!("Failed to deserialise config {}", path.display()))?;
        config.config_path = path.to_path_buf();
        if let Some(parent) = path.parent() {
            config.workspace_dir = parent.join("workspace");
            let store = crate::security::SecretStore::new(parent, config.secrets.encrypt);
            decrypt_config_secrets(&store, &mut config)?;
        }
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration values that would cause runtime failures.
    ///
    /// Called after TOML deserialization and env-override application to catch
    /// obviously invalid values early instead of failing at arbitrary runtime points.
    pub fn validate(&self) -> Result<()> {
        // Gateway
        if self.gateway.host.trim().is_empty() {
            anyhow::bail!("gateway.host must not be empty");
        }

        // Console login: the two fields are independently settable, but the
        // gate keys "is login enabled" on `password_hash` alone while the
        // comparison needs a username. A hash without one makes every login
        // attempt fail with "Invalid username or password" — unfalsifiable from
        // the login form — and burns a brute-force lockout slot each try. Fail
        // at load rather than let the operator debug an unwinnable prompt.
        let login = &self.gateway.login;
        let has_hash = login
            .password_hash
            .as_deref()
            .is_some_and(|h| !h.trim().is_empty());
        let has_user = login
            .username
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty());
        if has_hash && !has_user {
            anyhow::bail!(
                "gateway.login.password_hash is set but gateway.login.username is empty — \
                 every login attempt would fail. Run `rantaiclaw setup login` to set both, \
                 or clear password_hash to turn the gate off."
            );
        }
        if has_user && !has_hash {
            anyhow::bail!(
                "gateway.login.username is set but gateway.login.password_hash is empty — \
                 the login gate is off, so the username has no effect. Run \
                 `rantaiclaw setup login` to finish enabling it, or clear username."
            );
        }

        // Autonomy
        if self.autonomy.max_actions_per_hour == 0 {
            anyhow::bail!("autonomy.max_actions_per_hour must be greater than 0");
        }

        // Sampling: an out-of-range temperature is accepted by serde but rejected
        // by every provider, so catch it at load/write instead of at request time.
        if !(0.0..=2.0).contains(&self.default_temperature) {
            anyhow::bail!(
                "default_temperature must be between 0.0 and 2.0 (got {})",
                self.default_temperature
            );
        }

        // Scheduler
        if self.scheduler.max_concurrent == 0 {
            anyhow::bail!("scheduler.max_concurrent must be greater than 0");
        }
        if self.scheduler.max_tasks == 0 {
            anyhow::bail!("scheduler.max_tasks must be greater than 0");
        }

        // Model routes
        for (i, route) in self.model_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].hint must not be empty");
            }
            if route.provider.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].provider must not be empty");
            }
            if route.model.trim().is_empty() {
                anyhow::bail!("model_routes[{i}].model must not be empty");
            }
        }

        // Embedding routes
        for (i, route) in self.embedding_routes.iter().enumerate() {
            if route.hint.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].hint must not be empty");
            }
            if route.provider.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].provider must not be empty");
            }
            if route.model.trim().is_empty() {
                anyhow::bail!("embedding_routes[{i}].model must not be empty");
            }
        }

        // Ollama cloud-routing safety checks
        if self
            .default_provider
            .as_deref()
            .is_some_and(|provider| provider.trim().eq_ignore_ascii_case("ollama"))
            && self
                .default_model
                .as_deref()
                .is_some_and(|model| model.trim().ends_with(":cloud"))
        {
            if is_local_ollama_endpoint(self.api_url.as_deref()) {
                anyhow::bail!(
                    "default_model uses ':cloud' with provider 'ollama', but api_url is local or unset. Set api_url to a remote Ollama endpoint (for example https://ollama.com)."
                );
            }

            if !has_ollama_cloud_credential(self.api_key.as_deref()) {
                anyhow::bail!(
                    "default_model uses ':cloud' with provider 'ollama', but no API key is configured. Set api_key or OLLAMA_API_KEY."
                );
            }
        }

        // Proxy (delegate to existing validation)
        self.proxy.validate()?;

        Ok(())
    }

    /// Apply environment variable overrides to config
    /// Fold environment overrides onto this config, remembering what they
    /// changed so [`Config::save`] can leave those values out of `config.toml`.
    ///
    /// The environment still wins at runtime — only persistence is affected.
    pub fn apply_env_overrides(&mut self) {
        let before = serde_json::to_value(&*self).ok();

        self.apply_env_overrides_inner();

        // A before/after pair rather than a list of field names: every
        // override that exists today is covered, and any override added to
        // `apply_env_overrides_inner` later is covered with no further work.
        if let (Some(before), Ok(after)) = (before, serde_json::to_value(&*self)) {
            if before != after {
                // Keep the earliest `before` — that one is the on-disk truth.
                let before = self.env_overrides.take().map_or(before, |s| s.before);
                self.env_overrides = Some(Box::new(EnvOverrideSnapshot { before, after }));
            }
        }
    }

    fn apply_env_overrides_inner(&mut self) {
        // API Key: RANTAICLAW_API_KEY or API_KEY (generic)
        if let Ok(key) = std::env::var("RANTAICLAW_API_KEY").or_else(|_| std::env::var("API_KEY")) {
            if !key.is_empty() {
                self.api_key = Some(key);
            }
        }
        // KB keys: env folds onto config.knowledge at load (env wins),
        // matching api_key precedence. Downstream reads config.knowledge only.
        if let Ok(k) = std::env::var("KB_EMBEDDING_API_KEY") {
            if !k.trim().is_empty() {
                self.knowledge.embedding_api_key = Some(k);
                // Supplying the embedding key via env is an explicit opt-in to
                // the KB — turn it on (env wins, matching the precedence above).
                // This replaces the env evidence that used to live impurely in
                // migrate_v18, so the KB no longer stays off with the key present.
                self.knowledge.enabled = true;
            }
        }
        if let Ok(k) = std::env::var("KB_EXTRACT_VISION_API_KEY") {
            if !k.is_empty() {
                self.knowledge.vision_api_key = Some(k);
            }
        }
        // API Key: GLM_API_KEY overrides when provider is a GLM/Zhipu variant.
        if self.default_provider.as_deref().is_some_and(is_glm_alias) {
            if let Ok(key) = std::env::var("GLM_API_KEY") {
                if !key.is_empty() {
                    self.api_key = Some(key);
                }
            }
        }

        // API Key: ZAI_API_KEY overrides when provider is a Z.AI variant.
        if self.default_provider.as_deref().is_some_and(is_zai_alias) {
            if let Ok(key) = std::env::var("ZAI_API_KEY") {
                if !key.is_empty() {
                    self.api_key = Some(key);
                }
            }
        }

        // Provider override precedence:
        // 1) RANTAICLAW_PROVIDER always wins when set.
        // 2) Legacy PROVIDER is only honored when config still uses the
        //    default provider (openrouter) or provider is unset. This prevents
        //    container defaults from overriding explicit custom providers.
        if let Ok(provider) = std::env::var("RANTAICLAW_PROVIDER") {
            if !provider.is_empty() {
                self.default_provider = Some(provider);
            }
        } else if let Ok(provider) = std::env::var("PROVIDER") {
            let should_apply_legacy_provider =
                self.default_provider.as_deref().map_or(true, |configured| {
                    configured.trim().eq_ignore_ascii_case("openrouter")
                });
            if should_apply_legacy_provider && !provider.is_empty() {
                self.default_provider = Some(provider);
            }
        }

        // Model: RANTAICLAW_MODEL or MODEL
        if let Ok(model) = std::env::var("RANTAICLAW_MODEL").or_else(|_| std::env::var("MODEL")) {
            if !model.is_empty() {
                self.default_model = Some(model);
            }
        }

        // Workspace directory: RANTAICLAW_WORKSPACE.
        //
        // Honor CONFIG_DIR precedence: when RANTAICLAW_CONFIG_DIR is set,
        // `resolve_runtime_config_dirs` already derived `workspace_dir` as
        // `<config_dir>/workspace` and ignored RANTAICLAW_WORKSPACE. Re-applying
        // it here produced a split brain — `config_path` under CONFIG_DIR but
        // `workspace_dir` under WORKSPACE — so skills/memory/policy resolved
        // against a different tree than the config.
        if std::env::var_os("RANTAICLAW_CONFIG_DIR").is_none() {
            if let Ok(workspace) = std::env::var("RANTAICLAW_WORKSPACE") {
                if !workspace.is_empty() {
                    let (_, workspace_dir) =
                        resolve_config_dir_for_workspace(&PathBuf::from(workspace));
                    self.workspace_dir = workspace_dir;
                }
            }
        }

        // Open-skills opt-in flag: RANTAICLAW_OPEN_SKILLS_ENABLED
        if let Ok(flag) = std::env::var("RANTAICLAW_OPEN_SKILLS_ENABLED") {
            if !flag.trim().is_empty() {
                match flag.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" | "on" => self.skills.open_skills_enabled = true,
                    "0" | "false" | "no" | "off" => self.skills.open_skills_enabled = false,
                    _ => tracing::warn!(
                        "Ignoring invalid RANTAICLAW_OPEN_SKILLS_ENABLED (valid: 1|0|true|false|yes|no|on|off)"
                    ),
                }
            }
        }

        // Open-skills directory override: RANTAICLAW_OPEN_SKILLS_DIR
        if let Ok(path) = std::env::var("RANTAICLAW_OPEN_SKILLS_DIR") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                self.skills.open_skills_dir = Some(trimmed.to_string());
            }
        }

        // Skills prompt mode override: RANTAICLAW_SKILLS_PROMPT_MODE
        if let Ok(mode) = std::env::var("RANTAICLAW_SKILLS_PROMPT_MODE") {
            if !mode.trim().is_empty() {
                if let Some(parsed) = parse_skills_prompt_injection_mode(&mode) {
                    self.skills.prompt_injection_mode = parsed;
                } else {
                    tracing::warn!(
                        "Ignoring invalid RANTAICLAW_SKILLS_PROMPT_MODE (valid: full|compact)"
                    );
                }
            }
        }

        // Gateway port: RANTAICLAW_GATEWAY_PORT or PORT
        if let Ok(port_str) =
            std::env::var("RANTAICLAW_GATEWAY_PORT").or_else(|_| std::env::var("PORT"))
        {
            match port_str.trim().parse::<u16>() {
                Ok(port) => self.gateway.port = port,
                Err(_) if port_str.trim().is_empty() => {}
                Err(_) => tracing::warn!(
                    "Ignoring invalid gateway port {port_str:?} (expected 0–65535); keeping {}",
                    self.gateway.port
                ),
            }
        }

        // Gateway host: RANTAICLAW_GATEWAY_HOST or HOST
        if let Ok(host) =
            std::env::var("RANTAICLAW_GATEWAY_HOST").or_else(|_| std::env::var("HOST"))
        {
            if !host.is_empty() {
                self.gateway.host = host;
            }
        }

        // Allow public bind: RANTAICLAW_ALLOW_PUBLIC_BIND
        if let Ok(val) = std::env::var("RANTAICLAW_ALLOW_PUBLIC_BIND") {
            match parse_env_bool(&val) {
                Some(flag) => self.gateway.allow_public_bind = flag,
                None => tracing::warn!(
                    "Ignoring invalid RANTAICLAW_ALLOW_PUBLIC_BIND {val:?} (valid: true/false); \
                     keeping {}",
                    self.gateway.allow_public_bind
                ),
            }
        }

        // Temperature: RANTAICLAW_TEMPERATURE
        if let Ok(temp_str) = std::env::var("RANTAICLAW_TEMPERATURE") {
            match temp_str.trim().parse::<f64>() {
                Ok(temp) if (0.0..=2.0).contains(&temp) => self.default_temperature = temp,
                Err(_) if temp_str.trim().is_empty() => {}
                _ => tracing::warn!(
                    "Ignoring invalid RANTAICLAW_TEMPERATURE {temp_str:?} (expected 0.0–2.0); \
                     keeping {}",
                    self.default_temperature
                ),
            }
        }

        // Reasoning override: RANTAICLAW_REASONING_ENABLED or REASONING_ENABLED
        if let Ok(flag) = std::env::var("RANTAICLAW_REASONING_ENABLED")
            .or_else(|_| std::env::var("REASONING_ENABLED"))
        {
            let normalized = flag.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "1" | "true" | "yes" | "on" => self.runtime.reasoning_enabled = Some(true),
                "0" | "false" | "no" | "off" => self.runtime.reasoning_enabled = Some(false),
                _ => {}
            }
        }

        // Web search enabled: RANTAICLAW_WEB_SEARCH_ENABLED or WEB_SEARCH_ENABLED
        if let Ok(enabled) = std::env::var("RANTAICLAW_WEB_SEARCH_ENABLED")
            .or_else(|_| std::env::var("WEB_SEARCH_ENABLED"))
        {
            match parse_env_bool(&enabled) {
                Some(flag) => self.web_search.enabled = flag,
                None => tracing::warn!(
                    "Ignoring invalid WEB_SEARCH_ENABLED {enabled:?} (valid: true/false); \
                     keeping {}",
                    self.web_search.enabled
                ),
            }
        }

        // Web search provider: RANTAICLAW_WEB_SEARCH_PROVIDER or WEB_SEARCH_PROVIDER
        if let Ok(provider) = std::env::var("RANTAICLAW_WEB_SEARCH_PROVIDER")
            .or_else(|_| std::env::var("WEB_SEARCH_PROVIDER"))
        {
            let provider = provider.trim();
            if !provider.is_empty() {
                self.web_search.provider = provider.to_string();
            }
        }

        // Brave API key: RANTAICLAW_BRAVE_API_KEY or BRAVE_API_KEY
        if let Ok(api_key) =
            std::env::var("RANTAICLAW_BRAVE_API_KEY").or_else(|_| std::env::var("BRAVE_API_KEY"))
        {
            let api_key = api_key.trim();
            if !api_key.is_empty() {
                self.web_search.brave_api_key = Some(api_key.to_string());
            }
        }

        // Web search max results: RANTAICLAW_WEB_SEARCH_MAX_RESULTS or WEB_SEARCH_MAX_RESULTS
        if let Ok(max_results) = std::env::var("RANTAICLAW_WEB_SEARCH_MAX_RESULTS")
            .or_else(|_| std::env::var("WEB_SEARCH_MAX_RESULTS"))
        {
            match max_results.trim().parse::<usize>() {
                Ok(n) if (1..=10).contains(&n) => self.web_search.max_results = n,
                Err(_) if max_results.trim().is_empty() => {}
                _ => tracing::warn!(
                    "Ignoring invalid WEB_SEARCH_MAX_RESULTS {max_results:?} (expected 1–10); \
                     keeping {}",
                    self.web_search.max_results
                ),
            }
        }

        // Web search timeout: RANTAICLAW_WEB_SEARCH_TIMEOUT_SECS or WEB_SEARCH_TIMEOUT_SECS
        if let Ok(timeout_secs) = std::env::var("RANTAICLAW_WEB_SEARCH_TIMEOUT_SECS")
            .or_else(|_| std::env::var("WEB_SEARCH_TIMEOUT_SECS"))
        {
            match timeout_secs.trim().parse::<u64>() {
                Ok(n) if n > 0 => self.web_search.timeout_secs = n,
                Err(_) if timeout_secs.trim().is_empty() => {}
                _ => tracing::warn!(
                    "Ignoring invalid WEB_SEARCH_TIMEOUT_SECS {timeout_secs:?} (expected > 0); \
                     keeping {}",
                    self.web_search.timeout_secs
                ),
            }
        }

        // Proxy enabled flag: RANTAICLAW_PROXY_ENABLED
        let explicit_proxy_enabled = std::env::var("RANTAICLAW_PROXY_ENABLED")
            .ok()
            .as_deref()
            .and_then(parse_env_bool);
        if let Some(enabled) = explicit_proxy_enabled {
            self.proxy.enabled = enabled;
        }

        // Proxy URLs: RANTAICLAW_* wins, then generic *PROXY vars — but never a
        // generic var we authored ourselves (see `read_user_proxy_var`), so the
        // proxy env we wrote last apply cannot be read back as a user signal and
        // resurrect a disabled proxy.
        let mut proxy_url_overridden = false;
        if let Some(proxy_url) = read_user_proxy_var("RANTAICLAW_HTTP_PROXY", "HTTP_PROXY") {
            self.proxy.http_proxy = normalize_proxy_url_option(Some(&proxy_url));
            proxy_url_overridden = true;
        }
        if let Some(proxy_url) = read_user_proxy_var("RANTAICLAW_HTTPS_PROXY", "HTTPS_PROXY") {
            self.proxy.https_proxy = normalize_proxy_url_option(Some(&proxy_url));
            proxy_url_overridden = true;
        }
        if let Some(proxy_url) = read_user_proxy_var("RANTAICLAW_ALL_PROXY", "ALL_PROXY") {
            self.proxy.all_proxy = normalize_proxy_url_option(Some(&proxy_url));
            proxy_url_overridden = true;
        }
        if let Ok(no_proxy) =
            std::env::var("RANTAICLAW_NO_PROXY").or_else(|_| std::env::var("NO_PROXY"))
        {
            self.proxy.no_proxy = normalize_no_proxy_list(vec![no_proxy]);
        }

        if explicit_proxy_enabled.is_none()
            && proxy_url_overridden
            && self.proxy.has_any_proxy_url()
        {
            self.proxy.enabled = true;
        }

        // Proxy scope and service selectors.
        if let Ok(scope_raw) = std::env::var("RANTAICLAW_PROXY_SCOPE") {
            if let Some(scope) = parse_proxy_scope(&scope_raw) {
                self.proxy.scope = scope;
            } else {
                tracing::warn!(
                    scope = %scope_raw,
                    "Ignoring invalid RANTAICLAW_PROXY_SCOPE (valid: environment|rantaiclaw|services)"
                );
            }
        }

        if let Ok(services_raw) = std::env::var("RANTAICLAW_PROXY_SERVICES") {
            self.proxy.services = normalize_service_list(vec![services_raw]);
        }

        if let Err(error) = self.proxy.validate() {
            tracing::warn!("Invalid proxy configuration ignored: {error}");
            self.proxy.enabled = false;
        }

        if self.proxy.enabled && self.proxy.scope == ProxyScope::Environment {
            self.proxy.apply_to_process_env();
        } else {
            // Not applying to the process env (disabled, or a non-Environment
            // scope): clear only the proxy vars WE authored on a previous apply,
            // so `[proxy] enabled = false` actually stops proxying without wiping
            // a user's own shell HTTP_PROXY.
            ProxyConfig::clear_authored_process_env();
        }

        set_runtime_proxy_config(self.proxy.clone());
    }

    /// Undo, in `target` and for persistence only, what the environment
    /// contributed at load — but only where nothing has changed it since.
    ///
    /// A value the process deliberately set after load (a TUI model switch, a
    /// console save) is the operator's and must reach the file, even when the
    /// environment also names that field.
    fn strip_env_overrides(&self, target: &mut Config) -> Result<()> {
        let Some(snapshot) = self.env_overrides.as_deref() else {
            return Ok(());
        };

        let mut value = serde_json::to_value(&*target)
            .context("Failed to inspect config before removing environment overrides")?;
        restore_env_overridden(&snapshot.before, &snapshot.after, &mut value);

        // Fail closed: writing a config that still carries environment values
        // is the defect this exists to prevent, so a rebuild failure aborts
        // the save rather than falling back to it.
        let mut stripped: Config = serde_json::from_value(value)
            .context("Failed to rebuild config without environment overrides")?;

        // JSON carries only the serialised fields; put the skipped ones back.
        stripped.config_path = target.config_path.clone();
        stripped.workspace_dir = target.workspace_dir.clone();
        stripped.env_overrides = target.env_overrides.clone();
        *target = stripped;
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        // Debug-build guard: refuse to write anywhere but a tempdir, so a run
        // that never overrode `config_path` (it defaults to the developer's
        // real `$HOME/.rantaiclaw/config.toml`) cannot write the operator's
        // real config. Compiled out of release builds.
        #[cfg(any(test, debug_assertions))]
        {
            if !config_path_is_test_safe(&self.config_path) {
                anyhow::bail!(
                    "test config isolation: config_path {} is not under a tempdir, so \
                     this test would write the developer's real config. Point \
                     config_path at a tempdir (see crate::test_env::EnvGuard) or, if \
                     this test really must write the real path, set \
                     RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1. This guard also \
                     fires for `cargo run` and spawned debug binaries.",
                    self.config_path.display()
                );
            }
        }

        // Encrypt secrets before serialization
        let mut config_to_save = self.clone();
        // Before anything else: the file must describe the operator's config,
        // not the environment this process happened to run with.
        self.strip_env_overrides(&mut config_to_save)?;
        let rantaiclaw_dir = self
            .config_path
            .parent()
            .context("Config path must have a parent directory")?;
        let store = crate::security::SecretStore::new(rantaiclaw_dir, self.secrets.encrypt);

        encrypt_optional_secret(&store, &mut config_to_save.api_key, "config.api_key")?;
        encrypt_optional_secret(
            &store,
            &mut config_to_save.knowledge.embedding_api_key,
            "config.knowledge.embedding_api_key",
        )?;
        encrypt_optional_secret(
            &store,
            &mut config_to_save.knowledge.vision_api_key,
            "config.knowledge.vision_api_key",
        )?;
        encrypt_optional_secret(
            &store,
            &mut config_to_save.composio.api_key,
            "config.composio.api_key",
        )?;

        encrypt_optional_secret(
            &store,
            &mut config_to_save.browser.computer_use.api_key,
            "config.browser.computer_use.api_key",
        )?;

        encrypt_optional_secret(
            &store,
            &mut config_to_save.web_search.brave_api_key,
            "config.web_search.brave_api_key",
        )?;

        for agent in config_to_save.agents.values_mut() {
            encrypt_optional_secret(&store, &mut agent.api_key, "config.agents.*.api_key")?;
        }
        for agent in config_to_save.gateway_agents.values_mut() {
            encrypt_optional_secret(
                &store,
                &mut agent.api_key,
                "config.gateway_agents.*.api_key",
            )?;
        }

        for key in config_to_save.provider_api_keys.values_mut() {
            let mut wrapped = Some(std::mem::take(key));
            encrypt_optional_secret(&store, &mut wrapped, "config.provider_api_keys.*")?;
            *key = wrapped.unwrap_or_default();
        }

        // MCP server env values are credentials (Notion, Slack and GitHub
        // tokens live here). They were the one credential path that bypassed
        // this function entirely — the config API redacts them on the wire,
        // which made the plaintext on disk easy to miss.
        for (name, server) in &mut config_to_save.mcp_servers {
            for (var, value) in &mut server.env {
                let mut wrapped = Some(std::mem::take(value));
                encrypt_optional_secret(
                    &store,
                    &mut wrapped,
                    &format!("config.mcp_servers.{name}.env.{var}"),
                )?;
                *value = wrapped.unwrap_or_default();
            }
        }

        // Channel bot tokens are secrets too. `bot_token` is a plain `String`, so
        // wrap it in an `Option` to reuse the same encrypt helper (mirrors the
        // `provider_api_keys` handling above).
        if let Some(tg) = config_to_save.channels_config.telegram.as_mut() {
            let mut wrapped = Some(std::mem::take(&mut tg.bot_token));
            encrypt_optional_secret(
                &store,
                &mut wrapped,
                "config.channels_config.telegram.bot_token",
            )?;
            tg.bot_token = wrapped.unwrap_or_default();
        }

        // Skill literal API keys are secrets too — encrypt them symmetrically
        // with every other credential above instead of writing plaintext.
        // `env`-sourced keys have no `value` to encrypt (skip_serializing_if
        // already keeps them out of the file); only `literal` entries apply.
        for (name, entry) in &mut config_to_save.skills.entries {
            if let Some(api_key) = entry.api_key.as_mut() {
                if api_key.source == "literal" {
                    encrypt_optional_secret(
                        &store,
                        &mut api_key.value,
                        &format!("config.skills.entries.{name}.api_key.value"),
                    )?;
                }
            }
        }

        let toml_str =
            toml::to_string_pretty(&config_to_save).context("Failed to serialize config")?;

        // Publish atomically (temp → fsync → backup → rename → dir fsync). save()
        // drops the backup on success; the migration write-back keeps it.
        atomic_write_config(&self.config_path, toml_str.as_bytes(), false).await?;

        Ok(())
    }
}

/// Atomically write `contents` to `target`: create a 0600 temp file, fsync it,
/// back up any existing target to `<name>.bak`, rename into place (restoring the
/// backup on rename failure), and fsync the directory. When `keep_backup` is
/// false the `.bak` is removed on success; when true it is retained as a recovery
/// point — the migration write-back keeps it so a bad `migrate_vN` is reversible.
///
/// The temp name (`.<name>.tmp-<uuid>`) deliberately does NOT match the config
/// watcher's filename filter, so the transient temp file never triggers a reload.
async fn atomic_write_config(target: &Path, contents: &[u8], keep_backup: bool) -> Result<()> {
    let parent_dir = target
        .parent()
        .context("Config path must have a parent directory")?;
    fs::create_dir_all(parent_dir).await.with_context(|| {
        format!(
            "Failed to create config directory: {}",
            parent_dir.display()
        )
    })?;
    let file_name = target
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("config.toml");
    let temp_path = parent_dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::new_v4()));
    let backup_path = parent_dir.join(format!("{file_name}.bak"));

    let mut open_opts = OpenOptions::new();
    open_opts.create_new(true).write(true);
    // 0600 AT OPEN so the config (bot tokens / API keys / paired tokens) is never
    // briefly world-readable under the process umask between create and chmod.
    #[cfg(unix)]
    open_opts.mode(0o600);
    let mut temp_file = open_opts.open(&temp_path).await.with_context(|| {
        format!(
            "Failed to create temporary config file: {}",
            temp_path.display()
        )
    })?;
    temp_file
        .write_all(contents)
        .await
        .context("Failed to write temporary config contents")?;
    temp_file
        .sync_all()
        .await
        .context("Failed to fsync temporary config file")?;
    drop(temp_file);

    // Belt-and-braces: re-assert 0600 on the temp path before it is published.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| {
                format!(
                    "Failed to restrict config permissions to 0600: {}",
                    temp_path.display()
                )
            })?;
    }

    let had_existing = target.exists();
    if had_existing {
        fs::copy(target, &backup_path).await.with_context(|| {
            format!(
                "Failed to create config backup before atomic replace: {}",
                backup_path.display()
            )
        })?;
    }

    if let Err(e) = fs::rename(&temp_path, target).await {
        let _ = fs::remove_file(&temp_path).await;
        if had_existing && backup_path.exists() {
            fs::copy(&backup_path, target)
                .await
                .context("Failed to restore config backup")?;
        }
        anyhow::bail!("Failed to atomically replace config file: {e}");
    }

    sync_directory(parent_dir).await?;

    if had_existing && !keep_backup {
        let _ = fs::remove_file(&backup_path).await;
    }

    Ok(())
}

async fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = File::open(path)
            .await
            .with_context(|| format!("Failed to open directory for fsync: {}", path.display()))?;
        dir.sync_all()
            .await
            .with_context(|| format!("Failed to fsync directory metadata: {}", path.display()))?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Did `[memory].backend` OR `[storage.provider.config].provider` (the
/// storage override wins at runtime) carry the retired `markdown` name
/// before the migration ran? Used by `load_or_init` to gate the one-time
/// markdown import after `migrate_v34` stamps the new schema version.
fn raw_memory_or_storage_was_markdown(raw: &toml::Value) -> bool {
    let is_markdown = |s: &str| s.trim().eq_ignore_ascii_case("markdown");
    if raw
        .get("memory")
        .and_then(|m| m.get("backend"))
        .and_then(toml::Value::as_str)
        .is_some_and(is_markdown)
    {
        return true;
    }
    raw.get("storage")
        .and_then(|s| s.get("provider"))
        .and_then(|p| p.get("config"))
        .and_then(|c| c.get("provider"))
        .and_then(toml::Value::as_str)
        .is_some_and(is_markdown)
}

/// Did `[memory].backend` OR `[storage.provider.config].provider` (the
/// latter overrides the former at runtime, same as
/// [`raw_memory_or_storage_was_markdown`]) carry the retired `postgres`
/// name before the migration ran? Used by `load_or_init` to gate the
/// one-time WARN after `migrate_v35` stamps the new schema version and
/// strips `[storage]`. A config whose only postgres marker was the storage
/// override WAS wired to something: that override took effect at runtime,
/// so its notes really did live in Postgres, and skipping the WARN there
/// would leave the operator unaware their local sqlite store starts empty.
fn raw_memory_backend_was_postgres(raw: &toml::Value) -> bool {
    let is_postgres = |s: &str| s.trim().eq_ignore_ascii_case("postgres");
    if raw
        .get("memory")
        .and_then(|m| m.get("backend"))
        .and_then(toml::Value::as_str)
        .is_some_and(is_postgres)
    {
        return true;
    }
    raw.get("storage")
        .and_then(|s| s.get("provider"))
        .and_then(|p| p.get("config"))
        .and_then(|c| c.get("provider"))
        .and_then(toml::Value::as_str)
        .is_some_and(is_postgres)
}

/// One-time markdown → sqlite import for a config that used the retired
/// `markdown` backend.
///
/// Test-only convenience: backs up (`backup_markdown_memory`) and then
/// imports (`import_markdown_backup_into_sqlite`) in one call. Production
/// code no longer calls this — `Config::load_or_init` makes the backup
/// directly and leaves the import to `retry_unimported_markdown_imports`, so
/// a backup made at schema-bump time and one left over from an earlier
/// failed run go through the exact same import path and the exact same
/// warning.
#[cfg(test)]
fn import_markdown_memory_into_sqlite(workspace_dir: &Path) -> Result<()> {
    let Some(backup_dir) = crate::migration::backup_markdown_memory(workspace_dir)
        .context("back up markdown memory files before import")?
    else {
        // Nothing to import (no MEMORY.md and no daily files). The config
        // still moved to sqlite; nothing further to do.
        return Ok(());
    };
    import_markdown_backup_into_sqlite(workspace_dir, &backup_dir)
}

/// True for a `memory/migrations/<name>/` directory that is a whole,
/// complete markdown backup (see `backup_markdown_memory`): named
/// `markdown-*` and carrying a `BACKUP_COMPLETE` marker. Says nothing about
/// whether it has been imported yet.
fn is_complete_markdown_backup_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("markdown-"))
        && path.join("BACKUP_COMPLETE").exists()
}

/// The newest complete markdown backup under `memory/migrations/`, imported
/// or still pending, if there is one.
///
/// The gate in `Config::load_or_init` checks this before making a new
/// backup. Without it, a config write-back that keeps failing (a read-only
/// config file, say) leaves the on-disk schema at `markdown` forever, so
/// `migrated && markdown_pre_migration` is true again on every start — the
/// gate would otherwise make a fresh, complete backup on every single
/// start, and the sweep would import each one in turn, re-applying
/// markdown-wins and resurrecting rows the operator had since deleted. One
/// existing complete backup is enough: a pending one is the sweep's job to
/// finish, and an imported one has already done its job. Directory names
/// sort by timestamp, so the last name is the newest.
fn newest_complete_markdown_backup(workspace_dir: &Path) -> Option<std::path::PathBuf> {
    let migrations_dir = workspace_dir.join("memory").join("migrations");
    let read_dir = std::fs::read_dir(&migrations_dir).ok()?;
    read_dir
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_complete_markdown_backup_dir(path))
        .max()
}

/// True if a live markdown file holds notes written after `backup_dir` was
/// completed.
///
/// Used when the gate skips the backup because one exists. That backup cannot
/// hold notes written afterwards, for instance on the release the operator
/// rolled back to. `MEMORY.md` counts only when it has notes of its own: the
/// import rewrites it to the projection block, and that write is not a note.
/// Any failure to read means "not newer": the caller only warns.
fn live_markdown_newer_than_backup(workspace_dir: &Path, backup_dir: &Path) -> bool {
    let Ok(completed) =
        std::fs::metadata(backup_dir.join("BACKUP_COMPLETE")).and_then(|meta| meta.modified())
    else {
        return false;
    };
    let mut candidates = vec![workspace_dir.join("MEMORY.md")];
    if let Ok(read_dir) = std::fs::read_dir(workspace_dir.join("memory")) {
        candidates.extend(
            read_dir
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md")),
        );
    }
    candidates.iter().any(|path| {
        let newer = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified > completed);
        newer
            && crate::migration::read_markdown_file_entries(path).is_ok_and(|entries| {
                entries
                    .iter()
                    .any(|entry| !is_template_scaffold_line(&entry.content))
            })
    })
}

/// Pending markdown backups in `paths`, oldest first.
///
/// A backup is pending when it is complete (`BACKUP_COMPLETE` present, see
/// `backup_markdown_memory`) and has no `IMPORTED` marker next to it. The
/// directory name sorts by timestamp, and `read_dir`'s own order is not
/// guaranteed, so the caller's order never decides which backup goes first.
fn sorted_pending_markdown_backups(
    paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut pending: Vec<std::path::PathBuf> = paths
        .into_iter()
        .filter(|path| is_complete_markdown_backup_dir(path) && !path.join("IMPORTED").exists())
        .collect();
    pending.sort();
    pending
}

/// Retry every markdown backup under `memory/migrations/` that is complete
/// and has not finished importing, and honour a `PENDING` marker.
///
/// Called on every `Config::load_or_init` whose configured backend is
/// `sqlite`, not only the load that bumps the schema: an import can fail on
/// a later run, after the config was already written as sqlite, and the
/// marker file — not a config key — is what tells that later run to retry.
/// A `PENDING` marker means a backup itself failed and is still owed. While
/// it exists, each load warns with the marker's path, retries the backup from
/// the live files unless a pending backup already exists, imports, and
/// removes the marker once every import succeeded. The loader also calls this
/// for a `PENDING` marker when the backend is not `sqlite`. When the load's own
/// backup already failed (`backup_failed_this_load`), the sweep does not make
/// a second attempt in the same load.
/// This replaces the removed `rantaiclaw migrate` CLI retry.
///
/// A backup missing `BACKUP_COMPLETE` is either still being written by a
/// concurrent process, was left partial by a failed copy, or is a flat,
/// pre-completeness-marker backup from an earlier development build — none of
/// those are safe to read from, so they are left untouched rather than
/// imported. Pending backups are processed oldest first.
///
/// Cheap when there is nothing to do: in the common case (a config that
/// never named `markdown`) `memory/migrations/` does not exist, so this is a
/// single failed `read_dir`, and the `PENDING` marker, which lives in that
/// directory, costs nothing more.
fn retry_unimported_markdown_imports(workspace_dir: &Path, backup_failed_this_load: bool) {
    let migrations_dir = workspace_dir.join("memory").join("migrations");
    let Ok(read_dir) = std::fs::read_dir(&migrations_dir) else {
        return;
    };

    let mut pending = sorted_pending_markdown_backups(read_dir.filter_map(|entry| match entry {
        Ok(entry) => Some(entry.path()),
        // No content here, just the error chain: a directory entry we
        // could not even stat is not itself a memory value.
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                dir = %migrations_dir.display(),
                "failed to read a directory entry under memory/migrations/"
            );
            None
        }
    }));

    let pending_marker = crate::migration::pending_markdown_import_marker(workspace_dir);
    let marker_present = pending_marker.exists();
    if marker_present {
        // The load's own backup just failed. Trying again here would only fail
        // the same way, so the marker stays for the next start.
        if pending.is_empty() && backup_failed_this_load {
            return;
        }
        tracing::warn!(
            marker = %pending_marker.display(),
            "a markdown memory backup failed earlier and is still owed; retrying it from the \
             live files"
        );
        if pending.is_empty() {
            match crate::migration::backup_markdown_memory(workspace_dir) {
                Ok(Some(backup_dir)) => pending.push(backup_dir),
                // Nothing left to back up: the markdown files are gone.
                Ok(None) => remove_pending_marker(&pending_marker),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        marker = %pending_marker.display(),
                        "the markdown memory backup failed again; it retries on the next start"
                    );
                    return;
                }
            }
            if pending.is_empty() {
                return;
            }
        }
    }

    let mut all_imported = true;
    for path in pending {
        if let Err(e) = import_markdown_backup_into_sqlite(workspace_dir, &path) {
            all_imported = false;
            // No key or content here, only the backup path and the error
            // chain: the message must be safe to paste into a bug report.
            tracing::warn!(
                error = %format!("{e:#}"),
                backup = %path.display(),
                "markdown memory import failed; the config still loads as sqlite, but the \
                 original markdown notes were not migrated. The backup at {0} is intact and \
                 the import retries automatically on the next start.",
                path.display()
            );
        }
    }
    if marker_present && all_imported {
        remove_pending_marker(&pending_marker);
    }
}

fn remove_pending_marker(marker: &Path) {
    if let Err(e) = std::fs::remove_file(marker) {
        tracing::warn!(
            error = %format!("{e:#}"),
            marker = %marker.display(),
            "could not remove the pending markdown import marker; the next start repeats the \
             import harmlessly"
        );
    }
}

/// Open (creating) `brain.db` for the markdown import, through the same
/// schema the backend would use.
///
/// The connection must outlive the inserts; `SqliteMemory::init_schema`
/// builds the FTS5 schema and the trigger pair `memories` keeps in sync with
/// `memories_fts`. Using the backend's own schema (not a duplicate here) is
/// the same reason `hydrate_from_snapshot` does it.
fn open_markdown_import_connection(workspace_dir: &Path) -> Result<rusqlite::Connection> {
    let db_dir = workspace_dir.join("memory");
    std::fs::create_dir_all(&db_dir).context("create memory directory")?;
    let db_path = db_dir.join("brain.db");
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("open {}", db_path.display()))?;
    // A running daemon can hold brain.db for ordinary traffic; wait for it
    // rather than fail immediately with SQLITE_BUSY. Set before any pragma,
    // which itself takes a lock.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .context("set busy timeout")?;
    // `FULL` flushes the WAL at every commit, so the import is durable when
    // `commit` returns. This connection is private to the import; the
    // daemon's own connection keeps its setting.
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
        .context("set sqlite pragmas")?;
    crate::memory::SqliteMemory::init_schema(&conn).context("initialise sqlite schema")?;
    Ok(conn)
}

fn mark_markdown_backup_imported(backup_dir: &Path) -> Result<()> {
    crate::migration::write_marker_durably(&backup_dir.join("IMPORTED"), &[])
        .context("write IMPORTED marker")
}

/// Import the markdown notes frozen in `backup_dir` into `workspace_dir`'s
/// `brain.db`, then best-effort re-render `MEMORY.md` from it.
///
///   1. **Read** entries with `read_openclaw_markdown_entries`, from the
///      backup (not the live workspace) so a retry always sees the same
///      frozen originals regardless of what the live files have become
///      since. Skip lines that match the wizard's `MEMORY.md` template
///      exactly (`crate::memory::MEMORY_MD_TEMPLATE`) and bare `---`
///      separators — placeholder prose the operator did not write.
///
///   2. **Insert** every entry inside one transaction, updating on a
///      `memories.key` conflict: the markdown value is what the operator
///      has been using and wins over a `brain.db` row that is not newer than
///      the backup (the pre-import `brain.db` is what the backup's copy is
///      for). A row in the shared place whose `updated_at` is later than the
///      backup time was stored after the backup, and no backup holds it, so it
///      is kept and counted. A row held by another place (a conversation's
///      `session_id`) is overwritten whatever its age and moves to the shared
///      place; those moves are counted too. The backup time is read from `BACKUP_COMPLETE`. The
///      imported rows carry that time as their own, so a newer backup
///      imported afterwards can still replace them. A key whose value
///      passes `is_autosave_key` is stored as `conversation` regardless of
///      its source category, so the cross-chat memory injection continues
///      to skip it. A `busy_timeout` is set first, since a concurrently
///      running daemon can hold `brain.db` for ordinary traffic.
///
///      The connection runs with `synchronous = FULL`, so the commit is
///      durable on its own. The `IMPORTED` marker is written once this
///      transaction has committed, before step 3 runs. The WAL checkpoint
///      that follows the commit is best-effort: a busy result is logged and
///      does not fail the import. That is what
///      makes this idempotent under failure: once rows are durably imported,
///      nothing here ever re-applies markdown-wins over an edit or a delete
///      made afterward, no matter what happens next.
///
///   3. **Project** core memories into `MEMORY.md` — best-effort, and not
///      gated behind `IMPORTED`. `project_and_rewrite_live_memory_md` only
///      rewrites the live file when it is still byte-identical to the copy
///      the backup holds; an operator edit made since the backup is left
///      alone, and a `WARN` says so. When the import kept a key because a
///      newer row held it, the file is not rewritten to the projection
///      alone, so the operator's line for that key survives. A failure here
///      is logged and does not fail the import overall — a later ordinary
///      memory write already re-projects `MEMORY.md` on its own
///      (`refresh_projection`), so this step never needs its own retry
///      machinery.
///
/// Unless a key was kept as newer, the whole `MEMORY.md` file, not only the
/// block outside the markers, is replaced by the projection once step 3 runs:
/// the backup directory holds the pre-import original for recovery, nothing
/// survives in place.
fn import_markdown_backup_into_sqlite(workspace_dir: &Path, backup_dir: &Path) -> Result<()> {
    use crate::migration::read_openclaw_markdown_entries;
    use rusqlite::OptionalExtension;

    let raw_entries =
        read_openclaw_markdown_entries(backup_dir).context("read markdown entries")?;
    let entries: Vec<_> = raw_entries
        .into_iter()
        .filter(|e| !is_template_scaffold_line(&e.content))
        .collect();

    let mut conn = open_markdown_import_connection(workspace_dir)?;

    // The imported rows carry the backup time, not the import time: a row
    // stored at runtime after the backup stays newer than the note, and a
    // newer backup imported later can still replace an older backup's rows.
    let backup_time = crate::migration::marker_time(&backup_dir.join("BACKUP_COMPLETE"))
        .context("read the backup time")?
        .to_rfc3339();
    let mut imported = 0_usize;
    let mut skipped_newer = 0_usize;
    let mut moved_from_other_place = 0_usize;
    // `Immediate` takes the write lock here, where SQLite runs the busy
    // handler. A deferred transaction would read first and ask for the lock at
    // its first `INSERT`, and SQLite fails that upgrade at once with
    // `SQLITE_BUSY` instead of waiting while a daemon is writing.
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .context("begin markdown import transaction")?;
    for entry in &entries {
        if entry.content.trim().is_empty() {
            continue;
        }
        // An autosave key (`<prefix>_<uuid>`) is runtime-generated, not a
        // name the operator chose; keeping it a `daily`/`core` row would let
        // it re-enter another chat's memory through the shared backfill that
        // only screens `conversation`.
        let category = if crate::memory::is_autosave_key(&entry.key) {
            "conversation"
        } else {
            category_str(&entry.category)
        };
        let id = uuid::Uuid::new_v4().to_string();
        // A row already in another place (`session_id` set) is overwritten
        // whatever its age: its `updated_at` says when a conversation wrote it,
        // not that the operator's note is stale, and skipping it would leave
        // the key nowhere the operator can read it.
        let held_elsewhere = tx
            .query_row(
                "SELECT session_id IS NOT NULL FROM memories WHERE key = ?1",
                rusqlite::params![entry.key],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .context("look up the place of a markdown memory entry")?
            .unwrap_or(false);
        // The update applies to a shared row only while it is not newer than
        // the backup (sqlite reports 0 rows changed when the guard refuses).
        //
        // It also moves the row to the operator's place and drops its vector:
        // `session_id = NULL` and the three embedding columns cleared, so a
        // conversation-scoped row that shared this key is no longer visible
        // to `MemoryView::Only` readers, and `reindex` rebuilds the vector for
        // the new text. A runtime `store` never moves a row's `session_id` and
        // refuses a key held by another place. This one-time import of the
        // operator's own notes is the single intended exception.
        let changed = tx
            .execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT(key) DO UPDATE SET \
                    content = excluded.content, \
                    category = excluded.category, \
                    session_id = NULL, \
                    embedding = NULL, \
                    embedding_model = NULL, \
                    embedding_dims = NULL, \
                    updated_at = excluded.updated_at \
                 WHERE memories.session_id IS NOT NULL OR memories.updated_at <= ?5",
                rusqlite::params![id, entry.key, entry.content, category, backup_time],
            )
            // No key or content in the error context — only the counts
            // below, and those have no operator-authored text in them either.
            .context("insert markdown memory entry")?;
        if changed == 0 {
            skipped_newer += 1;
        } else {
            imported += 1;
            if held_elsewhere {
                moved_from_other_place += 1;
            }
        }
    }
    tx.commit().context("commit markdown import transaction")?;

    // The commit above is already durable (`synchronous = FULL`). Copying the
    // WAL back into the main file is housekeeping: a reader that keeps part of
    // the WAL in use makes the checkpoint report busy, and failing the import
    // for that would make the next start import again and bring back every
    // row deleted in between.
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(0) => {}
        Ok(_) => tracing::info!(
            backup = %backup_dir.display(),
            "markdown import: the WAL checkpoint after the import was blocked by another \
             connection; the imported rows are committed and the WAL is checkpointed later"
        ),
        Err(e) => tracing::warn!(
            error = %format!("{e:#}"),
            backup = %backup_dir.display(),
            "markdown import: the WAL checkpoint after the import failed; the imported rows \
             are committed"
        ),
    }

    // The rows are durable now. Mark this backup done before attempting
    // anything else: everything past this point is best-effort and must
    // never cause a later run to redo the import above.
    mark_markdown_backup_imported(backup_dir).context("write IMPORTED marker")?;

    if let Err(e) = project_and_rewrite_live_memory_md(workspace_dir, backup_dir, skipped_newer > 0)
    {
        // No content here either — just the error chain.
        tracing::warn!(
            error = %format!("{e:#}"),
            backup = %backup_dir.display(),
            "markdown import: MEMORY.md was not re-rendered after the import committed; a \
             later memory write re-projects it"
        );
    }

    tracing::info!(
        imported,
        skipped_newer,
        moved_from_other_place,
        backup = %backup_dir.display(),
        "imported markdown memory entries into sqlite"
    );

    Ok(())
}

/// Re-project core memories into the live `MEMORY.md`, unless an operator
/// edit landed there after the backup was taken.
///
/// `project_core_memories` either splices an existing projection block or
/// appends one — both leave the wizard's `- **key**:` lines and the
/// operator's own structured lines next to the projection, and those lines
/// are now also rows in `brain.db`. `rewrite_memory_md_to_projection_only`
/// then replaces the WHOLE file with the projection block alone, so this
/// must only run while the live file still matches what the backup froze:
/// otherwise an edit made between the backup and this call (a slow first
/// start, a crash and a manual fix, a later retry) would be silently
/// discarded.
fn project_and_rewrite_live_memory_md(
    workspace_dir: &Path,
    backup_dir: &Path,
    keep_operator_lines: bool,
) -> Result<()> {
    use crate::memory::snapshot as snap;

    let live_path = workspace_dir.join("MEMORY.md");
    let live_existed = live_path.exists();
    if live_existed {
        // The reads below, and `project_core_memories`'s own, would block on a
        // FIFO that took the place of the file after the backup.
        crate::migration::require_regular_file(&live_path)?;
    }
    let backed_up_memory_md = backup_dir.join("MEMORY.md");
    let backup_had_memory_md = backed_up_memory_md.exists();
    if backup_had_memory_md {
        let live = std::fs::read_to_string(&live_path).unwrap_or_default();
        let backed_up =
            std::fs::read_to_string(&backed_up_memory_md).context("read backed-up MEMORY.md")?;
        if live != backed_up {
            anyhow::bail!(
                "MEMORY.md changed after the backup was taken; leaving it untouched instead \
                 of overwriting the edit"
            );
        }
    }
    // No `MEMORY.md` existed when the backup was taken (only daily files
    // did): nothing to compare against.

    let _projected = snap::project_core_memories(workspace_dir)
        .context("project core memories into MEMORY.md")?;
    if !backup_had_memory_md && live_existed {
        // The file was created after the backup, by the runtime's own
        // projection, and the operator may have added prose outside the
        // markers since. With no frozen copy to compare against, that prose
        // cannot be told from leftovers, so the file keeps everything
        // `project_core_memories` left in it.
        return Ok(());
    }
    if keep_operator_lines {
        // A key the import skipped as newer is still only in the operator's
        // line here. The projection-only rewrite would drop that line for
        // good, so the file keeps its lines beside the projection block.
        return Ok(());
    }
    rewrite_memory_md_to_projection_only(workspace_dir)
        .context("rewrite MEMORY.md to projection-only")
}

/// True for a content line that came from the wizard's `MEMORY.md` template
/// (or a bare markdown separator). The wizard writes those lines so the
/// operator has a scaffolded file; importing them would put wizard prose
/// into `brain.db` and surface it in the prompt. Filtered at the call site
/// (not inside `read_openclaw_markdown_entries`) so the reader stays
/// general-purpose for the OpenClaw migration path.
///
/// Matches a template line only when equal after trim (mirroring the
/// leading `- ` a bullet line loses on the way to becoming `content`), never
/// by prefix: a prefix match drops a real operator line that merely starts
/// with the same words, and misses a template line with no bullet prefix at
/// all (the italic intro line).
fn is_template_scaffold_line(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed == "---" {
        return true;
    }
    crate::memory::MEMORY_MD_TEMPLATE.lines().any(|line| {
        let line = line.trim();
        line.strip_prefix("- ").unwrap_or(line) == trimmed
    })
}

/// Map the in-process memory category to the on-disk sqlite string.
fn category_str(category: &crate::memory::MemoryCategory) -> &'static str {
    use crate::memory::MemoryCategory;
    match category {
        MemoryCategory::Core => "core",
        MemoryCategory::Daily => "daily",
        MemoryCategory::Conversation => "conversation",
        MemoryCategory::Custom(_) => "custom",
    }
}

/// After the markdown → sqlite import, write `MEMORY.md` to contain only the
/// projection block.
///
/// `project_core_memories` either splices an existing block or appends a new
/// one — both leave the wizard scaffold (`- Daily files (...)`,
/// `## Key Facts`, etc.) and the operator's own structured `- **key**:` lines
/// next to the projection. Every `- **key**:` line is now also a row in
/// `brain.db`, so leaving them in `MEMORY.md` would surface the same fact
/// twice in the system prompt. The backup directory holds the original; this
/// pass replaces the whole file with the projection block alone.
///
/// Takes the FIRST begin marker and the FIRST end marker after it: that is
/// exactly the span `project_core_memories`'s own `splice_block`
/// (`src/memory/snapshot.rs`) just wrote or replaced — `splice_block` itself
/// only ever touches the first pair, copying anything after its end marker
/// through unchanged. A stale second pair or prose between two pairs is
/// therefore always leftover from an old bug or a manual edit, not content
/// `splice_block` produced, and must be dropped rather than kept: keeping it
/// (e.g. by taking the LAST end marker instead) would let it survive every
/// further rewrite forever, alongside the one live block.
fn rewrite_memory_md_to_projection_only(workspace_dir: &Path) -> Result<()> {
    use crate::memory::snapshot::{PROJECTION_BEGIN, PROJECTION_END};

    let path = workspace_dir.join("MEMORY.md");
    let current = std::fs::read_to_string(&path).context("read MEMORY.md after projection")?;
    let start = current
        .find(PROJECTION_BEGIN)
        .context("projection begin marker missing after project_core_memories")?;
    let end = current
        .find(PROJECTION_END)
        .filter(|end| *end > start)
        .context("projection end marker missing or out of order after project_core_memories")?
        + PROJECTION_END.len();

    crate::migration::replace_file_atomically(&path, &current.as_bytes()[start..end])
        .context("write MEMORY.md to projection only")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};
    use tokio::sync::MutexGuard;
    use tokio::test;
    use tokio_stream::wrappers::ReadDirStream;
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn gateway_login_defaults_to_disabled() {
        let g = GatewayConfig::default();
        assert!(g.login.username.is_none());
        assert!(g.login.password_hash.is_none());
        assert_eq!(g.login.idle_timeout_secs, 0, "auto-lock is off by default");
    }

    #[tokio::test]
    async fn gateway_login_round_trips_through_toml() {
        let mut g = GatewayConfig::default();
        g.login.username = Some("op".into());
        g.login.password_hash = Some("$argon2id$v=19$m=1,t=1,p=1$abc$def".into());
        g.login.idle_timeout_secs = 900;
        let s = toml::to_string(&g).unwrap();
        let back: GatewayConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.login.username.as_deref(), Some("op"));
        assert_eq!(back.login.password_hash, g.login.password_hash);
        assert_eq!(back.login.idle_timeout_secs, 900);
    }

    #[tokio::test]
    async fn gateway_login_idle_timeout_is_optional_in_toml() {
        // Configs written before the key existed must still deserialise, and
        // must land on the inert default rather than picking up an auto-lock
        // nobody asked for.
        let back: GatewayConfig =
            toml::from_str("[login]\nusername = \"op\"\n").expect("legacy config parses");
        assert_eq!(back.login.idle_timeout_secs, 0);
    }

    #[tokio::test]
    async fn resolve_key_for_provider_is_provider_aware() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("minimax".into());
        cfg.api_key = Some("minimax-key".into());
        cfg.provider_api_keys
            .insert("openai".into(), "openai-key".into());

        // per-provider store wins
        assert_eq!(
            cfg.resolve_key_for_provider("openai").as_deref(),
            Some("openai-key")
        );
        // top-level api_key applies to the active default provider (+ aliases)
        assert_eq!(
            cfg.resolve_key_for_provider("minimax").as_deref(),
            Some("minimax-key")
        );
        assert_eq!(
            cfg.resolve_key_for_provider("minimax-cn").as_deref(),
            Some("minimax-key")
        );
        // THE bug: the default provider's key must NOT leak to a different
        // provider with no stored key → None, so the env-var fallback applies
        // instead of sending the wrong key.
        assert_eq!(cfg.resolve_key_for_provider("anthropic"), None);
    }

    // ── Defaults ─────────────────────────────────────────────

    #[test]
    async fn config_default_has_sane_values() {
        let c = Config::default();
        assert_eq!(c.default_provider.as_deref(), Some("openrouter"));
        // No baked-in model: a fresh install is unconfigured until setup runs,
        // so the agent won't silently guess a model and the UI shows it blank.
        assert_eq!(c.default_model, None);
        assert!((c.default_temperature - 0.7).abs() < f64::EPSILON);
        assert!(c.api_key.is_none());
        assert!(!c.skills.open_skills_enabled);
        assert_eq!(
            c.skills.prompt_injection_mode,
            SkillsPromptInjectionMode::Full
        );
        assert!(c.workspace_dir.to_string_lossy().contains("workspace"));
        assert!(c.config_path.to_string_lossy().contains("config.toml"));
    }

    #[test]
    async fn config_dir_creation_error_mentions_openrc_and_path() {
        let msg = config_dir_creation_error(Path::new("/etc/rantaiclaw"));
        assert!(msg.contains("/etc/rantaiclaw"));
        assert!(msg.contains("OpenRC"));
        assert!(msg.contains("rantaiclaw"));
    }

    #[test]
    async fn config_schema_export_contains_expected_contract_shape() {
        let schema = schemars::schema_for!(Config);
        let schema_json = serde_json::to_value(&schema).expect("schema should serialize to json");

        assert_eq!(
            schema_json
                .get("$schema")
                .and_then(serde_json::Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema")
        );

        let properties = schema_json
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("schema should expose top-level properties");

        assert!(properties.contains_key("default_provider"));
        assert!(properties.contains_key("skills"));
        assert!(properties.contains_key("gateway"));
        assert!(properties.contains_key("channels_config"));
        assert!(!properties.contains_key("workspace_dir"));
        assert!(!properties.contains_key("config_path"));

        assert!(
            schema_json
                .get("$defs")
                .and_then(serde_json::Value::as_object)
                .is_some(),
            "schema should include reusable type definitions"
        );
    }

    #[test]
    async fn known_top_level_keys_cover_real_fields_and_exclude_computed_paths() {
        let known = known_top_level_config_keys();
        assert!(known.contains("default_provider"));
        assert!(known.contains("gateway"));
        assert!(known.contains("channels_config"));
        // Computed paths are serde-skipped and must not be treated as writable.
        assert!(!known.contains("config_path"));
        assert!(!known.contains("workspace_dir"));
    }

    #[test]
    async fn nearest_known_key_suggests_typo_but_not_foreign_key() {
        let known = known_top_level_config_keys();
        assert_eq!(
            nearest_known_key("defualt_provider", &known),
            Some("default_provider")
        );
        // A key with no near neighbour gets no misleading suggestion.
        assert_eq!(nearest_known_key("zzzzzzzzzzzz", &known), None);
    }

    #[test]
    async fn unknown_key_warning_does_not_flag_known_keys() {
        // The warn path must be silent (no panic, no fail) for a well-formed
        // config; this exercises the table walk over every real top-level key.
        let raw =
            toml::Value::try_from(Config::default()).expect("default config serializes to toml");
        warn_on_unknown_top_level_config_keys(&raw, Path::new("/tmp/config.toml"));
    }

    /// `[security.*]` has to stay an unknown top-level key so the
    /// unknown-key warning keeps reporting the section at load — the audit
    /// trail runs on `AuditConfig::default()` at the gateway's config-change
    /// logger and the runtime `AuditLogger`, with no parsed `[security.*]`
    /// block to source a per-deployment config from. The day `security`
    /// becomes a known top-level key, the warning goes silent; revisit the
    /// README's claim that operators see the warning when they put one in.
    #[test]
    async fn security_is_not_a_known_top_level_key() {
        let known = known_top_level_config_keys();
        assert!(
            !known.contains("security"),
            "`[security.*]` now parses silently — update the README claim that the warning still fires"
        );
    }

    #[test]
    async fn observability_config_default() {
        let o = ObservabilityConfig::default();
        assert_eq!(o.backend, "none");
    }

    #[test]
    async fn autonomy_config_default() {
        let a = AutonomyConfig::default();
        assert_eq!(a.level, AutonomyLevel::Supervised);
        assert!(a.workspace_only);
        assert!(a.allowed_commands.contains(&"git".to_string()));
        assert!(a.allowed_commands.contains(&"cargo".to_string()));
        assert!(a.forbidden_paths.contains(&"/etc".to_string()));
        assert_eq!(a.max_actions_per_hour, 200);
        assert!(a.require_approval_for_medium_risk);
        // Easy-mode default: high-risk commands are no longer hard-blocked.
        assert!(!a.block_high_risk_commands);
    }

    #[test]
    async fn runtime_config_default() {
        let r = RuntimeConfig::default();
        assert_eq!(r.kind, "native");
        assert_eq!(r.docker.image, "alpine:3.20");
        assert_eq!(r.docker.network, "none");
        assert_eq!(r.docker.memory_limit_mb, Some(512));
        assert_eq!(r.docker.cpu_limit, Some(1.0));
        assert!(r.docker.read_only_rootfs);
        assert!(r.docker.mount_workspace);
    }

    #[test]
    async fn heartbeat_config_default() {
        let h = HeartbeatConfig::default();
        assert!(!h.enabled);
        assert_eq!(h.interval_minutes, 30);
    }

    #[test]
    async fn cron_config_default() {
        let c = CronConfig::default();
        assert!(c.enabled);
        assert_eq!(c.max_run_history, 50);
    }

    #[test]
    async fn cron_config_serde_roundtrip() {
        let c = CronConfig {
            enabled: false,
            max_run_history: 100,
            max_catchup_age_secs: 3600,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: CronConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.enabled);
        assert_eq!(parsed.max_run_history, 100);
        assert_eq!(parsed.max_catchup_age_secs, 3600);
    }

    #[test]
    async fn config_defaults_cron_when_section_missing() {
        let toml_str = r#"
workspace_dir = "/tmp/workspace"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;

        let parsed: Config = toml::from_str(toml_str).unwrap();
        assert!(parsed.cron.enabled);
        assert_eq!(parsed.cron.max_run_history, 50);
    }

    #[test]
    async fn memory_config_default_hygiene_settings() {
        let m = MemoryConfig::default();
        assert_eq!(m.backend, "sqlite");
        assert!(m.auto_save);
        assert!(m.hygiene_enabled);
        assert_eq!(m.archive_after_days, 7);
        assert_eq!(m.purge_after_days, 30);
        assert_eq!(m.conversation_retention_days, 30);
        assert!(m.sqlite_open_timeout_secs.is_none());
    }

    #[test]
    async fn channels_config_default() {
        let c = ChannelsConfig::default();
        assert!(c.cli);
        assert!(c.telegram.is_none());
        assert!(c.discord.is_none());
    }

    // ── Serde round-trip ─────────────────────────────────────

    #[test]
    async fn config_toml_roundtrip() {
        let config = Config {
            env_overrides: None,
            ui: UiConfig::default(),
            schema_version: crate::config::migrations::CURRENT_VERSION,
            workspace_dir: PathBuf::from("/tmp/test/workspace"),
            config_path: PathBuf::from("/tmp/test/config.toml"),
            api_key: Some("sk-test-key".into()),
            provider_api_keys: HashMap::from([(
                "openai".to_string(),
                "sk-openai-roundtrip".to_string(),
            )]),
            api_url: None,
            default_provider: Some("openrouter".into()),
            default_model: Some("gpt-4o".into()),
            default_temperature: 0.5,
            observability: ObservabilityConfig {
                backend: "log".into(),
                ..ObservabilityConfig::default()
            },
            autonomy: AutonomyConfig {
                level: AutonomyLevel::Full,
                workspace_only: false,
                allowed_commands: vec!["docker".into()],
                forbidden_paths: vec!["/secret".into()],
                max_actions_per_hour: 50,
                require_approval_for_medium_risk: false,
                block_high_risk_commands: true,
                auto_approve: vec!["file_read".into()],
                always_ask: vec![],
            },
            runtime: RuntimeConfig {
                kind: "docker".into(),
                ..RuntimeConfig::default()
            },
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            skills: SkillsConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            query_classification: QueryClassificationConfig::default(),
            heartbeat: HeartbeatConfig {
                enabled: true,
                interval_minutes: 15,
            },
            cron: CronConfig::default(),
            tasks: TasksConfig::default(),
            channels_config: ChannelsConfig {
                cli: true,
                telegram: Some(TelegramConfig {
                    bot_token: "123:ABC".into(),
                    allowed_users: vec!["user1".into()],
                    stream_mode: StreamMode::default(),
                    draft_update_interval_ms: default_draft_update_interval_ms(),
                    interrupt_on_new_message: false,
                    mention_only: false,
                }),
                discord: None,
                slack: None,
                mattermost: None,
                webhook: None,
                imessage: None,
                matrix: None,
                signal: None,
                whatsapp: None,
                whatsapp_web: None,
                linq: None,
                nextcloud_talk: None,
                email: None,
                irc: None,
                lark: None,
                dingtalk: None,
                qq: None,
                message_timeout_secs: 300,
                autonomous_tools: false,
                thread_replies: true,
                approval_owners: Vec::new(),
                guest_allowed_tools: Vec::new(),
                guest_allowed_commands: Vec::new(),
            },
            memory: MemoryConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            composio: ComposioConfig::default(),
            knowledge: KnowledgeConfig::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_search: WebSearchConfig::default(),
            services: ServicesConfig::default(),
            proxy: ProxyConfig::default(),
            agent: AgentConfig::default(),
            identity: IdentityConfig::default(),
            cost: CostConfig::default(),
            agents: HashMap::new(),
            gateway_agents: HashMap::new(),
            mcp_servers: HashMap::new(),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.api_key, config.api_key);
        assert_eq!(parsed.provider_api_keys, config.provider_api_keys);
        assert_eq!(parsed.default_provider, config.default_provider);
        assert_eq!(parsed.default_model, config.default_model);
        assert!((parsed.default_temperature - config.default_temperature).abs() < f64::EPSILON);
        assert_eq!(parsed.observability.backend, "log");
        assert_eq!(parsed.autonomy.level, AutonomyLevel::Full);
        assert!(!parsed.autonomy.workspace_only);
        assert_eq!(parsed.runtime.kind, "docker");
        assert!(parsed.heartbeat.enabled);
        assert_eq!(parsed.heartbeat.interval_minutes, 15);
        assert!(parsed.channels_config.telegram.is_some());
        assert_eq!(
            parsed.channels_config.telegram.unwrap().bot_token,
            "123:ABC"
        );
    }

    #[test]
    async fn config_minimal_toml_uses_defaults() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(parsed.api_key.is_none());
        assert!(parsed.default_provider.is_none());
        assert_eq!(parsed.observability.backend, "none");
        assert_eq!(parsed.autonomy.level, AutonomyLevel::Supervised);
        assert_eq!(parsed.runtime.kind, "native");
        assert!(!parsed.heartbeat.enabled);
        assert!(parsed.channels_config.cli);
        assert!(parsed.memory.hygiene_enabled);
        assert_eq!(parsed.memory.archive_after_days, 7);
        assert_eq!(parsed.memory.purge_after_days, 30);
        assert_eq!(parsed.memory.conversation_retention_days, 30);
    }

    /// A channel table without `mention_only` deserialises to `true`: in a
    /// group the bot answers only when addressed, unless the operator turned
    /// that off explicitly.
    #[test]
    async fn telegram_and_discord_mention_only_default_true_when_key_absent() {
        let raw = r#"
default_temperature = 0.7

[channels_config.telegram]
bot_token = "123:ABC"
allowed_users = ["*"]

[channels_config.discord]
bot_token = "discord-bot-token"
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        let telegram = parsed
            .channels_config
            .telegram
            .expect("telegram table parses");
        assert!(
            telegram.mention_only,
            "telegram mention_only must default to true"
        );
        let discord = parsed
            .channels_config
            .discord
            .expect("discord table parses");
        assert!(
            discord.mention_only,
            "discord mention_only must default to true"
        );
    }

    /// An operator's written `mention_only = false` is a choice, not an
    /// absence: deserialisation must keep it.
    #[test]
    async fn telegram_and_discord_explicit_mention_only_false_is_kept() {
        let raw = r#"
default_temperature = 0.7

[channels_config.telegram]
bot_token = "123:ABC"
allowed_users = ["*"]
mention_only = false

[channels_config.discord]
bot_token = "discord-bot-token"
mention_only = false
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert!(
            !parsed
                .channels_config
                .telegram
                .expect("telegram table parses")
                .mention_only
        );
        assert!(
            !parsed
                .channels_config
                .discord
                .expect("discord table parses")
                .mention_only
        );
    }

    #[test]
    async fn runtime_reasoning_enabled_deserializes() {
        let raw = r#"
default_temperature = 0.7

[runtime]
reasoning_enabled = false
"#;

        let parsed: Config = toml::from_str(raw).unwrap();
        assert_eq!(parsed.runtime.reasoning_enabled, Some(false));
    }

    #[test]
    async fn agent_config_defaults() {
        let cfg = AgentConfig::default();
        assert!(!cfg.compact_context);
        assert_eq!(cfg.max_tool_iterations, 50);
        assert_eq!(cfg.max_history_messages, 50);
        assert_eq!(cfg.tool_dispatcher, "auto");
    }

    #[test]
    async fn agent_config_deserializes() {
        let raw = r#"
default_temperature = 0.7
[agent]
compact_context = true
max_tool_iterations = 20
max_history_messages = 80
tool_dispatcher = "xml"
"#;
        let parsed: Config = toml::from_str(raw).unwrap();
        assert!(parsed.agent.compact_context);
        assert_eq!(parsed.agent.max_tool_iterations, 20);
        assert_eq!(parsed.agent.max_history_messages, 80);
        assert_eq!(parsed.agent.tool_dispatcher, "xml");
    }

    #[tokio::test]
    async fn sync_directory_handles_existing_directory() {
        let dir = std::env::temp_dir().join(format!(
            "rantaiclaw_test_sync_directory_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).await.unwrap();

        sync_directory(&dir).await.unwrap();

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_save_and_load_tmpdir() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_config");
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(&dir).await.unwrap();

        let config_path = dir.join("config.toml");
        let config = Config {
            env_overrides: None,
            ui: UiConfig::default(),
            schema_version: crate::config::migrations::CURRENT_VERSION,
            workspace_dir: dir.join("workspace"),
            config_path: config_path.clone(),
            api_key: Some("sk-roundtrip".into()),
            provider_api_keys: HashMap::new(),
            api_url: None,
            default_provider: Some("openrouter".into()),
            default_model: Some("test-model".into()),
            default_temperature: 0.9,
            observability: ObservabilityConfig::default(),
            autonomy: AutonomyConfig::default(),
            runtime: RuntimeConfig::default(),
            reliability: ReliabilityConfig::default(),
            scheduler: SchedulerConfig::default(),
            skills: SkillsConfig::default(),
            model_routes: Vec::new(),
            embedding_routes: Vec::new(),
            query_classification: QueryClassificationConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            cron: CronConfig::default(),
            tasks: TasksConfig::default(),
            channels_config: ChannelsConfig::default(),
            memory: MemoryConfig::default(),
            tunnel: TunnelConfig::default(),
            gateway: GatewayConfig::default(),
            composio: ComposioConfig::default(),
            knowledge: KnowledgeConfig::default(),
            secrets: SecretsConfig::default(),
            browser: BrowserConfig::default(),
            http_request: HttpRequestConfig::default(),
            multimodal: MultimodalConfig::default(),
            web_search: WebSearchConfig::default(),
            services: ServicesConfig::default(),
            proxy: ProxyConfig::default(),
            agent: AgentConfig::default(),
            identity: IdentityConfig::default(),
            cost: CostConfig::default(),
            agents: HashMap::new(),
            gateway_agents: HashMap::new(),
            mcp_servers: HashMap::new(),
        };

        config.save().await.unwrap();
        assert!(config_path.exists());

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        let loaded: Config = toml::from_str(&contents).unwrap();
        assert!(loaded
            .api_key
            .as_deref()
            .is_some_and(crate::security::SecretStore::is_encrypted));
        let store = crate::security::SecretStore::new(&dir, true);
        let decrypted = store.decrypt(loaded.api_key.as_deref().unwrap()).unwrap();
        assert_eq!(decrypted, "sk-roundtrip");
        assert_eq!(loaded.default_model.as_deref(), Some("test-model"));
        assert!((loaded.default_temperature - 0.9).abs() < f64::EPSILON);

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_save_encrypts_nested_credentials() {
        let dir = std::env::temp_dir().join(format!(
            "rantaiclaw_test_nested_credentials_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = dir.join("config.toml");
        config.api_key = Some("root-credential".into());
        config.composio.api_key = Some("composio-credential".into());
        config.browser.computer_use.api_key = Some("browser-credential".into());
        config.web_search.brave_api_key = Some("brave-credential".into());

        config.agents.insert(
            "worker".into(),
            DelegateAgentConfig {
                provider: "openrouter".into(),
                model: "model-test".into(),
                system_prompt: None,
                api_key: Some("agent-credential".into()),
                temperature: None,
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
            },
        );

        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(config.config_path.clone())
            .await
            .unwrap();
        let stored: Config = toml::from_str(&contents).unwrap();
        let store = crate::security::SecretStore::new(&dir, true);

        let root_encrypted = stored.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(root_encrypted));
        assert_eq!(store.decrypt(root_encrypted).unwrap(), "root-credential");

        let composio_encrypted = stored.composio.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(
            composio_encrypted
        ));
        assert_eq!(
            store.decrypt(composio_encrypted).unwrap(),
            "composio-credential"
        );

        let browser_encrypted = stored.browser.computer_use.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(
            browser_encrypted
        ));
        assert_eq!(
            store.decrypt(browser_encrypted).unwrap(),
            "browser-credential"
        );

        let web_search_encrypted = stored.web_search.brave_api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(
            web_search_encrypted
        ));
        assert_eq!(
            store.decrypt(web_search_encrypted).unwrap(),
            "brave-credential"
        );

        let worker = stored.agents.get("worker").unwrap();
        let worker_encrypted = worker.api_key.as_deref().unwrap();
        assert!(crate::security::SecretStore::is_encrypted(worker_encrypted));
        assert_eq!(store.decrypt(worker_encrypted).unwrap(), "agent-credential");

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_save_encrypts_telegram_bot_token() {
        // The Telegram bot token is a channel secret and must be encrypted at rest
        // just like `api_key` — the allowlist is not a secret and stays plaintext.
        let dir =
            std::env::temp_dir().join(format!("rantaiclaw_test_tg_token_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = dir.join("config.toml");
        config.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": "123456789:secret-bot-token-value",
                "allowed_users": ["alice"],
            }))
            .unwrap(),
        );

        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config.config_path)
            .await
            .unwrap();
        let stored: Config = toml::from_str(&contents).unwrap();
        let store = crate::security::SecretStore::new(&dir, true);

        let tg = stored.channels_config.telegram.as_ref().unwrap();
        assert!(
            crate::security::SecretStore::is_encrypted(&tg.bot_token),
            "bot_token must be encrypted at rest"
        );
        assert_eq!(
            store.decrypt(&tg.bot_token).unwrap(),
            "123456789:secret-bot-token-value"
        );
        // The allowlist is not a secret — it must stay readable.
        assert_eq!(tg.allowed_users, vec!["alice".to_string()]);

        let _ = fs::remove_dir_all(&dir).await;
    }

    /// `decrypt_config_secrets` is the single decrypt authority shared by
    /// `load_or_init` and the TUI's `reload_config`. Round-trip the three
    /// fields whose decrypt coverage has historically drifted between those
    /// two callers: a per-provider key, the Telegram bot token, and a
    /// literal skill key. save() encrypts them; one shared call must bring
    /// all three back to plaintext.
    #[tokio::test]
    async fn decrypt_config_secrets_round_trips_every_drift_prone_field() {
        let dir = std::env::temp_dir().join(format!(
            "rantaiclaw_test_decrypt_pass_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).await.unwrap();

        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = dir.join("config.toml");
        config.secrets.encrypt = true;
        config
            .provider_api_keys
            .insert("openrouter".into(), "neutral-provider-key-value".into());
        config.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": "123456789:neutral-bot-token-value",
                "allowed_users": ["rantaiclaw_user"],
            }))
            .unwrap(),
        );
        config.skills.entries.insert(
            "x".into(),
            SkillEntryConfig {
                api_key: Some(SkillApiKey {
                    source: "literal".into(),
                    id: None,
                    value: Some("neutral-skill-key-value".into()),
                }),
                ..Default::default()
            },
        );
        // An MCP server's `env` carries the token the server authenticates
        // with — the same class of credential as the three above.
        config.mcp_servers.insert(
            "example".into(),
            McpServerConfig {
                command: "npx".into(),
                args: vec!["-y".into(), "example-mcp-server".into()],
                env: [(
                    "EXAMPLE_API_KEY".to_string(),
                    "neutral-mcp-key-value".to_string(),
                )]
                .into_iter()
                .collect(),
            },
        );

        config.save().await.unwrap();

        // At rest: all three encrypted.
        let contents = tokio::fs::read_to_string(&config.config_path)
            .await
            .unwrap();
        let mut stored: Config = toml::from_str(&contents).unwrap();
        assert!(
            crate::security::SecretStore::is_encrypted(&stored.provider_api_keys["openrouter"]),
            "provider key must be encrypted at rest"
        );
        assert!(
            crate::security::SecretStore::is_encrypted(
                &stored.channels_config.telegram.as_ref().unwrap().bot_token
            ),
            "bot_token must be encrypted at rest"
        );
        assert!(
            crate::security::SecretStore::is_encrypted(
                stored.skills.entries["x"]
                    .api_key
                    .as_ref()
                    .unwrap()
                    .value
                    .as_ref()
                    .unwrap()
            ),
            "skill literal key must be encrypted at rest"
        );
        assert!(
            crate::security::SecretStore::is_encrypted(
                &stored.mcp_servers["example"].env["EXAMPLE_API_KEY"]
            ),
            "MCP server env value must be encrypted at rest: {}",
            stored.mcp_servers["example"].env["EXAMPLE_API_KEY"]
        );

        // One shared pass restores all of them.
        let store = crate::security::SecretStore::new(&dir, true);
        decrypt_config_secrets(&store, &mut stored).unwrap();
        assert_eq!(
            stored.provider_api_keys["openrouter"],
            "neutral-provider-key-value"
        );
        assert_eq!(
            stored.channels_config.telegram.as_ref().unwrap().bot_token,
            "123456789:neutral-bot-token-value"
        );
        assert_eq!(
            stored.skills.entries["x"]
                .api_key
                .as_ref()
                .unwrap()
                .value
                .as_deref(),
            Some("neutral-skill-key-value")
        );
        assert_eq!(
            stored.mcp_servers["example"].env["EXAMPLE_API_KEY"], "neutral-mcp-key-value",
            "the MCP server must receive the plaintext token it authenticates with"
        );

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn literal_skill_api_key_not_plaintext_in_serialized_config() {
        // A `source = "literal"` skill API key is a credential like any
        // other and must round-trip through the secret store exactly the
        // way `config.api_key` / the Telegram bot token do (plan 045,
        // SECURITY DX-01) — never written to disk as plaintext.
        let dir = std::env::temp_dir().join(format!(
            "rantaiclaw_test_skill_literal_key_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).await.unwrap();

        let plaintext = "neutral-test-skill-key-value";
        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = dir.join("config.toml");
        config.secrets.encrypt = true;
        config.skills.entries.insert(
            "x".into(),
            SkillEntryConfig {
                api_key: Some(SkillApiKey {
                    source: "literal".into(),
                    id: None,
                    value: Some(plaintext.into()),
                }),
                ..Default::default()
            },
        );

        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config.config_path)
            .await
            .unwrap();
        assert!(
            !contents.contains(plaintext),
            "literal skill api_key value leaked into config.toml as plaintext:\n{contents}"
        );

        let stored: Config = toml::from_str(&contents).unwrap();
        let store = crate::security::SecretStore::new(&dir, true);
        let encrypted_value = stored
            .skills
            .entries
            .get("x")
            .and_then(|e| e.api_key.as_ref())
            .and_then(|k| k.value.as_deref())
            .expect("literal api_key.value survives serialization (encrypted)");
        assert!(
            crate::security::SecretStore::is_encrypted(encrypted_value),
            "expected an enc2:/enc: prefixed value, got {encrypted_value:?}"
        );
        // Symmetric with the load path in `Config::load_or_init` — proves
        // `src/tools/mod.rs` still receives the plaintext value at runtime.
        assert_eq!(store.decrypt(encrypted_value).unwrap(), plaintext);

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn config_save_atomic_cleanup() {
        let dir =
            std::env::temp_dir().join(format!("rantaiclaw_test_config_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();

        let config_path = dir.join("config.toml");
        let mut config = Config::default();
        config.workspace_dir = dir.join("workspace");
        config.config_path = config_path.clone();
        config.default_model = Some("model-a".into());
        config.save().await.unwrap();
        assert!(config_path.exists());

        config.default_model = Some("model-b".into());
        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(contents.contains("model-b"));

        let names: Vec<String> = ReadDirStream::new(fs::read_dir(&dir).await.unwrap())
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect()
            .await;
        assert!(!names.iter().any(|name| name.contains(".tmp-")));
        assert!(!names.iter().any(|name| name.ends_with(".bak")));

        let _ = fs::remove_dir_all(&dir).await;
    }

    // ── Telegram / Discord config ────────────────────────────

    #[test]
    async fn telegram_config_serde() {
        let tc = TelegramConfig {
            bot_token: "123:XYZ".into(),
            allowed_users: vec!["alice".into(), "bob".into()],
            stream_mode: StreamMode::Partial,
            draft_update_interval_ms: 500,
            interrupt_on_new_message: true,
            mention_only: false,
        };
        let json = serde_json::to_string(&tc).unwrap();
        let parsed: TelegramConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.bot_token, "123:XYZ");
        assert_eq!(parsed.allowed_users.len(), 2);
        assert_eq!(parsed.stream_mode, StreamMode::Partial);
        assert_eq!(parsed.draft_update_interval_ms, 500);
        assert!(parsed.interrupt_on_new_message);
    }

    #[test]
    async fn telegram_config_defaults_stream_off() {
        let json = r#"{"bot_token":"tok","allowed_users":[]}"#;
        let parsed: TelegramConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.stream_mode, StreamMode::Off);
        assert_eq!(parsed.draft_update_interval_ms, 1000);
        assert!(!parsed.interrupt_on_new_message);
    }

    #[test]
    async fn discord_config_serde() {
        let dc = DiscordConfig {
            bot_token: "discord-token".into(),
            guild_id: Some("12345".into()),
            allowed_users: vec![],
            listen_to_bots: false,
            mention_only: false,
        };
        let json = serde_json::to_string(&dc).unwrap();
        let parsed: DiscordConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.bot_token, "discord-token");
        assert_eq!(parsed.guild_id.as_deref(), Some("12345"));
    }

    #[test]
    async fn discord_config_optional_guild() {
        let dc = DiscordConfig {
            bot_token: "tok".into(),
            guild_id: None,
            allowed_users: vec![],
            listen_to_bots: false,
            mention_only: false,
        };
        let json = serde_json::to_string(&dc).unwrap();
        let parsed: DiscordConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.guild_id.is_none());
    }

    // ── iMessage / Matrix config ────────────────────────────

    #[test]
    async fn imessage_config_serde() {
        let ic = IMessageConfig {
            allowed_contacts: vec!["+1234567890".into(), "user@icloud.com".into()],
        };
        let json = serde_json::to_string(&ic).unwrap();
        let parsed: IMessageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.allowed_contacts.len(), 2);
        assert_eq!(parsed.allowed_contacts[0], "+1234567890");
    }

    #[test]
    async fn imessage_config_empty_contacts() {
        let ic = IMessageConfig {
            allowed_contacts: vec![],
        };
        let json = serde_json::to_string(&ic).unwrap();
        let parsed: IMessageConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.allowed_contacts.is_empty());
    }

    #[test]
    async fn imessage_config_wildcard() {
        let ic = IMessageConfig {
            allowed_contacts: vec!["*".into()],
        };
        let toml_str = toml::to_string(&ic).unwrap();
        let parsed: IMessageConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.allowed_contacts, vec!["*"]);
    }

    #[test]
    async fn matrix_config_serde() {
        let mc = MatrixConfig {
            homeserver: "https://matrix.org".into(),
            access_token: "syt_token_abc".into(),
            user_id: Some("@bot:matrix.org".into()),
            device_id: Some("DEVICE123".into()),
            room_id: "!room123:matrix.org".into(),
            allowed_users: vec!["@user:matrix.org".into()],
        };
        let json = serde_json::to_string(&mc).unwrap();
        let parsed: MatrixConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.homeserver, "https://matrix.org");
        assert_eq!(parsed.access_token, "syt_token_abc");
        assert_eq!(parsed.user_id.as_deref(), Some("@bot:matrix.org"));
        assert_eq!(parsed.device_id.as_deref(), Some("DEVICE123"));
        assert_eq!(parsed.room_id, "!room123:matrix.org");
        assert_eq!(parsed.allowed_users.len(), 1);
    }

    #[test]
    async fn matrix_config_toml_roundtrip() {
        let mc = MatrixConfig {
            homeserver: "https://synapse.local:8448".into(),
            access_token: "tok".into(),
            user_id: None,
            device_id: None,
            room_id: "!abc:synapse.local".into(),
            allowed_users: vec!["@admin:synapse.local".into(), "*".into()],
        };
        let toml_str = toml::to_string(&mc).unwrap();
        let parsed: MatrixConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.homeserver, "https://synapse.local:8448");
        assert_eq!(parsed.allowed_users.len(), 2);
    }

    #[test]
    async fn matrix_config_backward_compatible_without_session_hints() {
        let toml = r#"
homeserver = "https://matrix.org"
access_token = "tok"
room_id = "!ops:matrix.org"
allowed_users = ["@ops:matrix.org"]
"#;

        let parsed: MatrixConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.homeserver, "https://matrix.org");
        assert!(parsed.user_id.is_none());
        assert!(parsed.device_id.is_none());
    }

    #[test]
    async fn signal_config_serde() {
        let sc = SignalConfig {
            http_url: "http://127.0.0.1:8686".into(),
            account: "+1234567890".into(),
            group_id: Some("group123".into()),
            allowed_from: vec!["+1111111111".into()],
            ignore_attachments: true,
            ignore_stories: false,
        };
        let json = serde_json::to_string(&sc).unwrap();
        let parsed: SignalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.http_url, "http://127.0.0.1:8686");
        assert_eq!(parsed.account, "+1234567890");
        assert_eq!(parsed.group_id.as_deref(), Some("group123"));
        assert_eq!(parsed.allowed_from.len(), 1);
        assert!(parsed.ignore_attachments);
        assert!(!parsed.ignore_stories);
    }

    #[test]
    async fn signal_config_toml_roundtrip() {
        let sc = SignalConfig {
            http_url: "http://localhost:8080".into(),
            account: "+9876543210".into(),
            group_id: None,
            allowed_from: vec!["*".into()],
            ignore_attachments: false,
            ignore_stories: true,
        };
        let toml_str = toml::to_string(&sc).unwrap();
        let parsed: SignalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.http_url, "http://localhost:8080");
        assert_eq!(parsed.account, "+9876543210");
        assert!(parsed.group_id.is_none());
        assert!(parsed.ignore_stories);
    }

    #[test]
    async fn signal_config_defaults() {
        let json = r#"{"http_url":"http://127.0.0.1:8686","account":"+1234567890"}"#;
        let parsed: SignalConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.group_id.is_none());
        assert!(parsed.allowed_from.is_empty());
        assert!(!parsed.ignore_attachments);
        assert!(!parsed.ignore_stories);
    }

    #[test]
    async fn channels_config_with_imessage_and_matrix() {
        let c = ChannelsConfig {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: Some(IMessageConfig {
                allowed_contacts: vec!["+1".into()],
            }),
            matrix: Some(MatrixConfig {
                homeserver: "https://m.org".into(),
                access_token: "tok".into(),
                user_id: None,
                device_id: None,
                room_id: "!r:m".into(),
                allowed_users: vec!["@u:m".into()],
            }),
            signal: None,
            whatsapp: None,
            whatsapp_web: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            message_timeout_secs: 300,
            autonomous_tools: false,
            thread_replies: true,
            approval_owners: Vec::new(),
            guest_allowed_tools: Vec::new(),
            guest_allowed_commands: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&c).unwrap();
        let parsed: ChannelsConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.imessage.is_some());
        assert!(parsed.matrix.is_some());
        assert_eq!(parsed.imessage.unwrap().allowed_contacts, vec!["+1"]);
        assert_eq!(parsed.matrix.unwrap().homeserver, "https://m.org");
    }

    #[test]
    async fn channels_config_default_has_no_imessage_matrix() {
        let c = ChannelsConfig::default();
        assert!(c.imessage.is_none());
        assert!(c.matrix.is_none());
    }

    // ── Edge cases: serde(default) for allowed_users ─────────

    #[test]
    async fn discord_config_deserializes_without_allowed_users() {
        // Old configs won't have allowed_users — serde(default) should fill vec![]
        let json = r#"{"bot_token":"tok","guild_id":"123"}"#;
        let parsed: DiscordConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.allowed_users.is_empty());
    }

    #[test]
    async fn discord_config_deserializes_with_allowed_users() {
        let json = r#"{"bot_token":"tok","guild_id":"123","allowed_users":["111","222"]}"#;
        let parsed: DiscordConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.allowed_users, vec!["111", "222"]);
    }

    #[test]
    async fn slack_config_deserializes_without_allowed_users() {
        let json = r#"{"bot_token":"xoxb-tok"}"#;
        let parsed: SlackConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.allowed_users.is_empty());
    }

    #[test]
    async fn slack_config_deserializes_with_allowed_users() {
        let json = r#"{"bot_token":"xoxb-tok","allowed_users":["U111"]}"#;
        let parsed: SlackConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.allowed_users, vec!["U111"]);
    }

    #[test]
    async fn discord_config_toml_backward_compat() {
        let toml_str = r#"
bot_token = "tok"
guild_id = "123"
"#;
        let parsed: DiscordConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.allowed_users.is_empty());
        assert_eq!(parsed.bot_token, "tok");
    }

    #[test]
    async fn slack_config_toml_backward_compat() {
        let toml_str = r#"
bot_token = "xoxb-tok"
channel_id = "C123"
"#;
        let parsed: SlackConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.allowed_users.is_empty());
        assert_eq!(parsed.channel_id.as_deref(), Some("C123"));
    }

    #[test]
    async fn webhook_config_with_secret() {
        let json = r#"{"port":8080,"secret":"my-secret-key"}"#;
        let parsed: WebhookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.secret.as_deref(), Some("my-secret-key"));
    }

    /// `port` was removed in schema v21 — nothing read it. A config still
    /// carrying it must load, not error, because operators have it written.
    #[test]
    async fn webhook_config_without_secret_ignores_the_removed_port_key() {
        let json = r#"{"port":8080}"#;
        let parsed: WebhookConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.secret.is_none());
    }

    // ── WhatsApp config ──────────────────────────────────────

    #[test]
    async fn whatsapp_config_serde() {
        let wc = WhatsAppConfig {
            access_token: Some("EAABx...".into()),
            phone_number_id: Some("123456789".into()),
            verify_token: Some("my-verify-token".into()),
            app_secret: None,
            allowed_numbers: vec!["+1234567890".into(), "+9876543210".into()],
        };
        let json = serde_json::to_string(&wc).unwrap();
        let parsed: WhatsAppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.access_token, Some("EAABx...".into()));
        assert_eq!(parsed.phone_number_id, Some("123456789".into()));
        assert_eq!(parsed.verify_token, Some("my-verify-token".into()));
        assert_eq!(parsed.allowed_numbers.len(), 2);
    }

    #[test]
    async fn whatsapp_config_toml_roundtrip() {
        let wc = WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("12345".into()),
            verify_token: Some("verify".into()),
            app_secret: Some("secret123".into()),
            allowed_numbers: vec!["+1".into()],
        };
        let toml_str = toml::to_string(&wc).unwrap();
        let parsed: WhatsAppConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.phone_number_id, Some("12345".into()));
        assert_eq!(parsed.allowed_numbers, vec!["+1"]);
    }

    #[test]
    async fn whatsapp_config_deserializes_without_allowed_numbers() {
        let json = r#"{"access_token":"tok","phone_number_id":"123","verify_token":"ver"}"#;
        let parsed: WhatsAppConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.allowed_numbers.is_empty());
    }

    #[test]
    async fn whatsapp_config_wildcard_allowed() {
        let wc = WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("123".into()),
            verify_token: Some("ver".into()),
            app_secret: None,
            allowed_numbers: vec!["*".into()],
        };
        let toml_str = toml::to_string(&wc).unwrap();
        let parsed: WhatsAppConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.allowed_numbers, vec!["*"]);
    }

    /// Replaces the two `backend_type` tests deleted in schema v32.
    ///
    /// The mode used to be inferred from which keys happened to be filled, and
    /// those tests pinned the guess: Cloud won when both were present, Web won
    /// when only `session_path` was. There is nothing left to infer. The table
    /// an operator writes is the declaration, so what is worth pinning now is
    /// that the two tables carry disjoint keys and neither can express the
    /// other's transport.
    #[test]
    async fn the_two_whatsapp_tables_are_disjoint_and_neither_infers_a_mode() {
        let cloud_toml = toml::to_string(&WhatsAppConfig {
            access_token: Some("tok".into()),
            phone_number_id: Some("123".into()),
            verify_token: Some("ver".into()),
            app_secret: None,
            allowed_numbers: vec!["+1".into()],
        })
        .expect("cloud serialises");
        for web_key in ["session_path", "pair_phone", "pair_code"] {
            assert!(
                !cloud_toml.contains(web_key),
                "the Cloud table must not carry `{web_key}`: {cloud_toml}"
            );
        }

        let web_toml = toml::to_string(&WhatsAppWebConfig {
            session_path: "~/.rantaiclaw/state/whatsapp-web/session.db".into(),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec![],
        })
        .expect("web serialises");
        for cloud_key in [
            "access_token",
            "phone_number_id",
            "verify_token",
            "app_secret",
        ] {
            assert!(
                !web_toml.contains(cloud_key),
                "the Web table must not carry `{cloud_key}`: {web_toml}"
            );
        }

        // `session_path` is required, so a Web table cannot be written empty.
        let empty: Result<WhatsAppWebConfig, _> = toml::from_str("allowed_numbers = []");
        assert!(
            empty.is_err(),
            "a Web table with no session_path must not deserialise"
        );
    }

    #[test]
    async fn channels_config_with_whatsapp() {
        let c = ChannelsConfig {
            cli: true,
            telegram: None,
            discord: None,
            slack: None,
            mattermost: None,
            webhook: None,
            imessage: None,
            matrix: None,
            signal: None,
            whatsapp: Some(WhatsAppConfig {
                access_token: Some("tok".into()),
                phone_number_id: Some("123".into()),
                verify_token: Some("ver".into()),
                app_secret: None,
                allowed_numbers: vec!["+1".into()],
            }),
            whatsapp_web: None,
            linq: None,
            nextcloud_talk: None,
            email: None,
            irc: None,
            lark: None,
            dingtalk: None,
            qq: None,
            message_timeout_secs: 300,
            autonomous_tools: false,
            thread_replies: true,
            approval_owners: Vec::new(),
            guest_allowed_tools: Vec::new(),
            guest_allowed_commands: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&c).unwrap();
        let parsed: ChannelsConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.whatsapp.is_some());
        let wa = parsed.whatsapp.unwrap();
        assert_eq!(wa.phone_number_id, Some("123".into()));
        assert_eq!(wa.allowed_numbers, vec!["+1"]);
    }

    #[test]
    async fn channels_config_default_has_no_whatsapp() {
        let c = ChannelsConfig::default();
        assert!(c.whatsapp.is_none());
    }

    #[test]
    async fn channels_config_default_has_no_nextcloud_talk() {
        let c = ChannelsConfig::default();
        assert!(c.nextcloud_talk.is_none());
    }

    #[test]
    async fn channels_config_autonomous_tools_defaults_off_and_parses() {
        // Default posture must stay safe: channel webhooks auto-deny tools.
        assert!(!ChannelsConfig::default().autonomous_tools);
        // Old configs without the key still deserialize (serde default).
        let legacy: ChannelsConfig = toml::from_str("cli = true\n").unwrap();
        assert!(!legacy.autonomous_tools);
        // Opt-in parses through.
        let opted: ChannelsConfig =
            toml::from_str("cli = true\nautonomous_tools = true\n").unwrap();
        assert!(opted.autonomous_tools);
    }

    // ══════════════════════════════════════════════════════════
    // SECURITY CHECKLIST TESTS — Gateway config
    // ══════════════════════════════════════════════════════════

    #[test]
    async fn checklist_gateway_default_requires_pairing() {
        let g = GatewayConfig::default();
        assert!(g.require_pairing, "Pairing must be required by default");
    }

    #[test]
    async fn checklist_gateway_default_blocks_public_bind() {
        let g = GatewayConfig::default();
        assert!(
            !g.allow_public_bind,
            "Public bind must be blocked by default"
        );
    }

    #[test]
    async fn checklist_gateway_default_no_tokens() {
        let g = GatewayConfig::default();
        assert!(
            g.paired_tokens.is_empty(),
            "No pre-paired tokens by default"
        );
        assert_eq!(g.pair_rate_limit_per_minute, 10);
        assert_eq!(g.webhook_rate_limit_per_minute, 60);
        assert!(!g.trust_forwarded_headers);
        assert_eq!(g.rate_limit_max_keys, 10_000);
        assert_eq!(g.idempotency_ttl_secs, 300);
        assert_eq!(g.idempotency_max_keys, 10_000);
    }

    #[test]
    async fn checklist_gateway_cli_default_host_is_localhost() {
        // The CLI default for --host is 127.0.0.1 (checked in main.rs)
        // Here we verify the config default matches
        let c = Config::default();
        assert!(
            c.gateway.require_pairing,
            "Config default must require pairing"
        );
        assert!(
            !c.gateway.allow_public_bind,
            "Config default must block public bind"
        );
    }

    #[test]
    async fn checklist_gateway_serde_roundtrip() {
        let g = GatewayConfig {
            port: 3000,
            host: "127.0.0.1".into(),
            require_pairing: true,
            allow_public_bind: false,
            paired_tokens: vec!["zc_test_token".into()],
            pair_rate_limit_per_minute: 12,
            webhook_rate_limit_per_minute: 80,
            api_rate_limit_per_minute: 900,
            trust_forwarded_headers: true,
            rate_limit_max_keys: 2048,
            idempotency_ttl_secs: 600,
            idempotency_max_keys: 4096,
            request_timeout_secs: 120,
            login: GatewayLoginConfig::default(),
        };
        let toml_str = toml::to_string(&g).unwrap();
        let parsed: GatewayConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.require_pairing);
        assert!(!parsed.allow_public_bind);
        assert_eq!(parsed.paired_tokens, vec!["zc_test_token"]);
        assert_eq!(parsed.pair_rate_limit_per_minute, 12);
        assert_eq!(parsed.webhook_rate_limit_per_minute, 80);
        assert!(parsed.trust_forwarded_headers);
        assert_eq!(parsed.rate_limit_max_keys, 2048);
        assert_eq!(parsed.idempotency_ttl_secs, 600);
        assert_eq!(parsed.idempotency_max_keys, 4096);
    }

    #[test]
    async fn checklist_gateway_backward_compat_no_gateway_section() {
        // Old configs without [gateway] should get secure defaults
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(
            parsed.gateway.require_pairing,
            "Missing [gateway] must default to require_pairing=true"
        );
        assert!(
            !parsed.gateway.allow_public_bind,
            "Missing [gateway] must default to allow_public_bind=false"
        );
    }

    #[test]
    async fn checklist_autonomy_default_is_workspace_scoped() {
        let a = AutonomyConfig::default();
        assert!(a.workspace_only, "Default autonomy must be workspace_only");
        assert!(
            a.forbidden_paths.contains(&"/etc".to_string()),
            "Must block /etc"
        );
        assert!(
            a.forbidden_paths.contains(&"/proc".to_string()),
            "Must block /proc"
        );
        assert!(
            a.forbidden_paths.contains(&"~/.ssh".to_string()),
            "Must block ~/.ssh"
        );
    }

    // ══════════════════════════════════════════════════════════
    // COMPOSIO CONFIG TESTS
    // ══════════════════════════════════════════════════════════

    #[test]
    async fn composio_config_default_disabled() {
        let c = ComposioConfig::default();
        assert!(!c.enabled, "Composio must be disabled by default");
        assert!(c.api_key.is_none(), "No API key by default");
        assert_eq!(c.entity_id, "default");
    }

    #[test]
    async fn composio_config_serde_roundtrip() {
        let c = ComposioConfig {
            enabled: true,
            api_key: Some("comp-key-123".into()),
            entity_id: "user42".into(),
        };
        let toml_str = toml::to_string(&c).unwrap();
        let parsed: ComposioConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.api_key.as_deref(), Some("comp-key-123"));
        assert_eq!(parsed.entity_id, "user42");
    }

    #[test]
    async fn composio_config_backward_compat_missing_section() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(
            !parsed.composio.enabled,
            "Missing [composio] must default to disabled"
        );
        assert!(parsed.composio.api_key.is_none());
    }

    #[test]
    async fn composio_config_partial_toml() {
        let toml_str = r"
enabled = true
";
        let parsed: ComposioConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.api_key.is_none());
        assert_eq!(parsed.entity_id, "default");
    }

    #[test]
    async fn composio_config_enable_alias_supported() {
        let toml_str = r"
enable = true
";
        let parsed: ComposioConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.api_key.is_none());
        assert_eq!(parsed.entity_id, "default");
    }

    // ══════════════════════════════════════════════════════════
    // SECRETS CONFIG TESTS
    // ══════════════════════════════════════════════════════════

    #[test]
    async fn secrets_config_default_encrypts() {
        let s = SecretsConfig::default();
        assert!(s.encrypt, "Encryption must be enabled by default");
    }

    #[test]
    async fn secrets_config_serde_roundtrip() {
        let s = SecretsConfig { encrypt: false };
        let toml_str = toml::to_string(&s).unwrap();
        let parsed: SecretsConfig = toml::from_str(&toml_str).unwrap();
        assert!(!parsed.encrypt);
    }

    #[test]
    async fn secrets_config_backward_compat_missing_section() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(
            parsed.secrets.encrypt,
            "Missing [secrets] must default to encrypt=true"
        );
    }

    #[test]
    async fn config_default_has_composio_and_secrets() {
        let c = Config::default();
        assert!(!c.composio.enabled);
        assert!(c.composio.api_key.is_none());
        assert!(c.secrets.encrypt);
        assert!(c.browser.enabled);
        assert!(c.browser.allowed_domains.is_empty());
    }

    #[test]
    async fn browser_config_default_enabled() {
        let b = BrowserConfig::default();
        assert!(b.enabled);
        assert!(b.allowed_domains.is_empty());
        assert_eq!(b.backend, "agent_browser");
        assert_eq!(b.computer_use.endpoint, "http://127.0.0.1:8787/v1/actions");
        assert_eq!(b.computer_use.timeout_ms, 15_000);
        assert!(!b.computer_use.allow_remote_endpoint);
        assert!(b.computer_use.window_allowlist.is_empty());
        assert!(b.computer_use.max_coordinate_x.is_none());
        assert!(b.computer_use.max_coordinate_y.is_none());
    }

    #[test]
    async fn browser_config_serde_roundtrip() {
        let b = BrowserConfig {
            enabled: true,
            allowed_domains: vec!["example.com".into(), "docs.example.com".into()],
            session_name: None,
            backend: "auto".into(),
            computer_use: BrowserComputerUseConfig {
                endpoint: "https://computer-use.example.com/v1/actions".into(),
                api_key: Some("test-token".into()),
                timeout_ms: 8_000,
                allow_remote_endpoint: true,
                window_allowlist: vec!["Chrome".into(), "Visual Studio Code".into()],
                max_coordinate_x: Some(3840),
                max_coordinate_y: Some(2160),
            },
        };
        let toml_str = toml::to_string(&b).unwrap();
        let parsed: BrowserConfig = toml::from_str(&toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.allowed_domains.len(), 2);
        assert_eq!(parsed.allowed_domains[0], "example.com");
        assert_eq!(parsed.backend, "auto");
        assert_eq!(
            parsed.computer_use.endpoint,
            "https://computer-use.example.com/v1/actions"
        );
        assert_eq!(parsed.computer_use.api_key.as_deref(), Some("test-token"));
        assert_eq!(parsed.computer_use.timeout_ms, 8_000);
        assert!(parsed.computer_use.allow_remote_endpoint);
        assert_eq!(parsed.computer_use.window_allowlist.len(), 2);
        assert_eq!(parsed.computer_use.max_coordinate_x, Some(3840));
        assert_eq!(parsed.computer_use.max_coordinate_y, Some(2160));
    }

    #[test]
    async fn browser_config_backward_compat_missing_section() {
        let minimal = r#"
workspace_dir = "/tmp/ws"
config_path = "/tmp/config.toml"
default_temperature = 0.7
"#;
        let parsed: Config = toml::from_str(minimal).unwrap();
        assert!(parsed.browser.enabled);
        assert!(parsed.browser.allowed_domains.is_empty());
    }

    #[test]
    async fn partial_section_uses_impl_default_not_stale_serde() {
        // A section PRESENT but omitting a field must fill it from the intended
        // default (impl Default), not a stale serde default. Before the F2 fix,
        // `[http_request]` with no `allowed_domains` loaded `[]` (which rejects
        // every request) and `[browser]`/`[web_search]` loaded disabled. Also
        // covers F3: a partial `[autonomy]` must load, not fail with "missing
        // field".
        let toml_str = r#"
default_temperature = 0.7
[http_request]
timeout_secs = 25
[browser]
backend = "auto"
[web_search]
provider = "brave"
[autonomy]
level = "full"
"#;
        let cfg: Config = toml::from_str(toml_str).expect("partial sections must load");

        // http_request: present, omits enabled + allowed_domains → intended defaults.
        assert!(
            cfg.http_request.enabled,
            "omitted enabled must default true"
        );
        assert_eq!(
            cfg.http_request.allowed_domains,
            vec!["*".to_string()],
            "omitted allowed_domains must default to the wildcard, not []"
        );
        assert_eq!(cfg.http_request.timeout_secs, 25, "explicit value is kept");
        assert!(cfg.browser.enabled);
        assert!(cfg.web_search.enabled);

        // A partial [autonomy] loads and fills the rest from Default.
        assert_eq!(cfg.autonomy.level, crate::security::AutonomyLevel::Full);
        assert!(!cfg.autonomy.block_high_risk_commands);
        assert!(cfg.autonomy.workspace_only);
    }

    // ── Environment variable overrides (Docker support) ─────────

    async fn env_override_lock() -> MutexGuard<'static, ()> {
        // Shared with every other config-resolution-env test in the crate;
        // per-module locks don't serialize across modules. See `crate::test_env`.
        crate::test_env::ENV_LOCK.lock().await
    }

    fn clear_proxy_env_test_vars() {
        for key in [
            "RANTAICLAW_PROXY_ENABLED",
            "RANTAICLAW_HTTP_PROXY",
            "RANTAICLAW_HTTPS_PROXY",
            "RANTAICLAW_ALL_PROXY",
            "RANTAICLAW_NO_PROXY",
            "RANTAICLAW_PROXY_SCOPE",
            "RANTAICLAW_PROXY_SERVICES",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            std::env::remove_var(key);
        }
    }

    // ── Env overrides must not be persisted by save() ───────────

    /// `load_or_init` applies env overrides onto the struct `save()`
    /// serialises, so the first console/TUI/setup write used to bake whatever
    /// the environment held into `config.toml` permanently. For a container
    /// started with `RANTAICLAW_ALLOW_PUBLIC_BIND=true`, an exposure setting
    /// meant to last one run outlived its cause.
    #[test]
    async fn save_does_not_persist_an_env_set_exposure_flag() {
        let _env_guard = env_override_lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = Config::default();
        config.config_path = config_path.clone();
        // An unrelated value the operator did set, to prove the rest of the
        // file survives the rewrite intact.
        config.default_model = Some("anthropic/claude-sonnet-4.6".into());
        assert!(!config.gateway.allow_public_bind, "off by default");

        let _bind = crate::test_env::EnvGuard::set("RANTAICLAW_ALLOW_PUBLIC_BIND", "true");
        config.apply_env_overrides();
        assert!(
            config.gateway.allow_public_bind,
            "the running process must still see the override"
        );

        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        let persisted: Config = toml::from_str(&contents).unwrap();
        assert!(
            !persisted.gateway.allow_public_bind,
            "an exposure flag set for one run must not outlive it in config.toml"
        );
        assert_eq!(
            persisted.default_model.as_deref(),
            Some("anthropic/claude-sonnet-4.6"),
            "unrelated values must survive"
        );
    }

    /// Same defect, credential class: `RANTAICLAW_API_KEY` became a stored
    /// secret on the first save.
    #[test]
    async fn save_does_not_persist_an_env_supplied_credential() {
        let _env_guard = env_override_lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = Config::default();
        config.config_path = config_path.clone();
        assert!(config.api_key.is_none());

        let _key = crate::test_env::EnvGuard::set("RANTAICLAW_API_KEY", "sk-from-the-environment");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("sk-from-the-environment"));

        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        assert!(
            !contents.contains("sk-from-the-environment"),
            "an env-supplied credential must not be written to config.toml"
        );
        let persisted: Config = toml::from_str(&contents).unwrap();
        assert!(persisted.api_key.is_none());
    }

    /// The other half of the contract: not persisting the *environment's*
    /// value must not mean discarding the *operator's*. A container sets
    /// `RANTAICLAW_MODEL`, the operator then picks a different model in the
    /// TUI — their choice has to reach the file.
    #[test]
    async fn save_persists_a_value_changed_after_the_env_override() {
        let _env_guard = env_override_lock().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mut config = Config::default();
        config.config_path = config_path.clone();

        let _model = crate::test_env::EnvGuard::set("RANTAICLAW_MODEL", "env/model");
        config.apply_env_overrides();
        assert_eq!(config.default_model.as_deref(), Some("env/model"));

        // What a TUI model switch does.
        config.default_model = Some("operator/model".into());
        config.save().await.unwrap();

        let contents = tokio::fs::read_to_string(&config_path).await.unwrap();
        let persisted: Config = toml::from_str(&contents).unwrap();
        assert_eq!(
            persisted.default_model.as_deref(),
            Some("operator/model"),
            "a deliberate change after the override must be persisted"
        );
    }

    #[test]
    async fn env_override_api_key() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert!(config.api_key.is_none());

        let _g_api_key = crate::test_env::EnvGuard::set("RANTAICLAW_API_KEY", "sk-test-env-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("sk-test-env-key"));
    }

    #[test]
    async fn env_override_api_key_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_rc_api_key = crate::test_env::EnvGuard::unset("RANTAICLAW_API_KEY");
        let _g_api_key = crate::test_env::EnvGuard::set("API_KEY", "sk-fallback-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("sk-fallback-key"));
    }

    #[test]
    async fn env_override_provider() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_provider = crate::test_env::EnvGuard::set("RANTAICLAW_PROVIDER", "anthropic");
        config.apply_env_overrides();
        assert_eq!(config.default_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    async fn web_search_enabled_yes_is_true() {
        let _env_guard = env_override_lock().await;
        let _g_rc_web_search = crate::test_env::EnvGuard::unset("RANTAICLAW_WEB_SEARCH_ENABLED");
        let _g_web_search = crate::test_env::EnvGuard::set("WEB_SEARCH_ENABLED", "yes");
        let mut config = Config::default();
        config.web_search.enabled = false;
        config.apply_env_overrides();
        assert!(
            config.web_search.enabled,
            "`WEB_SEARCH_ENABLED=yes` must enable, not disable"
        );
    }

    #[test]
    async fn invalid_port_keeps_config_value() {
        let _env_guard = env_override_lock().await;
        let _g_rc_gateway_port = crate::test_env::EnvGuard::unset("RANTAICLAW_GATEWAY_PORT");
        let _g_port = crate::test_env::EnvGuard::set("PORT", "not-a-number");
        let mut config = Config::default();
        config.gateway.port = 8787;
        config.apply_env_overrides();
        assert_eq!(
            config.gateway.port, 8787,
            "an invalid PORT must be ignored, not silently discarded to 0"
        );
    }

    #[test]
    async fn disabled_proxy_stays_disabled_across_reload() {
        let _env_guard = env_override_lock().await;
        clear_proxy_env_test_vars();
        ProxyConfig::clear_authored_process_env();

        // 1. An enabled proxy applies its URL to the process env (authoring
        //    HTTP_PROXY) so requests are proxied.
        let mut config = Config::default();
        config.proxy.enabled = true;
        config.proxy.scope = ProxyScope::Environment;
        config.proxy.http_proxy = Some("http://proxy.local:8080".into());
        config.apply_env_overrides();
        assert!(config.proxy.enabled);
        assert!(
            std::env::var("HTTP_PROXY").is_ok(),
            "an enabled proxy writes HTTP_PROXY"
        );

        // 2. The operator disables it. A reload must NOT read back the HTTP_PROXY
        //    we wrote and flip `enabled` to true, and must clear our own var.
        config.proxy.enabled = false;
        config.apply_env_overrides();
        assert!(
            !config.proxy.enabled,
            "a disabled proxy must not resurrect itself on reload"
        );
        assert!(
            std::env::var("HTTP_PROXY").is_err(),
            "the proxy env we authored is cleared on disable"
        );

        clear_proxy_env_test_vars();
        ProxyConfig::clear_authored_process_env();
    }

    #[test]
    async fn config_dir_wins_over_workspace_split() {
        let _env_guard = env_override_lock().await;
        let _g_config_dir = crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", "/custom/cfg");
        let _g_workspace =
            crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", "/other/workspace");
        let mut config = Config::default();
        // Simulates what `resolve_runtime_config_dirs` derives under CONFIG_DIR.
        config.workspace_dir = PathBuf::from("/custom/cfg/workspace");
        config.apply_env_overrides();
        assert_eq!(
            config.workspace_dir,
            PathBuf::from("/custom/cfg/workspace"),
            "RANTAICLAW_WORKSPACE must not override workspace_dir when CONFIG_DIR is set"
        );
    }

    #[test]
    async fn kb_env_key_enables_kb() {
        let _env_guard = env_override_lock().await;
        let _g_kb_key =
            crate::test_env::EnvGuard::set("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key");
        let mut config = Config::default();
        config.knowledge.enabled = false;
        config.apply_env_overrides();
        // Supplying the embedding key via env is an opt-in to the KB — this is
        // where the enable evidence lives now that migrate_v18 is pure.
        assert!(
            config.knowledge.enabled,
            "a non-empty KB_EMBEDDING_API_KEY must enable the KB"
        );
        assert_eq!(
            config.knowledge.embedding_api_key.as_deref(),
            Some("rantaiclaw_test_key")
        );
    }

    #[test]
    async fn env_override_open_skills_enabled_and_dir() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert!(!config.skills.open_skills_enabled);
        assert!(config.skills.open_skills_dir.is_none());
        assert_eq!(
            config.skills.prompt_injection_mode,
            SkillsPromptInjectionMode::Full
        );

        let _g_skills_enabled =
            crate::test_env::EnvGuard::set("RANTAICLAW_OPEN_SKILLS_ENABLED", "true");
        let _g_skills_dir =
            crate::test_env::EnvGuard::set("RANTAICLAW_OPEN_SKILLS_DIR", "/tmp/open-skills");
        let _g_skills_mode =
            crate::test_env::EnvGuard::set("RANTAICLAW_SKILLS_PROMPT_MODE", "compact");
        config.apply_env_overrides();

        assert!(config.skills.open_skills_enabled);
        assert_eq!(
            config.skills.open_skills_dir.as_deref(),
            Some("/tmp/open-skills")
        );
        assert_eq!(
            config.skills.prompt_injection_mode,
            SkillsPromptInjectionMode::Compact
        );
    }

    #[test]
    async fn env_override_open_skills_enabled_invalid_value_keeps_existing_value() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        config.skills.open_skills_enabled = true;
        config.skills.prompt_injection_mode = SkillsPromptInjectionMode::Compact;

        let _g_skills_enabled =
            crate::test_env::EnvGuard::set("RANTAICLAW_OPEN_SKILLS_ENABLED", "maybe");
        let _g_skills_mode =
            crate::test_env::EnvGuard::set("RANTAICLAW_SKILLS_PROMPT_MODE", "invalid");
        config.apply_env_overrides();

        assert!(config.skills.open_skills_enabled);
        assert_eq!(
            config.skills.prompt_injection_mode,
            SkillsPromptInjectionMode::Compact
        );
    }

    #[test]
    async fn env_override_provider_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_rc_provider = crate::test_env::EnvGuard::unset("RANTAICLAW_PROVIDER");
        let _g_provider = crate::test_env::EnvGuard::set("PROVIDER", "openai");
        config.apply_env_overrides();
        assert_eq!(config.default_provider.as_deref(), Some("openai"));
    }

    #[test]
    async fn env_override_provider_fallback_does_not_replace_non_default_provider() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("custom:https://proxy.example.com/v1".to_string()),
            ..Config::default()
        };

        let _g_rc_provider = crate::test_env::EnvGuard::unset("RANTAICLAW_PROVIDER");
        let _g_provider = crate::test_env::EnvGuard::set("PROVIDER", "openrouter");
        config.apply_env_overrides();
        assert_eq!(
            config.default_provider.as_deref(),
            Some("custom:https://proxy.example.com/v1")
        );
    }

    #[test]
    async fn env_override_provider_overrides_non_default_provider() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("custom:https://proxy.example.com/v1".to_string()),
            ..Config::default()
        };

        let _g_rc_provider = crate::test_env::EnvGuard::set("RANTAICLAW_PROVIDER", "openrouter");
        let _g_provider = crate::test_env::EnvGuard::set("PROVIDER", "anthropic");
        config.apply_env_overrides();
        assert_eq!(config.default_provider.as_deref(), Some("openrouter"));
    }

    #[test]
    async fn env_override_glm_api_key_for_regional_aliases() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("glm-cn".to_string()),
            ..Config::default()
        };

        let _g_glm_key = crate::test_env::EnvGuard::set("GLM_API_KEY", "glm-regional-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("glm-regional-key"));
    }

    #[test]
    async fn env_override_zai_api_key_for_regional_aliases() {
        let _env_guard = env_override_lock().await;
        let mut config = Config {
            default_provider: Some("zai-cn".to_string()),
            ..Config::default()
        };

        let _g_zai_key = crate::test_env::EnvGuard::set("ZAI_API_KEY", "zai-regional-key");
        config.apply_env_overrides();
        assert_eq!(config.api_key.as_deref(), Some("zai-regional-key"));
    }

    #[test]
    async fn env_override_model() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_model = crate::test_env::EnvGuard::set("RANTAICLAW_MODEL", "gpt-4o");
        config.apply_env_overrides();
        assert_eq!(config.default_model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    async fn validate_ollama_cloud_model_requires_remote_api_url() {
        let _env_guard = env_override_lock().await;
        let config = Config {
            default_provider: Some("ollama".to_string()),
            default_model: Some("glm-5:cloud".to_string()),
            api_url: None,
            api_key: Some("ollama-key".to_string()),
            ..Config::default()
        };

        let error = config.validate().expect_err("expected validation to fail");
        assert!(error.to_string().contains(
            "default_model uses ':cloud' with provider 'ollama', but api_url is local or unset"
        ));
    }

    #[test]
    async fn validate_rejects_password_hash_without_username() {
        // The gate keys "enabled" on password_hash alone, but the comparison
        // needs a username — so a hash without one is an unwinnable prompt.
        let mut config = Config::default();
        config.gateway.login.password_hash = Some("$argon2id$v=19$m=1,t=1,p=1$a$b".into());
        let error = config.validate().expect_err("expected validation to fail");
        assert!(
            error.to_string().contains("username is empty"),
            "unexpected error: {error}"
        );
    }

    #[test]
    async fn validate_rejects_username_without_password_hash() {
        let mut config = Config::default();
        config.gateway.login.username = Some("rantaiclaw_operator".into());
        let error = config.validate().expect_err("expected validation to fail");
        assert!(
            error.to_string().contains("password_hash is empty"),
            "unexpected error: {error}"
        );
    }

    #[test]
    async fn validate_rejects_out_of_range_temperature() {
        let mut config = Config::default();
        config.default_temperature = 99.0;
        let error = config.validate().expect_err("expected validation to fail");
        assert!(
            error.to_string().contains("default_temperature"),
            "unexpected error: {error}"
        );
    }

    #[test]
    async fn validate_accepts_default_config() {
        // The default config must pass validate(), or persist_and_swap would
        // reject every normal console write.
        Config::default()
            .validate()
            .expect("default config should be valid");
    }

    #[test]
    async fn validate_accepts_both_set_or_neither() {
        // Default (gate off) and a fully configured gate both pass.
        assert!(Config::default().validate().is_ok());

        let mut config = Config::default();
        config.gateway.login.username = Some("rantaiclaw_operator".into());
        config.gateway.login.password_hash = Some("$argon2id$v=19$m=1,t=1,p=1$a$b".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    async fn validate_treats_whitespace_only_login_fields_as_unset() {
        // A blank string is not a credential; it must not look like one.
        let mut config = Config::default();
        config.gateway.login.username = Some("   ".into());
        config.gateway.login.password_hash = Some("$argon2id$v=19$m=1,t=1,p=1$a$b".into());
        assert!(config.validate().is_err());
    }

    #[test]
    async fn validate_ollama_cloud_model_accepts_remote_endpoint_and_env_key() {
        let _env_guard = env_override_lock().await;
        let config = Config {
            default_provider: Some("ollama".to_string()),
            default_model: Some("glm-5:cloud".to_string()),
            api_url: Some("https://ollama.com/api".to_string()),
            api_key: None,
            ..Config::default()
        };

        let _g_ollama_key = crate::test_env::EnvGuard::set("OLLAMA_API_KEY", "ollama-env-key");
        let result = config.validate();

        assert!(result.is_ok(), "expected validation to pass: {result:?}");
    }

    #[test]
    async fn env_override_model_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_rc_model = crate::test_env::EnvGuard::unset("RANTAICLAW_MODEL");
        let _g_model = crate::test_env::EnvGuard::set("MODEL", "anthropic/claude-3.5-sonnet");
        config.apply_env_overrides();
        assert_eq!(
            config.default_model.as_deref(),
            Some("anthropic/claude-3.5-sonnet")
        );
    }

    #[test]
    async fn env_override_workspace() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        // `apply_env_overrides` honours CONFIG_DIR precedence over WORKSPACE
        // (`apply_env_overrides_inner`), so unset the former too — otherwise a
        // test runner with RANTAICLAW_CONFIG_DIR exported would shadow the
        // override under test.
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace =
            crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", "/custom/workspace");
        config.apply_env_overrides();
        assert_eq!(config.workspace_dir, PathBuf::from("/custom/workspace"));
    }

    #[test]
    async fn resolve_runtime_config_dirs_uses_env_workspace_first() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");
        let workspace_dir = default_config_dir.join("profile-a");

        // Unset so a parent-shell `RANTAICLAW_CONFIG_DIR` does not shadow the
        // WORKSPACE branch under test (`resolve_runtime_config_dirs` checks
        // CONFIG_DIR first). Mirrors the PR #904 lark pairing pattern.
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);
        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::EnvWorkspace);
        assert_eq!(config_dir, workspace_dir);
        assert_eq!(resolved_workspace_dir, workspace_dir.join("workspace"));

        let _ = fs::remove_dir_all(default_config_dir).await;
    }

    #[test]
    async fn resolve_runtime_config_dirs_uses_env_config_dir_first() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");
        let explicit_config_dir = default_config_dir.join("explicit-config");
        let marker_config_dir = default_config_dir.join("profiles").join("alpha");
        let state_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

        fs::create_dir_all(&default_config_dir).await.unwrap();
        let state = ActiveWorkspaceState {
            config_dir: marker_config_dir.to_string_lossy().into_owned(),
        };
        fs::write(&state_path, toml::to_string(&state).unwrap())
            .await
            .unwrap();

        let _g_config_dir =
            crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", &explicit_config_dir);
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::EnvConfigDir);
        assert_eq!(config_dir, explicit_config_dir);
        assert_eq!(
            resolved_workspace_dir,
            explicit_config_dir.join("workspace")
        );

        let _ = fs::remove_dir_all(default_config_dir).await;
    }

    #[test]
    async fn resolve_runtime_config_dirs_uses_active_workspace_marker() {
        let _env_guard = env_override_lock().await;
        let default_config_dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let default_workspace_dir = default_config_dir.join("workspace");
        let marker_config_dir = default_config_dir.join("profiles").join("alpha");
        let state_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

        // Pin HOME to a tempdir before opting out of the isolation guard: if
        // the guard were ever removed and this test's marker lookup fell
        // through to HOME-derived resolution, it would land in this tempdir
        // instead of the developer's real ~/.rantaiclaw.
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_home).await.unwrap();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);

        // Marker-driven resolution is the first non-env branch. The test is
        // the one place we explicitly exercise the default-resolution path
        // without setting CONFIG_DIR or WORKSPACE, so opt out of the test
        // isolation guard for this one call.
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        fs::create_dir_all(&default_config_dir).await.unwrap();
        let state = ActiveWorkspaceState {
            config_dir: marker_config_dir.to_string_lossy().into_owned(),
        };
        fs::write(&state_path, toml::to_string(&state).unwrap())
            .await
            .unwrap();

        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::ActiveWorkspaceMarker);
        assert_eq!(config_dir, marker_config_dir);
        assert_eq!(resolved_workspace_dir, marker_config_dir.join("workspace"));

        let _ = fs::remove_dir_all(default_config_dir).await;
        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn active_workspace_marker_temp_leak_is_detected_only_for_real_installs() {
        let temp = std::env::temp_dir();
        // Real install whose marker points into the OS temp dir → leaked
        // ephemeral/test workspace; must be ignored so it can't shadow config.
        assert!(active_workspace_marker_is_temp_leak(
            &temp.join(".tmpABC").join(".rantaiclaw"),
            Path::new("/home/rantaiclaw_user/.rantaiclaw"),
            &temp,
        ));
        // Test harness: the default dir is ALSO under temp → legitimate, honored.
        let test_default = temp.join("rantaiclaw_test_root");
        assert!(!active_workspace_marker_is_temp_leak(
            &test_default.join("profiles").join("alpha"),
            &test_default,
            &temp,
        ));
        // Ordinary install (nothing under temp) → not a leak.
        assert!(!active_workspace_marker_is_temp_leak(
            Path::new("/home/rantaiclaw_user/.rantaiclaw/profiles/alpha"),
            Path::new("/home/rantaiclaw_user/.rantaiclaw"),
            &temp,
        ));
    }

    #[test]
    async fn resolve_runtime_config_dirs_falls_back_to_default_layout() {
        let _env_guard = env_override_lock().await;
        // Pin HOME so the v0.5.0 profile-aware fallback resolves into a
        // tempdir we control instead of the real ~/.rantaiclaw.
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_home).await.unwrap();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");

        let default_config_dir = temp_home.join(".rantaiclaw");
        let default_workspace_dir = default_config_dir.join("workspace");

        // This test is the canonical exercise of the default-resolution
        // branch — opt out of the test isolation guard for this one call.
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let (config_dir, resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default_config_dir, &default_workspace_dir)
                .await
                .unwrap();

        // Source is still `DefaultConfigDir`, but the dirs now point at the
        // active profile under `<HOME>/.rantaiclaw/profiles/default/`.
        assert_eq!(source, ConfigResolutionSource::DefaultConfigDir);
        assert_eq!(config_dir, temp_home.join(".rantaiclaw/profiles/default"));
        assert_eq!(
            resolved_workspace_dir,
            temp_home.join(".rantaiclaw/profiles/default/workspace")
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    // ── Cross-root active-workspace marker ─────────────────────────
    //
    // A `~/.rantaiclaw` tree that has been copied, moved or restored to a
    // different path still carries its `active_workspace.toml` marker. The
    // marker names `<original>/profiles/<name>` — an absolute path that now
    // points at a different root. Honoring it silently would let the run
    // follow the marker back to the original location and migrate a config
    // the operator never asked to touch. When the local root has its own
    // `config.toml` for that profile, the local profile wins; the marker is
    // set aside with a single warning per process.

    /// Pure unit test: the guard fires when the marker names another root's
    /// profile and the local root has its own config for that profile.
    /// (The `cross_root_guard_*` siblings need `TempDir` because the guard
    /// reads `config.toml` from the local root.)
    #[tokio::test]
    async fn cross_root_guard_fires_when_marker_targets_another_root_with_local_config() {
        let local_root = tempfile::TempDir::new().unwrap();
        let default = local_root.path().join(".rantaiclaw");
        fs::create_dir_all(default.join("profiles").join("default"))
            .await
            .unwrap();
        fs::write(default.join("profiles/default/config.toml"), "")
            .await
            .unwrap();
        let other_root = tempfile::TempDir::new().unwrap();
        let marker_dir = other_root.path().join(".rantaiclaw/profiles/default");

        let fired = active_workspace_marker_names_another_roots_profile(&default, &marker_dir);
        assert_eq!(
            fired,
            Some(default.join("profiles").join("default")),
            "marker pointing at another root's profile must be set aside when the local root has its own config"
        );
    }

    /// Pure unit test: the guard does NOT fire when the marker points inside
    /// the local root (case (a) — a relative marker path or an absolute one
    /// under the default config dir).
    #[tokio::test]
    async fn cross_root_guard_does_not_fire_when_marker_is_inside_its_own_root() {
        let local_root = tempfile::TempDir::new().unwrap();
        let default = local_root.path().join(".rantaiclaw");
        fs::create_dir_all(default.join("profiles/default"))
            .await
            .unwrap();
        fs::write(default.join("profiles/default/config.toml"), "")
            .await
            .unwrap();
        let marker_dir = default.join("profiles/default");

        let fired = active_workspace_marker_names_another_roots_profile(&default, &marker_dir);
        assert_eq!(
            fired, None,
            "a marker pointing inside its own root must be honored as today"
        );
    }

    /// Pure unit test: the guard does NOT fire when the marker names a
    /// custom directory that is NOT shaped like `<some root>/profiles/<name>`
    /// (case (b) — a workspace elsewhere on disk that the operator chose
    /// explicitly).
    #[tokio::test]
    async fn cross_root_guard_does_not_fire_when_marker_targets_a_custom_directory() {
        let local_root = tempfile::TempDir::new().unwrap();
        let default = local_root.path().join(".rantaiclaw");
        let marker_dir = Path::new("/srv/workspaces/custom-thing");

        let fired = active_workspace_marker_names_another_roots_profile(&default, marker_dir);
        assert_eq!(
            fired, None,
            "a marker naming a custom directory must be honored as today"
        );
    }

    /// Pure unit test: the guard does NOT fire when the marker names another
    /// root's profile but the local root has NO config of its own for that
    /// profile (case (c) — the marker is the only way to reach that profile,
    /// so it must still be honored).
    #[tokio::test]
    async fn cross_root_guard_does_not_fire_when_marker_targets_another_root_without_local_config()
    {
        let local_root = tempfile::TempDir::new().unwrap();
        let default = local_root.path().join(".rantaiclaw");
        // Deliberately do NOT create `default/profiles/default/config.toml`.
        let other_root = tempfile::TempDir::new().unwrap();
        let marker_dir = other_root.path().join(".rantaiclaw/profiles/default");

        let fired = active_workspace_marker_names_another_roots_profile(&default, &marker_dir);
        assert_eq!(
            fired, None,
            "a marker naming another root's profile must be honored when the local root has no config for it"
        );
    }

    /// End-to-end: copy `A` (with its marker pointing at `A/profiles/default`)
    /// to `B`. Resolving with `B` as the default config root must pick the
    /// local profile under `B`, not the original location.
    #[tokio::test]
    async fn copied_tree_resolves_inside_itself() {
        let _env_guard = env_override_lock().await;
        let a_home = tempfile::TempDir::new().unwrap();
        let b_home = tempfile::TempDir::new().unwrap();
        let a_root = a_home.path().join(".rantaiclaw");
        let b_root = b_home.path().join(".rantaiclaw");

        // Build root A with `profiles/default/config.toml` and a marker
        // naming A/profiles/default.
        fs::create_dir_all(a_root.join("profiles/default"))
            .await
            .unwrap();
        fs::write(
            a_root.join("profiles/default/config.toml"),
            "default_model = \"a-cfg\"\n",
        )
        .await
        .unwrap();
        let marker_state = ActiveWorkspaceState {
            config_dir: a_root
                .join("profiles/default")
                .to_string_lossy()
                .into_owned(),
        };
        fs::write(
            a_root.join(ACTIVE_WORKSPACE_STATE_FILE),
            toml::to_string(&marker_state).unwrap(),
        )
        .await
        .unwrap();

        // Recursive copy of A onto B (the copy preserves the marker; that is
        // the whole point).
        copy_tree(&a_root, &b_root).await.unwrap();

        // Now resolve with B as the default config root, HOME pinned to
        // B's home so the profile-aware fallback lands inside B.
        let _g_home = crate::test_env::EnvGuard::set("HOME", b_home.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let (config_dir, _resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&b_root, &b_root.join("workspace"))
                .await
                .unwrap();

        assert_eq!(
            source,
            ConfigResolutionSource::DefaultConfigDir,
            "marker must be set aside, not honored"
        );
        assert_eq!(
            config_dir,
            b_root.join("profiles/default"),
            "the local profile under B must win"
        );
    }

    /// End-to-end: same as the copy test, but `A` is deleted after the copy,
    /// simulating a moved tree. Resolving with `B` must still pick the local
    /// profile under `B`, and the deletion of `A` must not affect resolution
    /// because the marker is set aside before any I/O against `A`.
    #[tokio::test]
    async fn moved_tree_resolves_inside_itself() {
        let _env_guard = env_override_lock().await;
        let a_home = tempfile::TempDir::new().unwrap();
        let b_home = tempfile::TempDir::new().unwrap();
        let a_root = a_home.path().join(".rantaiclaw");
        let b_root = b_home.path().join(".rantaiclaw");

        fs::create_dir_all(a_root.join("profiles/default"))
            .await
            .unwrap();
        fs::write(
            a_root.join("profiles/default/config.toml"),
            "default_model = \"a-cfg\"\n",
        )
        .await
        .unwrap();
        let marker_state = ActiveWorkspaceState {
            config_dir: a_root
                .join("profiles/default")
                .to_string_lossy()
                .into_owned(),
        };
        fs::write(
            a_root.join(ACTIVE_WORKSPACE_STATE_FILE),
            toml::to_string(&marker_state).unwrap(),
        )
        .await
        .unwrap();

        copy_tree(&a_root, &b_root).await.unwrap();

        // Delete A (a moved tree).
        fs::remove_dir_all(&a_root).await.unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", b_home.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let (config_dir, _resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&b_root, &b_root.join("workspace"))
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::DefaultConfigDir);
        assert_eq!(config_dir, b_root.join("profiles/default"));
    }

    /// Regression: a marker that names a directory inside its own root
    /// (case (a)) must still be honored. The local `profiles/default` is
    /// where the marker points and where the config lives.
    #[tokio::test]
    async fn marker_pointing_inside_its_own_root_is_honored() {
        let _env_guard = env_override_lock().await;
        let temp_home = tempfile::TempDir::new().unwrap();
        let default = temp_home.path().join(".rantaiclaw");
        let marker_dir = default.join("profiles/default");
        fs::create_dir_all(&marker_dir).await.unwrap();
        fs::write(marker_dir.join("config.toml"), "").await.unwrap();
        let marker_state = ActiveWorkspaceState {
            config_dir: marker_dir.to_string_lossy().into_owned(),
        };
        fs::write(
            default.join(ACTIVE_WORKSPACE_STATE_FILE),
            toml::to_string(&marker_state).unwrap(),
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", temp_home.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let (config_dir, _resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default, &default.join("workspace"))
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::ActiveWorkspaceMarker);
        assert_eq!(config_dir, marker_dir);
    }

    /// Regression: a marker that names a custom, non-`profiles/<name>`
    /// directory (case (b) — a workspace elsewhere on disk) must still be
    /// honored.
    #[tokio::test]
    async fn marker_naming_a_custom_directory_is_honored() {
        let _env_guard = env_override_lock().await;
        let temp_home = tempfile::TempDir::new().unwrap();
        let default = temp_home.path().join(".rantaiclaw");
        fs::create_dir_all(&default).await.unwrap();
        let custom = temp_home.path().join("workspaces/custom-thing");
        fs::create_dir_all(&custom).await.unwrap();
        fs::write(custom.join("config.toml"), "").await.unwrap();
        let marker_state = ActiveWorkspaceState {
            config_dir: custom.to_string_lossy().into_owned(),
        };
        fs::write(
            default.join(ACTIVE_WORKSPACE_STATE_FILE),
            toml::to_string(&marker_state).unwrap(),
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", temp_home.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let (config_dir, _resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default, &default.join("workspace"))
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::ActiveWorkspaceMarker);
        assert_eq!(config_dir, custom);
    }

    /// Regression: a marker naming another root's profile when the local
    /// root has NO config for that profile (case (c)) must still be
    /// honored — the marker is the only way to reach that profile.
    #[tokio::test]
    async fn marker_naming_another_roots_profile_without_local_config_is_honored() {
        let _env_guard = env_override_lock().await;
        let home = tempfile::TempDir::new().unwrap();
        let default = home.path().join(".rantaiclaw");
        fs::create_dir_all(&default).await.unwrap();
        // No `default/profiles/default/config.toml` exists in the local root.
        let marker_dir = home.path().join("other-home/.rantaiclaw/profiles/default");
        fs::create_dir_all(&marker_dir).await.unwrap();
        fs::write(marker_dir.join("config.toml"), "").await.unwrap();
        let marker_state = ActiveWorkspaceState {
            config_dir: marker_dir.to_string_lossy().into_owned(),
        };
        fs::write(
            default.join(ACTIVE_WORKSPACE_STATE_FILE),
            toml::to_string(&marker_state).unwrap(),
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", home.path());
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let (config_dir, _resolved_workspace_dir, source) =
            resolve_runtime_config_dirs(&default, &default.join("workspace"))
                .await
                .unwrap();

        assert_eq!(source, ConfigResolutionSource::ActiveWorkspaceMarker);
        assert_eq!(config_dir, marker_dir);
    }

    /// Recursive copy helper for the cross-root tests. The snapshot-restore
    /// variant of the cross-root case lives in
    /// `tests/cross_root_marker.rs`; it needs `lifecycle::update_snapshot`,
    /// which this lib-internal test module cannot reach from both
    /// `cargo test --lib` and the bin's test build.
    async fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dst).await?;
        let mut entries = fs::read_dir(src).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            let target = dst.join(entry.file_name());
            if ft.is_dir() {
                Box::pin(copy_tree(&entry.path(), &target)).await?;
            } else if ft.is_file() {
                fs::copy(entry.path(), &target).await?;
            }
        }
        Ok(())
    }

    /// v27 → v28. MCP server env values were the one credential path that
    /// bypassed the encryption authority, so every config written before this
    /// bump holds those tokens in plaintext. The upgrade must take them off
    /// disk, not merely encrypt future writes.
    /// `encrypt_config_secrets_in_raw` mirrors `decrypt_config_secrets`'s field
    /// list on raw TOML. Two hand-maintained lists drift — that is exactly how
    /// the KB keys and `provider_api_keys` were lost from the decrypt side
    /// before it became the single authority. This test binds them: it encrypts
    /// through the raw list and decrypts through the typed one, so a field
    /// present in the typed list but missing from the raw list shows up as a
    /// plaintext value still sitting in the serialised config.
    #[tokio::test]
    async fn raw_and_typed_secret_lists_cover_the_same_fields() {
        let dir = std::env::temp_dir().join(format!(
            "rantaiclaw_test_raw_secret_lists_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).await.unwrap();

        // One distinctive plaintext per governed field.
        let mut config = Config::default();
        config.secrets.encrypt = true;
        config.api_key = Some("plain-api-key".into());
        config.knowledge.embedding_api_key = Some("plain-kb-embedding".into());
        config.knowledge.vision_api_key = Some("plain-kb-vision".into());
        config.composio.api_key = Some("plain-composio".into());
        config.browser.computer_use.api_key = Some("plain-computer-use".into());
        config.web_search.brave_api_key = Some("plain-brave".into());
        config
            .provider_api_keys
            .insert("openrouter".into(), "plain-provider".into());
        config.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": "plain-bot-token",
                "allowed_users": ["rantaiclaw_user"],
            }))
            .unwrap(),
        );
        config.mcp_servers.insert(
            "example".into(),
            McpServerConfig {
                command: "npx".into(),
                args: Vec::new(),
                env: [("EXAMPLE_API_KEY".to_string(), "plain-mcp-env".to_string())]
                    .into_iter()
                    .collect(),
            },
        );
        config.skills.entries.insert(
            "x".into(),
            SkillEntryConfig {
                api_key: Some(SkillApiKey {
                    source: "literal".into(),
                    id: None,
                    value: Some("plain-skill-literal".into()),
                }),
                ..Default::default()
            },
        );

        let plaintexts = [
            "plain-api-key",
            "plain-kb-embedding",
            "plain-kb-vision",
            "plain-composio",
            "plain-computer-use",
            "plain-brave",
            "plain-provider",
            "plain-bot-token",
            "plain-mcp-env",
            "plain-skill-literal",
        ];

        let mut raw: toml::Value = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        let changed = encrypt_config_secrets_in_raw(&mut raw, &dir);
        assert_eq!(
            changed,
            plaintexts.len(),
            "the raw list must reach every governed field"
        );

        let serialised = toml::to_string_pretty(&raw).unwrap();
        for plain in plaintexts {
            assert!(
                !serialised.contains(plain),
                "`{plain}` is still plaintext — the raw list is missing a field \
                 `decrypt_config_secrets` governs:\n{serialised}"
            );
        }

        // And the typed authority restores every one of them.
        let mut restored: Config = toml::from_str(&serialised).unwrap();
        let store = crate::security::SecretStore::new(&dir, true);
        decrypt_config_secrets(&store, &mut restored).unwrap();
        assert_eq!(restored.api_key.as_deref(), Some("plain-api-key"));
        assert_eq!(
            restored.knowledge.embedding_api_key.as_deref(),
            Some("plain-kb-embedding")
        );
        assert_eq!(
            restored.knowledge.vision_api_key.as_deref(),
            Some("plain-kb-vision")
        );
        assert_eq!(restored.composio.api_key.as_deref(), Some("plain-composio"));
        assert_eq!(
            restored.browser.computer_use.api_key.as_deref(),
            Some("plain-computer-use")
        );
        assert_eq!(
            restored.web_search.brave_api_key.as_deref(),
            Some("plain-brave")
        );
        assert_eq!(restored.provider_api_keys["openrouter"], "plain-provider");
        assert_eq!(
            restored
                .channels_config
                .telegram
                .as_ref()
                .unwrap()
                .bot_token,
            "plain-bot-token"
        );
        assert_eq!(
            restored.mcp_servers["example"].env["EXAMPLE_API_KEY"],
            "plain-mcp-env"
        );
        assert_eq!(
            restored.skills.entries["x"]
                .api_key
                .as_ref()
                .unwrap()
                .value
                .as_deref(),
            Some("plain-skill-literal")
        );

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[test]
    async fn load_or_init_encrypts_mcp_env_left_in_plaintext_by_an_older_version() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();

        // What v27 wrote: the token in the clear.
        fs::write(
            &config_path,
            "schema_version = 27\n\n             [mcp_servers.example]\n             command = \"npx\"\n             args = [\"-y\", \"example-mcp-server\"]\n\n             [mcp_servers.example.env]\n             EXAMPLE_API_KEY = \"neutral-mcp-key-value\"\n",
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        // Unset CONFIG_DIR so the parent-shell export doesn't shadow the
        // WORKSPACE branch under test (CONFIG_DIR wins first in the resolver).
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        // The running process still gets the token it spawns the server with.
        assert_eq!(
            config.mcp_servers["example"].env["EXAMPLE_API_KEY"], "neutral-mcp-key-value",
            "the loaded config must carry the plaintext the MCP client spawns with"
        );

        // On disk it is gone.
        let on_disk = fs::read_to_string(&config_path).await.unwrap();
        assert!(
            !on_disk.contains("neutral-mcp-key-value"),
            "the plaintext token must not survive the upgrade on disk:\n{on_disk}"
        );
        let stored: Config = toml::from_str(&on_disk).unwrap();
        assert!(
            crate::security::SecretStore::is_encrypted(
                &stored.mcp_servers["example"].env["EXAMPLE_API_KEY"]
            ),
            "the value on disk must be ciphertext"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_workspace_override_uses_workspace_root_for_config() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        // Unset CONFIG_DIR so a parent-shell export doesn't shadow WORKSPACE.
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, workspace_dir.join("workspace"));
        assert_eq!(config.config_path, workspace_dir.join("config.toml"));
        assert!(workspace_dir.join("config.toml").exists());

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_workspace_suffix_uses_legacy_config_layout() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("workspace");
        let legacy_config_path = temp_home.join(".rantaiclaw").join("config.toml");

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, workspace_dir);
        assert_eq!(config.config_path, legacy_config_path);
        assert!(config.config_path.exists());

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_workspace_override_keeps_existing_legacy_config() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("custom-workspace");
        let legacy_config_dir = temp_home.join(".rantaiclaw");
        let legacy_config_path = legacy_config_dir.join("config.toml");

        fs::create_dir_all(&legacy_config_dir).await.unwrap();
        fs::write(
            &legacy_config_path,
            r#"default_temperature = 0.7
default_model = "legacy-model"
"#,
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, workspace_dir);
        assert_eq!(config.config_path, legacy_config_path);
        assert_eq!(config.default_model.as_deref(), Some("legacy-model"));

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_uses_persisted_active_workspace_marker() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let custom_config_dir = temp_home.join("profiles").join("agent-alpha");

        fs::create_dir_all(&custom_config_dir).await.unwrap();
        fs::write(
            custom_config_dir.join("config.toml"),
            "default_temperature = 0.7\ndefault_model = \"persisted-profile\"\n",
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        // Marker-driven resolution — opt out of the test isolation guard so
        // this test can exercise the default-resolution branch (HOME +
        // active_workspace.toml). HOME stays pinned to a tempdir so no real
        // ~/.rantaiclaw is touched.
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");

        persist_active_workspace_config_dir(&custom_config_dir)
            .await
            .unwrap();

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.config_path, custom_config_dir.join("config.toml"));
        assert_eq!(config.workspace_dir, custom_config_dir.join("workspace"));
        assert_eq!(config.default_model.as_deref(), Some("persisted-profile"));

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn load_or_init_env_workspace_override_takes_priority_over_marker() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let marker_config_dir = temp_home.join("profiles").join("persisted-profile");
        let env_workspace_dir = temp_home.join("env-workspace");

        fs::create_dir_all(&marker_config_dir).await.unwrap();
        fs::write(
            marker_config_dir.join("config.toml"),
            "default_temperature = 0.7\ndefault_model = \"marker-model\"\n",
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        persist_active_workspace_config_dir(&marker_config_dir)
            .await
            .unwrap();
        let _g_workspace =
            crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &env_workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.workspace_dir, env_workspace_dir.join("workspace"));
        assert_eq!(config.config_path, env_workspace_dir.join("config.toml"));

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn persist_active_workspace_marker_is_cleared_for_default_config_dir() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let default_config_dir = temp_home.join(".rantaiclaw");
        let custom_config_dir = temp_home.join("profiles").join("custom-profile");
        let marker_path = default_config_dir.join(ACTIVE_WORKSPACE_STATE_FILE);

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);

        persist_active_workspace_config_dir(&custom_config_dir)
            .await
            .unwrap();
        assert!(marker_path.exists());

        persist_active_workspace_config_dir(&default_config_dir)
            .await
            .unwrap();
        assert!(!marker_path.exists());

        let _ = fs::remove_dir_all(temp_home).await;
    }

    #[test]
    async fn env_override_empty_values_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        let original_provider = config.default_provider.clone();

        let _g_provider = crate::test_env::EnvGuard::set("RANTAICLAW_PROVIDER", "");
        config.apply_env_overrides();
        assert_eq!(config.default_provider, original_provider);
    }

    /// The test isolation guard refuses to fall back to a path derived from
    /// the developer's real `$HOME` — without an explicit override, a test
    /// would read and write the operator's real `~/.rantaiclaw`. Calling
    /// `load_or_init` with both env vars unset must surface the guard's
    /// diagnostic instead of silently migrating.
    #[test]
    async fn test_isolation_guard_refuses_load_or_init_without_an_override() {
        let _env_guard = env_override_lock().await;
        // Pin HOME to a tempdir before unsetting the overrides: if the guard
        // this test exercises were ever removed, `load_or_init` would fall
        // through to HOME-derived resolution and this test must still land
        // in a tempdir, never the developer's real ~/.rantaiclaw.
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_home).await.unwrap();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        let err = Config::load_or_init().await.expect_err(
            "missing override must surface the isolation guard, not silently load the real config",
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("test config isolation"),
            "expected the isolation guard message, got: {msg}"
        );
        assert!(
            msg.contains("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1"),
            "the message must name the opt-out so an agent in a hurry can find it: {msg}"
        );
        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// The opt-out (`RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1`) is what the
    /// default-resolution tests use to assert the shape of the default path
    /// without an explicit override. It must produce the same answer as the
    /// pre-guard fallback, so the guard changes the failure mode but not the
    /// successful one.
    #[test]
    async fn test_isolation_guard_opt_out_allows_default_resolution() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_home).await.unwrap();
        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");
        let _g_workspace = crate::test_env::EnvGuard::unset("RANTAICLAW_WORKSPACE");
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");

        // `load_or_init` runs through the same resolver; the guard admits it
        // because the opt-out is set, and the default-resolution branch lands
        // on the active profile under our pinned temp home.
        let config = Config::load_or_init()
            .await
            .expect("opt-out must admit default resolution so the existing default-layout tests keep working");
        assert!(
            config.config_path.starts_with(&temp_home),
            "the resolved config_path must be inside the pinned temp home: {:?}",
            config.config_path
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// A directory that is deliberately NOT under `std::env::temp_dir()` — a
    /// throwaway spot inside this crate's own `target/`, so a `save()` that
    /// slips past the write-site guard only ever writes build output, never
    /// the developer's real `$HOME`.
    fn non_temp_dir_scratch_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "rantaiclaw_test_save_guard_{}",
                uuid::Uuid::new_v4()
            ))
            .join("config.toml")
    }

    /// The write-site guard (in [`Config::save`]) refuses to write anywhere
    /// but a tempdir. A `config_path` outside `std::env::temp_dir()` (as
    /// `Config::default()`'s real-`$HOME`-derived path would be) must fail
    /// before any disk I/O, not silently overwrite the operator's config.
    #[test]
    async fn save_refuses_to_write_outside_a_tempdir() {
        let _env_guard = env_override_lock().await;
        let _g_allow = crate::test_env::EnvGuard::unset("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

        let unsafe_path = non_temp_dir_scratch_path();
        assert!(
            !unsafe_path.starts_with(std::env::temp_dir()),
            "test setup bug: a checkout under the OS temp dir would make this path \
             temp-dir-safe and defeat the assertion below"
        );
        let mut config = Config::default();
        config.config_path = unsafe_path.clone();

        let err = config
            .save()
            .await
            .expect_err("a config_path outside a tempdir must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("test config isolation"),
            "expected the write-site isolation guard message, got: {msg}"
        );
        assert!(
            msg.contains("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR=1"),
            "the message must name the opt-out so an agent in a hurry can find it: {msg}"
        );
        assert!(
            !unsafe_path.exists(),
            "the guard must refuse before any write, not write then complain"
        );
    }

    /// The opt-out lets `save()` proceed past the write-site guard even
    /// outside a tempdir — the same escape hatch the resolver guard uses.
    #[test]
    async fn save_with_the_opt_out_writes_outside_a_tempdir() {
        let _env_guard = env_override_lock().await;
        let _g_allow = crate::test_env::EnvGuard::set("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", "1");

        let unsafe_path = non_temp_dir_scratch_path();
        let mut config = Config::default();
        config.config_path = unsafe_path.clone();

        config
            .save()
            .await
            .expect("the opt-out must allow save() outside a tempdir");
        assert!(unsafe_path.exists());

        let _ = fs::remove_dir_all(unsafe_path.parent().unwrap()).await;
    }

    #[test]
    async fn env_override_gateway_port() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert_eq!(config.gateway.port, 9393);

        let _g_gateway_port = crate::test_env::EnvGuard::set("RANTAICLAW_GATEWAY_PORT", "8080");
        config.apply_env_overrides();
        assert_eq!(config.gateway.port, 8080);
    }

    #[test]
    async fn env_override_port_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_rc_gateway_port = crate::test_env::EnvGuard::unset("RANTAICLAW_GATEWAY_PORT");
        let _g_port = crate::test_env::EnvGuard::set("PORT", "9000");
        config.apply_env_overrides();
        assert_eq!(config.gateway.port, 9000);
    }

    #[test]
    async fn env_override_gateway_host() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert_eq!(config.gateway.host, "127.0.0.1");

        let _g_gateway_host = crate::test_env::EnvGuard::set("RANTAICLAW_GATEWAY_HOST", "0.0.0.0");
        config.apply_env_overrides();
        assert_eq!(config.gateway.host, "0.0.0.0");
    }

    #[test]
    async fn env_override_host_fallback() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_rc_gateway_host = crate::test_env::EnvGuard::unset("RANTAICLAW_GATEWAY_HOST");
        let _g_host = crate::test_env::EnvGuard::set("HOST", "0.0.0.0");
        config.apply_env_overrides();
        assert_eq!(config.gateway.host, "0.0.0.0");
    }

    #[test]
    async fn env_override_temperature() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        let _g_temperature = crate::test_env::EnvGuard::set("RANTAICLAW_TEMPERATURE", "0.5");
        config.apply_env_overrides();
        assert!((config.default_temperature - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    async fn env_override_temperature_out_of_range_ignored() {
        let _env_guard = env_override_lock().await;
        // Clean up any leftover env vars from other tests
        std::env::remove_var("RANTAICLAW_TEMPERATURE");

        let mut config = Config::default();
        let original_temp = config.default_temperature;

        // Temperature > 2.0 should be ignored
        let _g_temperature = crate::test_env::EnvGuard::set("RANTAICLAW_TEMPERATURE", "3.0");
        config.apply_env_overrides();
        assert!(
            (config.default_temperature - original_temp).abs() < f64::EPSILON,
            "Temperature 3.0 should be ignored (out of range)"
        );
    }

    #[test]
    async fn env_override_reasoning_enabled() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        assert_eq!(config.runtime.reasoning_enabled, None);

        let _g_reasoning_false =
            crate::test_env::EnvGuard::set("RANTAICLAW_REASONING_ENABLED", "false");
        config.apply_env_overrides();
        assert_eq!(config.runtime.reasoning_enabled, Some(false));

        let _g_reasoning_true =
            crate::test_env::EnvGuard::set("RANTAICLAW_REASONING_ENABLED", "true");
        config.apply_env_overrides();
        assert_eq!(config.runtime.reasoning_enabled, Some(true));
    }

    #[test]
    async fn env_override_reasoning_invalid_value_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        config.runtime.reasoning_enabled = Some(false);

        let _g_reasoning = crate::test_env::EnvGuard::set("RANTAICLAW_REASONING_ENABLED", "maybe");
        config.apply_env_overrides();
        assert_eq!(config.runtime.reasoning_enabled, Some(false));
    }

    #[test]
    async fn env_override_invalid_port_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        let original_port = config.gateway.port;

        let _g_port = crate::test_env::EnvGuard::set("PORT", "not_a_number");
        config.apply_env_overrides();
        assert_eq!(config.gateway.port, original_port);
    }

    #[test]
    async fn env_override_web_search_config() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();

        std::env::set_var("WEB_SEARCH_ENABLED", "false");
        std::env::set_var("WEB_SEARCH_PROVIDER", "brave");
        std::env::set_var("WEB_SEARCH_MAX_RESULTS", "7");
        std::env::set_var("WEB_SEARCH_TIMEOUT_SECS", "20");
        std::env::set_var("BRAVE_API_KEY", "brave-test-key");

        config.apply_env_overrides();

        assert!(!config.web_search.enabled);
        assert_eq!(config.web_search.provider, "brave");
        assert_eq!(config.web_search.max_results, 7);
        assert_eq!(config.web_search.timeout_secs, 20);
        assert_eq!(
            config.web_search.brave_api_key.as_deref(),
            Some("brave-test-key")
        );

        std::env::remove_var("WEB_SEARCH_ENABLED");
        std::env::remove_var("WEB_SEARCH_PROVIDER");
        std::env::remove_var("WEB_SEARCH_MAX_RESULTS");
        std::env::remove_var("WEB_SEARCH_TIMEOUT_SECS");
        std::env::remove_var("BRAVE_API_KEY");
    }

    #[test]
    async fn env_override_web_search_invalid_values_ignored() {
        let _env_guard = env_override_lock().await;
        let mut config = Config::default();
        let original_max_results = config.web_search.max_results;
        let original_timeout = config.web_search.timeout_secs;

        let _g_max_results = crate::test_env::EnvGuard::set("WEB_SEARCH_MAX_RESULTS", "99");
        let _g_timeout = crate::test_env::EnvGuard::set("WEB_SEARCH_TIMEOUT_SECS", "0");

        config.apply_env_overrides();

        assert_eq!(config.web_search.max_results, original_max_results);
        assert_eq!(config.web_search.timeout_secs, original_timeout);
    }

    #[test]
    async fn proxy_config_scope_services_requires_entries_when_enabled() {
        let proxy = ProxyConfig {
            enabled: true,
            http_proxy: Some("http://127.0.0.1:7890".into()),
            https_proxy: None,
            all_proxy: None,
            no_proxy: Vec::new(),
            scope: ProxyScope::Services,
            services: Vec::new(),
        };

        let error = proxy.validate().unwrap_err().to_string();
        assert!(error.contains("proxy.scope='services'"));
    }

    #[test]
    async fn env_override_proxy_scope_services() {
        let _env_guard = env_override_lock().await;
        clear_proxy_env_test_vars();

        let mut config = Config::default();
        std::env::set_var("RANTAICLAW_PROXY_ENABLED", "true");
        std::env::set_var("RANTAICLAW_HTTP_PROXY", "http://127.0.0.1:7890");
        std::env::set_var(
            "RANTAICLAW_PROXY_SERVICES",
            "provider.openai, tool.http_request",
        );
        std::env::set_var("RANTAICLAW_PROXY_SCOPE", "services");

        config.apply_env_overrides();

        assert!(config.proxy.enabled);
        assert_eq!(config.proxy.scope, ProxyScope::Services);
        assert_eq!(
            config.proxy.http_proxy.as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert!(config.proxy.should_apply_to_service("provider.openai"));
        assert!(config.proxy.should_apply_to_service("tool.http_request"));
        assert!(!config.proxy.should_apply_to_service("provider.anthropic"));

        clear_proxy_env_test_vars();
    }

    /// `NO_PROXY` must be published before the proxies it exempts from.
    ///
    /// These variables are process-global and read by every HTTP client in the
    /// process — including ones already built, and ones other threads build
    /// while this runs. Setting the proxies first left a window where traffic
    /// that should bypass them did not. Brief, but on a live process a window is
    /// real requests.
    ///
    /// Asserted on the assignment order rather than by racing a thread against
    /// the write, which would flake rather than fail.
    #[test]
    async fn no_proxy_is_published_before_the_proxies_it_exempts_from() {
        let config = ProxyConfig {
            enabled: true,
            http_proxy: Some("http://127.0.0.1:7890".into()),
            https_proxy: Some("http://127.0.0.1:7891".into()),
            all_proxy: Some("socks5://127.0.0.1:7892".into()),
            no_proxy: vec!["localhost".into(), "127.0.0.1".into()],
            ..Default::default()
        };

        let keys: Vec<&str> = config
            .process_env_assignments()
            .into_iter()
            .map(|(key, _)| key)
            .collect();

        assert_eq!(
            keys.first(),
            Some(&"NO_PROXY"),
            "the exemption list must be in place before the proxies, got {keys:?}"
        );
        for proxy_key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            assert!(keys.contains(&proxy_key), "{proxy_key} must still be set");
        }

        // And the values still round-trip, so ordering did not cost correctness.
        let assignments = config.process_env_assignments();
        let no_proxy = &assignments[0].1;
        assert!(
            no_proxy.as_deref().is_some_and(|v| v.contains("localhost")),
            "got {no_proxy:?}"
        );
    }

    #[test]
    async fn env_override_proxy_scope_environment_applies_process_env() {
        let _env_guard = env_override_lock().await;
        clear_proxy_env_test_vars();

        let mut config = Config::default();
        std::env::set_var("RANTAICLAW_PROXY_ENABLED", "true");
        std::env::set_var("RANTAICLAW_PROXY_SCOPE", "environment");
        std::env::set_var("RANTAICLAW_HTTP_PROXY", "http://127.0.0.1:7890");
        std::env::set_var("RANTAICLAW_HTTPS_PROXY", "http://127.0.0.1:7891");
        std::env::set_var("RANTAICLAW_NO_PROXY", "localhost,127.0.0.1");

        config.apply_env_overrides();

        assert_eq!(config.proxy.scope, ProxyScope::Environment);
        assert_eq!(
            std::env::var("HTTP_PROXY").ok().as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            std::env::var("HTTPS_PROXY").ok().as_deref(),
            Some("http://127.0.0.1:7891")
        );
        assert!(std::env::var("NO_PROXY")
            .ok()
            .is_some_and(|value| value.contains("localhost")));

        clear_proxy_env_test_vars();
    }

    fn runtime_proxy_cache_contains(cache_key: &str) -> bool {
        match runtime_proxy_client_cache().read() {
            Ok(guard) => guard.contains_key(cache_key),
            Err(poisoned) => poisoned.into_inner().contains_key(cache_key),
        }
    }

    #[test]
    async fn runtime_proxy_client_cache_reuses_default_profile_key() {
        let service_key = format!(
            "provider.cache_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );
        let cache_key = runtime_proxy_cache_key(
            &service_key,
            Some(DEFAULT_PROXY_REQUEST_TIMEOUT_SECS),
            Some(DEFAULT_PROXY_CONNECT_TIMEOUT_SECS),
        );

        clear_runtime_proxy_client_cache();
        assert!(!runtime_proxy_cache_contains(&cache_key));

        let _ = build_runtime_proxy_client(&service_key);
        assert!(runtime_proxy_cache_contains(&cache_key));

        let _ = build_runtime_proxy_client(&service_key);
        assert!(runtime_proxy_cache_contains(&cache_key));
    }

    #[test]
    async fn set_runtime_proxy_config_clears_runtime_proxy_client_cache() {
        let service_key = format!(
            "provider.cache_timeout_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );
        let cache_key = runtime_proxy_cache_key(&service_key, Some(30), Some(5));

        clear_runtime_proxy_client_cache();
        let _ = build_runtime_proxy_client_with_timeouts(&service_key, 30, 5);
        assert!(runtime_proxy_cache_contains(&cache_key));

        set_runtime_proxy_config(ProxyConfig::default());
        assert!(!runtime_proxy_cache_contains(&cache_key));
    }

    /// `build_runtime_proxy_client` used to leave the timeout unbounded, so
    /// a silent peer could pin the dispatch loop indefinitely. The default
    /// builder now goes through `build_runtime_proxy_client_with_timeouts`
    /// with [`DEFAULT_PROXY_REQUEST_TIMEOUT_SECS`] /
    /// [`DEFAULT_PROXY_CONNECT_TIMEOUT_SECS`], which is observable through
    /// the cache key (the unbounded key has `none|none`; the bounded one
    /// has the timeout pair).
    #[test]
    async fn runtime_proxy_default_builder_carries_bounded_timeouts() {
        let service_key = format!(
            "provider.default_timeout_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );

        clear_runtime_proxy_client_cache();
        let _ = build_runtime_proxy_client(&service_key);

        let bounded_key = runtime_proxy_cache_key(
            &service_key,
            Some(DEFAULT_PROXY_REQUEST_TIMEOUT_SECS),
            Some(DEFAULT_PROXY_CONNECT_TIMEOUT_SECS),
        );
        let unbounded_key = runtime_proxy_cache_key(&service_key, None, None);
        assert!(
            runtime_proxy_cache_contains(&bounded_key),
            "default builder no longer uses the bounded cache key — drop in timeout means \
             the dispatch loop can hang on a silent upstream"
        );
        assert!(
            !runtime_proxy_cache_contains(&unbounded_key),
            "default builder cached under the unbounded key — the timeout fix regressed"
        );
    }

    /// End-to-end check on the same code path: a request from a bounded
    /// client to a listener that accepts but never replies fails within
    /// the request timeout plus a small margin. Uses a one-second bound
    /// so the test stays fast; the production defaults are 120/10 but
    /// they share the `_with_timeouts` builder, so a regression here is
    /// a regression for the default builder too.
    #[tokio::test]
    async fn runtime_proxy_bounded_client_fails_within_the_bound() {
        // Bind a listener that accepts the TCP connection but never writes
        // a response, so the request hangs on the read. The thread detaches
        // so the test does not block on its own teardown — the OS reclaims
        // the listener when the test process exits.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local listener");
        let local_addr = listener.local_addr().expect("listener local addr");
        std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                // Hold the socket open until the OS reaps the process.
                // The client read times out long before this sleep ends.
                let _hold = stream;
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });

        let service_key = format!(
            "provider.hang_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );
        clear_runtime_proxy_client_cache();
        let client = build_runtime_proxy_client_with_timeouts(&service_key, 1, 1);

        let started = std::time::Instant::now();
        let outcome = client.get(format!("http://{local_addr}/")).send().await;
        let elapsed = started.elapsed();

        assert!(
            outcome.is_err(),
            "bounded client to a silent listener must fail, got: {outcome:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "request took {elapsed:?}, the 1s request timeout did not apply"
        );
    }

    #[test]
    async fn build_runtime_proxy_client_no_redirect_is_cached() {
        let service_key = format!(
            "provider.cache_no_redirect_test.{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock should be after unix epoch")
                .as_nanos()
        );
        let no_redirect_cache_key = format!(
            "{}|redirect=none",
            runtime_proxy_cache_key(&service_key, Some(20), Some(10))
        );
        let normal_cache_key = runtime_proxy_cache_key(&service_key, Some(20), Some(10));

        clear_runtime_proxy_client_cache();
        assert!(!runtime_proxy_cache_contains(&no_redirect_cache_key));
        assert!(!runtime_proxy_cache_contains(&normal_cache_key));

        // Two calls with the same key reuse the cached client — the cache
        // gains exactly one entry for the no-redirect key.
        let _ = build_runtime_proxy_client_no_redirect(&service_key, 20, 10);
        assert!(runtime_proxy_cache_contains(&no_redirect_cache_key));
        let _ = build_runtime_proxy_client_no_redirect(&service_key, 20, 10);
        assert!(runtime_proxy_cache_contains(&no_redirect_cache_key));

        // A normal (redirect-following) client for the same service key and
        // timeouts must land in a SEPARATE cache entry — the redirect
        // discriminator prevents aliasing between the two variants.
        assert!(!runtime_proxy_cache_contains(&normal_cache_key));
        let _ = build_runtime_proxy_client_with_timeouts(&service_key, 20, 10);
        assert!(runtime_proxy_cache_contains(&normal_cache_key));
        assert!(runtime_proxy_cache_contains(&no_redirect_cache_key));
        assert_ne!(no_redirect_cache_key, normal_cache_key);
    }

    #[test]
    async fn gateway_config_default_values() {
        let g = GatewayConfig::default();
        assert_eq!(g.port, 9393);
        assert_eq!(g.host, "127.0.0.1");
        assert!(g.require_pairing);
        assert!(!g.allow_public_bind);
        assert!(g.paired_tokens.is_empty());
        assert!(!g.trust_forwarded_headers);
        assert_eq!(g.rate_limit_max_keys, 10_000);
        assert_eq!(g.idempotency_max_keys, 10_000);
    }

    // ── Peripherals config ───────────────────────────────────────
    //
    // The `[peripherals]` section was removed with the hardware/peripheral
    // stack; nothing here parses into the new `Config` and the section is
    // intentionally kept as an unknown top-level key so the warning path
    // continues to fire on operator configs that still try to set it.

    #[test]
    async fn lark_config_serde() {
        let lc = LarkConfig {
            app_id: "cli_123456".into(),
            app_secret: "secret_abc".into(),
            encrypt_key: Some("encrypt_key".into()),
            verification_token: Some("verify_token".into()),
            allowed_users: vec!["user_123".into(), "user_456".into()],
            use_feishu: true,
            receive_mode: LarkReceiveMode::Websocket,
            port: None,
        };
        let json = serde_json::to_string(&lc).unwrap();
        let parsed: LarkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.app_id, "cli_123456");
        assert_eq!(parsed.app_secret, "secret_abc");
        assert_eq!(parsed.encrypt_key.as_deref(), Some("encrypt_key"));
        assert_eq!(parsed.verification_token.as_deref(), Some("verify_token"));
        assert_eq!(parsed.allowed_users.len(), 2);
        assert!(parsed.use_feishu);
    }

    #[test]
    async fn lark_config_toml_roundtrip() {
        let lc = LarkConfig {
            app_id: "cli_123456".into(),
            app_secret: "secret_abc".into(),
            encrypt_key: Some("encrypt_key".into()),
            verification_token: Some("verify_token".into()),
            allowed_users: vec!["*".into()],
            use_feishu: false,
            receive_mode: LarkReceiveMode::Webhook,
            port: Some(9898),
        };
        let toml_str = toml::to_string(&lc).unwrap();
        let parsed: LarkConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.app_id, "cli_123456");
        assert_eq!(parsed.app_secret, "secret_abc");
        assert!(!parsed.use_feishu);
    }

    #[test]
    async fn lark_config_deserializes_without_optional_fields() {
        let json = r#"{"app_id":"cli_123","app_secret":"secret"}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.encrypt_key.is_none());
        assert!(parsed.verification_token.is_none());
        assert!(parsed.allowed_users.is_empty());
        assert!(!parsed.use_feishu);
    }

    #[test]
    async fn lark_config_defaults_to_lark_endpoint() {
        let json = r#"{"app_id":"cli_123","app_secret":"secret"}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert!(
            !parsed.use_feishu,
            "use_feishu should default to false (Lark)"
        );
    }

    #[test]
    async fn lark_config_with_wildcard_allowed_users() {
        let json = r#"{"app_id":"cli_123","app_secret":"secret","allowed_users":["*"]}"#;
        let parsed: LarkConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.allowed_users, vec!["*"]);
    }

    #[test]
    async fn nextcloud_talk_config_serde() {
        let nc = NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: Some("webhook-secret".into()),
            allowed_users: vec!["user_a".into(), "*".into()],
        };

        let json = serde_json::to_string(&nc).unwrap();
        let parsed: NextcloudTalkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.base_url, "https://cloud.example.com");
        assert_eq!(parsed.app_token, "app-token");
        assert_eq!(parsed.webhook_secret.as_deref(), Some("webhook-secret"));
        assert_eq!(parsed.allowed_users, vec!["user_a", "*"]);
    }

    #[test]
    async fn nextcloud_talk_config_defaults_optional_fields() {
        let json = r#"{"base_url":"https://cloud.example.com","app_token":"app-token"}"#;
        let parsed: NextcloudTalkConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.webhook_secret.is_none());
        assert!(parsed.allowed_users.is_empty());
    }

    // ── Config file permission hardening (Unix only) ───────────────

    #[cfg(unix)]
    #[test]
    async fn new_config_file_has_restricted_permissions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Create a config and save it. `save()` itself must publish the file
        // owner-only (0600) — no external chmod step — since it carries secrets.
        let mut config = Config::default();
        config.config_path = config_path.clone();
        config.save().await.unwrap();

        let meta = fs::metadata(&config_path).await.unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "save() must produce an owner-only (0600) config, got {mode:o}"
        );
    }

    #[test]
    async fn atomic_write_config_replaces_and_manages_backup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("config.toml");
        let bak = tmp.path().join("config.toml.bak");

        // First write (no existing) — no backup.
        atomic_write_config(&target, b"a = 1\n", false)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "a = 1\n");
        assert!(!bak.exists(), "no backup on first write");

        // keep_backup = false drops the backup on success.
        atomic_write_config(&target, b"a = 2\n", false)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "a = 2\n");
        assert!(!bak.exists(), "backup dropped when keep_backup=false");

        // keep_backup = true (the migration write-back) retains the prior content.
        atomic_write_config(&target, b"a = 3\n", true)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&target).await.unwrap(), "a = 3\n");
        assert!(bak.exists(), "backup kept when keep_backup=true");
        assert_eq!(fs::read_to_string(&bak).await.unwrap(), "a = 2\n");

        // The atomic temp file must never be left behind.
        let residue: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(residue.is_empty(), "temp residue: {residue:?}");
    }

    #[test]
    async fn load_from_path_decrypts_all_secrets() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let mut config = Config::default();
        config.config_path = path.clone();
        config
            .provider_api_keys
            .insert("openai".into(), "sk-secret-xyz".into());
        config.save().await.unwrap(); // encrypts secrets at rest

        // On disk the provider key (a NON-api_key secret) must be ciphertext.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !raw.contains("sk-secret-xyz"),
            "provider key was not encrypted on disk: {raw}"
        );

        // load_from_path must decrypt it back — the runtime reload path used to
        // decrypt only `api_key`, leaving keys like this one `enc2:`-prefixed.
        let loaded = Config::load_from_path(&path).await.unwrap();
        assert_eq!(
            loaded.provider_api_keys.get("openai").map(String::as_str),
            Some("sk-secret-xyz"),
            "load_from_path must decrypt every secret, not just api_key"
        );
    }

    #[cfg(unix)]
    #[test]
    async fn resaving_world_readable_config_relocks_to_0600() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Simulate a pre-existing world-readable config (the state real hosts
        // were found in: an external edit left it 0664).
        let mut config = Config::default();
        config.config_path = config_path.clone();
        config.save().await.unwrap();
        fs::set_permissions(&config_path, Permissions::from_mode(0o664))
            .await
            .unwrap();

        // A subsequent save (e.g. a `bind-telegram` allowlist edit) must re-lock.
        config.save().await.unwrap();

        let mode = fs::metadata(&config_path)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "re-save must re-lock to 0600, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    async fn world_readable_config_is_detectable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        // Create a config file with intentionally loose permissions
        std::fs::write(&config_path, "# test config").unwrap();
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let meta = std::fs::metadata(&config_path).unwrap();
        let mode = meta.permissions().mode();
        assert!(
            mode & 0o004 != 0,
            "Test setup: file should be world-readable (mode {mode:o})"
        );
    }

    // ── v34: markdown memory import ──────────────────────────
    //
    // The import runs from `Config::load_or_init`, gated on the migration
    // running AND the pre-migration config naming `markdown`. These tests
    // drive the helper directly with a controlled workspace, so they are
    // independent of the env-driven path resolution the load tests rely on.

    /// Build a workspace containing the wizard's `MEMORY.md` scaffold, one
    /// structured entry, one unstructured note, plus a daily file.
    fn markdown_workspace(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();

        // Full wizard template (wizard.rs:5544-5563) so the scaffold filter
        // is exercised against the exact lines the wizard writes today.
        std::fs::write(
            workspace.join("MEMORY.md"),
            "\
# MEMORY.md — Long-Term Memory

*Your curated memories. The distilled essence, not raw logs.*

## How This Works
- Daily files (`memory/YYYY-MM-DD.md`) capture raw events (on-demand via tools)
- This file captures what's WORTH KEEPING long-term
- This file is auto-injected into your system prompt each session
- Keep it concise — every character here costs tokens

## Security
- ONLY loaded in main session (direct chat with your human)
- NEVER loaded in group chats or shared contexts

---

## Key Facts
(Add important facts about your human here)

## Decisions & Preferences
(Record decisions and preferences here)

## Lessons Learned
(Document mistakes and insights here)

## Open Loops
(Track unfinished tasks and follow-ups here)

- **user_lang**: prefers Rust
- A standalone prose note about project X.
",
        )
        .unwrap();

        std::fs::write(
            workspace.join("memory").join("2026-09-01.md"),
            "\
# 2026-09-01

- **morning_mood**: curious
- just woke up
",
        )
        .unwrap();

        workspace
    }

    /// The wizard's scaffold and `---` separators do NOT become entries;
    /// the operator's structured and unstructured lines DO. The daily file
    /// contributes its own entries. A backup directory is created next to
    /// `MEMORY.md` with the originals.
    #[tokio::test]
    async fn markdown_import_moves_entries_into_brain_db_and_skips_scaffold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let db_path = workspace.join("memory").join("brain.db");
        assert!(db_path.exists(), "brain.db must be created");

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT key, category FROM memories ORDER BY category, key")
            .unwrap();
        let entries: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"user_lang"), "structured entry survives");
        assert!(
            keys.iter()
                .any(|k| k.starts_with("openclaw_openclaw_core_")),
            "unstructured prose becomes an entry"
        );
        assert!(
            keys.contains(&"morning_mood"),
            "daily file entries are imported"
        );
        // The scaffold lines must not survive as entries. We seeded several
        // placeholders (`Daily files`, `(Add important facts...`, etc.) —
        // even after the key naming, none of them should appear as a row.
        for k in &keys {
            assert!(
                !k.contains("Daily files"),
                "scaffold content must not become a key: {k}"
            );
            assert!(
                !k.contains("Add important facts"),
                "placeholder prose must not become a key: {k}"
            );
            assert!(
                !k.contains("Record decisions"),
                "placeholder prose must not become a key: {k}"
            );
        }
        // The unstructured prose's content must be present in the entry set.
        let conn_for_content = rusqlite::Connection::open(&db_path).unwrap();
        let mut content_stmt = conn_for_content
            .prepare("SELECT content FROM memories WHERE category = 'core'")
            .unwrap();
        let core_contents: Vec<String> = content_stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        drop(content_stmt);
        assert!(
            core_contents
                .iter()
                .any(|c| c.contains("standalone prose note")),
            "the unstructured prose is in brain.db as a core entry: {core_contents:?}"
        );

        // Backup directory holds the originals, NOT the projection.
        let backup_root = workspace
            .join("memory")
            .join("migrations")
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("markdown-"))
            .expect("a markdown- backup directory must be created");
        let backup_dir = backup_root.path();
        assert!(backup_dir.join("MEMORY.md").exists());
        assert!(
            backup_dir.join("memory").join("2026-09-01.md").exists(),
            "the backup mirrors the source layout: daily files under its own memory/ subdir"
        );
        let backup_memory = std::fs::read_to_string(backup_dir.join("MEMORY.md")).unwrap();
        assert!(
            backup_memory.contains("**user_lang**"),
            "the backup must hold the operator's pre-projection MEMORY.md"
        );
    }

    /// After import, the new `MEMORY.md` carries the sqlite projection only:
    /// the marked block holds every imported core entry, and there is NO
    /// `- **key**:` line outside the markers (the wizard scaffold bullets and
    /// the operator's structured lines were both rewritten by the projection).
    #[tokio::test]
    async fn markdown_import_rewrites_memory_md_to_projection_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let after = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert!(after.contains(crate::memory::snapshot::PROJECTION_BEGIN));
        assert!(after.contains(crate::memory::snapshot::PROJECTION_END));
        assert!(
            after.contains("- user_lang: prefers Rust"),
            "the structured entry is now in the projection block"
        );

        // No `- **key**:` line outside the projection block. The wizard
        // scaffold (`- **key**:` lines from the template, plus prose like
        // `(Add important facts...)`) is no longer present.
        let projection_start = after
            .find(crate::memory::snapshot::PROJECTION_BEGIN)
            .expect("projection begin marker");
        let projection_end = after
            .find(crate::memory::snapshot::PROJECTION_END)
            .expect("projection end marker")
            + crate::memory::snapshot::PROJECTION_END.len();
        let outside = format!("{}{}", &after[..projection_start], &after[projection_end..]);
        assert!(
            !outside.contains("- **"),
            "no `- **key**:` line survives outside the projection block:\n{outside}"
        );
    }

    /// Owner decision (2026-09-28): on a key conflict the markdown value
    /// wins, because it is what the operator has been using. This replaces
    /// `markdown_import_keeps_an_existing_brain_db_value`, which pinned the
    /// opposite (`INSERT OR IGNORE`) — `brain.db` is backed up first (see the
    /// assertion below), so the old value is not lost, just no longer live.
    #[tokio::test]
    async fn markdown_import_overwrites_conflicting_key_with_markdown_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);

        // Seed brain.db with a different, older content under the same key.
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        let db_path = workspace.join("memory").join("brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        crate::memory::SqliteMemory::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at) \
             VALUES ('id-existing', 'user_lang', 'stale sqlite value', 'core', \
                     '2020-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00')",
            rusqlite::params![],
        )
        .unwrap();
        drop(conn);

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let content: String = conn
            .query_row(
                "SELECT content FROM memories WHERE key = 'user_lang'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            content, "prefers Rust",
            "the markdown value must win over a pre-existing brain.db row"
        );

        // The pre-import brain.db (with the stale value) is recoverable from
        // the backup, since the conflict overwrote the live row.
        let backup_root = workspace
            .join("memory")
            .join("migrations")
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("markdown-"))
            .expect("a markdown- backup directory must be created");
        let backed_up_db = backup_root.path().join("memory").join("brain.db");
        assert!(
            backed_up_db.exists(),
            "the backup must hold a copy of brain.db taken before the import"
        );
        let backup_conn = rusqlite::Connection::open(&backed_up_db).unwrap();
        let backed_up_content: String = backup_conn
            .query_row(
                "SELECT content FROM memories WHERE key = 'user_lang'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            backed_up_content, "stale sqlite value",
            "the backed-up brain.db must hold the pre-import (stale) value"
        );
    }

    /// The scaffold filter matches a template line only when it is exactly
    /// equal (after trim), not by prefix: the italic intro line (which has
    /// no bullet prefix at all) must never leak into `brain.db`, and an
    /// operator line that merely starts with the same words as a template
    /// bullet must survive.
    #[tokio::test]
    async fn markdown_import_filters_template_lines_by_exact_match_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();

        let mut memory_md = crate::memory::MEMORY_MD_TEMPLATE.to_string();
        memory_md.push_str(
            "\n- This file is auto-injected into your system prompt each session, \
             but I extended it.\n",
        );
        std::fs::write(workspace.join("MEMORY.md"), memory_md).unwrap();

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let db_path = workspace.join("memory").join("brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn.prepare("SELECT content FROM memories").unwrap();
        let contents: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        drop(stmt);

        for line in crate::memory::MEMORY_MD_TEMPLATE.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let as_content = trimmed.strip_prefix("- ").unwrap_or(trimmed);
            assert!(
                !contents.iter().any(|c| c == as_content),
                "template line leaked into brain.db as a memory entry: {as_content:?}"
            );
        }

        assert!(
            contents.iter().any(|c| c
                == "This file is auto-injected into your system prompt each session, \
                    but I extended it."),
            "an operator line that merely starts with template words must survive: {contents:?}"
        );
    }

    /// A line whose key looks like a runtime autosave key (`<prefix>_<uuid>`)
    /// is imported as `conversation` regardless of the markdown file it came
    /// from, so the shared cross-chat backfill continues to skip it.
    #[tokio::test]
    async fn markdown_import_categorizes_autosave_keyed_lines_as_conversation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(
            workspace.join("memory").join("2026-09-01.md"),
            "- **daily_550e8400-e29b-41d4-a716-446655440000**: raw autosave note\n",
        )
        .unwrap();

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let db_path = workspace.join("memory").join("brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let category: String = conn
            .query_row(
                "SELECT category FROM memories \
                 WHERE key = 'daily_550e8400-e29b-41d4-a716-446655440000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            category, "conversation",
            "an autosave-keyed line must be imported as conversation, not daily"
        );
    }

    /// The reader must skip the projection markers and everything between
    /// them, not just avoid treating the markers themselves as scaffold: a
    /// markdown source that already holds a rendered projection block (and a
    /// second, partial one with no closing marker) must import only the
    /// operator's own line. No marker text and no previously-projected line
    /// may reach `brain.db`. This is what keeps a retry over the same frozen
    /// backup from re-importing its own earlier output and growing without
    /// bound — the historical bug this guards against.
    #[tokio::test]
    async fn markdown_import_skips_projection_markers_and_does_not_grow_on_retry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();

        let begin = crate::memory::snapshot::PROJECTION_BEGIN;
        let end = crate::memory::snapshot::PROJECTION_END;
        let memory_md = format!(
            "- **operator_key**: operator value\n\n\
             {begin}\n\
             <!-- Generated from core memory. Edits inside this block are overwritten; \
             write prose outside it. -->\n\
             - projected_key: projected value\n\
             {end}\n\n\
             {begin}\n\
             - stale_key: stale partial value\n"
        );
        std::fs::write(workspace.join("MEMORY.md"), &memory_md).unwrap();

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let db_path = workspace.join("memory").join("brain.db");
        let read_rows = |conn: &rusqlite::Connection| -> Vec<(String, String)> {
            let mut stmt = conn.prepare("SELECT key, content FROM memories").unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        };

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let rows = read_rows(&conn);
        drop(conn);

        assert!(
            rows.iter().any(|(k, _)| k == "operator_key"),
            "the operator's own line must still be imported: {rows:?}"
        );
        assert!(
            !rows
                .iter()
                .any(|(k, _)| k == "projected_key" || k == "stale_key"),
            "no line from inside a projection block (complete or partial) may be imported: \
             {rows:?}"
        );
        assert!(
            !rows
                .iter()
                .any(|(_, c)| c.contains("Generated from core memory")
                    || c.contains(begin)
                    || c.contains(end)),
            "no marker text may ever become memory content: {rows:?}"
        );

        // Simulate a retry directly over the same frozen backup (the
        // IMPORTED-marker gate that would normally prevent this is tested
        // separately in `retry_imports_a_pending_markdown_backup_but_skips_a_finished_one`).
        // The reader's own marker skip must be what keeps this from growing
        // brain.db or MEMORY.md, independent of that marker file.
        let backup_dir = workspace
            .join("memory")
            .join("migrations")
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("markdown-"))
            .expect("a markdown- backup directory must be created")
            .path();

        let row_count_before = rows.len();
        let memory_md_before = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();

        super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let rows_after = read_rows(&conn);
        drop(conn);
        let memory_md_after = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();

        assert_eq!(
            rows_after.len(),
            row_count_before,
            "re-importing the same frozen source must not grow brain.db: {rows_after:?}"
        );
        assert_eq!(
            memory_md_before, memory_md_after,
            "re-importing the same frozen source must not grow MEMORY.md"
        );
    }

    /// Only the freshly written block survives a rewrite. `splice_block`
    /// (`src/memory/snapshot.rs`) only ever replaces the FIRST marker pair
    /// and copies everything after its end marker through unchanged, so a
    /// stale second pair, prose sitting between two pairs, and a trailing
    /// partial pair with no closing marker are all leftovers `splice_block`
    /// never produced and never will clean up on its own — they must be
    /// dropped here, not kept alongside the first block. Keeping them (by
    /// taking a LATER end marker) would let them survive every further
    /// rewrite, forever, next to the one live block.
    #[tokio::test]
    async fn rewrite_memory_md_to_projection_only_keeps_only_the_first_block() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let begin = crate::memory::snapshot::PROJECTION_BEGIN;
        let end = crate::memory::snapshot::PROJECTION_END;
        let malformed = format!(
            "{begin}\n- first: block\n{end}\nstale prose between pairs\n{begin}\n\
             - second: block\n{end}\n{begin}\n- dangling: block\n"
        );
        std::fs::write(workspace.join("MEMORY.md"), &malformed).unwrap();

        super::rewrite_memory_md_to_projection_only(&workspace).unwrap();
        let after_first_pass = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();

        assert_eq!(
            after_first_pass,
            format!("{begin}\n- first: block\n{end}"),
            "only the first block, exactly, must remain: {after_first_pass:?}"
        );
        assert!(
            !after_first_pass.contains("second: block"),
            "a stale second pair must be dropped: {after_first_pass:?}"
        );
        assert!(
            !after_first_pass.contains("stale prose"),
            "prose left between two pairs must be dropped: {after_first_pass:?}"
        );
        assert!(
            !after_first_pass.contains("dangling: block"),
            "a trailing partial pair with no closing marker must be dropped: {after_first_pass:?}"
        );

        // A repeated rewrite over the now-clean content must be a no-op:
        // this is what keeps a retry from growing MEMORY.md on every write.
        super::rewrite_memory_md_to_projection_only(&workspace).unwrap();
        let after_second_pass = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert_eq!(
            after_first_pass, after_second_pass,
            "a repeated rewrite over already-clean content must not change it"
        );
    }

    /// The rewrite keeps the mode of the file it replaces. 0o640 is neither
    /// the 0o600 the temp file starts with under a strict umask nor the 0o644
    /// it starts with under the usual one, so a rewrite that forgets to copy
    /// the mode cannot pass by luck.
    #[cfg(unix)]
    #[test]
    async fn rewrite_memory_md_to_projection_only_keeps_the_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("MEMORY.md");
        let begin = crate::memory::snapshot::PROJECTION_BEGIN;
        let end = crate::memory::snapshot::PROJECTION_END;
        std::fs::write(&path, format!("{begin}\n- a: b\n{end}\nprivate notes\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        super::rewrite_memory_md_to_projection_only(tmp.path()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o640,
            "the rewrite must keep the file mode, got {mode:o}"
        );
    }

    /// A symlinked `MEMORY.md` stays a symlink: the rewrite replaces the
    /// link target, not the link.
    #[cfg(unix)]
    #[test]
    async fn rewrite_memory_md_to_projection_only_writes_through_a_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("notes-target.md");
        let link = tmp.path().join("MEMORY.md");
        let begin = crate::memory::snapshot::PROJECTION_BEGIN;
        let end = crate::memory::snapshot::PROJECTION_END;
        std::fs::write(&target, format!("{begin}\n- a: b\n{end}\nprivate notes\n")).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        super::rewrite_memory_md_to_projection_only(tmp.path()).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "MEMORY.md must still be a symlink"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            format!("{begin}\n- a: b\n{end}"),
            "the link target must hold the new content"
        );
    }

    /// A markdown backup left without an `IMPORTED` marker (an earlier
    /// failed import) is retried; one that already finished is not touched
    /// again.
    #[tokio::test]
    async fn retry_imports_a_pending_markdown_backup_but_skips_a_finished_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let pending = workspace
            .join("memory")
            .join("migrations")
            .join("markdown-pending-1");
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(
            pending.join("MEMORY.md"),
            "- **k_pending**: pending value\n",
        )
        .unwrap();
        std::fs::write(pending.join("BACKUP_COMPLETE"), "").unwrap();

        let done = workspace
            .join("memory")
            .join("migrations")
            .join("markdown-done-2");
        std::fs::create_dir_all(&done).unwrap();
        std::fs::write(
            done.join("MEMORY.md"),
            "- **k_done**: must stay untouched\n",
        )
        .unwrap();
        std::fs::write(done.join("BACKUP_COMPLETE"), "").unwrap();
        std::fs::write(done.join("IMPORTED"), "").unwrap();

        super::retry_unimported_markdown_imports(&workspace, false);

        let db_path = workspace.join("memory").join("brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let pending_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE key = 'k_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending_count, 1,
            "a backup without IMPORTED must be retried"
        );

        let done_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE key = 'k_done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(done_count, 0, "a backup with IMPORTED must not be retried");

        assert!(
            pending.join("IMPORTED").exists(),
            "a successful retry must write the IMPORTED marker"
        );
    }

    /// Two pending backups disagree on the same key; the later one must win,
    /// and it is created first on disk. Each backup's rows carry that
    /// backup's time, so the newer backup's value survives whichever order
    /// the sweep imports them in. That the sweep also goes oldest first, by
    /// name, is pinned independently of `read_dir` order by
    /// `pending_markdown_backups_are_listed_oldest_first_whatever_the_input_order`.
    #[tokio::test]
    async fn retry_processes_pending_backups_in_timestamp_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let newer = workspace
            .join("memory")
            .join("migrations")
            .join("markdown-20260202-000000-1");
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(newer.join("MEMORY.md"), "- **k**: newer value\n").unwrap();
        std::fs::write(newer.join("BACKUP_COMPLETE"), "2026-02-02T00:00:00+00:00").unwrap();

        let older = workspace
            .join("memory")
            .join("migrations")
            .join("markdown-20260101-000000-1");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::write(older.join("MEMORY.md"), "- **k**: older value\n").unwrap();
        std::fs::write(older.join("BACKUP_COMPLETE"), "2026-01-01T00:00:00+00:00").unwrap();

        super::retry_unimported_markdown_imports(&workspace, false);

        let db_path = workspace.join("memory").join("brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let content: String = conn
            .query_row("SELECT content FROM memories WHERE key = 'k'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            content, "newer value",
            "the later-named backup must be applied last, so its value wins"
        );
    }

    /// A copy that fails partway through `backup_markdown_memory` (a daily
    /// file, `ENOSPC`, the `brain.db` vacuum) must leave a directory the
    /// sweep never imports from: it is missing `BACKUP_COMPLETE`, written
    /// only as the very last step of a successful backup.
    #[tokio::test]
    async fn retry_never_imports_a_backup_left_partial_by_a_failed_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: v\n").unwrap();
        // A directory masquerading as a daily markdown file makes the copy
        // fail partway through `backup_markdown_memory`, after `MEMORY.md`
        // was already copied.
        std::fs::create_dir_all(workspace.join("memory").join("broken.md")).unwrap();

        let result = crate::migration::backup_markdown_memory(&workspace);
        assert!(
            result.is_err(),
            "a failed copy must surface as an error, not a silent partial backup"
        );

        // The sweep must never import from the partial directory the failed
        // call left behind.
        super::retry_unimported_markdown_imports(&workspace, false);
        let db_path = workspace.join("memory").join("brain.db");
        assert!(
            !db_path.exists(),
            "a partial backup from a failed copy must never be imported"
        );
    }

    /// A flat backup made by the already-merged, pre-completeness-marker
    /// development build — files copied directly under `markdown-<ts>/`, no
    /// `memory/` subdirectory, no `BACKUP_COMPLETE`, no `IMPORTED` — must
    /// never be swept and imported: doing so would apply markdown-wins over
    /// whatever `brain.db` has accumulated since, with no backed-up
    /// `brain.db` to recover from.
    #[tokio::test]
    async fn retry_skips_an_old_style_flat_backup_without_a_completeness_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let old_backup = workspace
            .join("memory")
            .join("migrations")
            .join("markdown-20260101-000000");
        std::fs::create_dir_all(&old_backup).unwrap();
        std::fs::write(
            old_backup.join("MEMORY.md"),
            "- **k_old**: must stay untouched\n",
        )
        .unwrap();

        super::retry_unimported_markdown_imports(&workspace, false);

        let db_path = workspace.join("memory").join("brain.db");
        assert!(
            !db_path.exists(),
            "an old-style backup without a completeness marker must never be imported"
        );
    }

    /// `brain.db` runs WAL; a row committed but not yet checkpointed into
    /// the main file lives only in `-wal`. A plain file copy of the main
    /// file alone would miss it; `VACUUM INTO` must not.
    #[tokio::test]
    async fn backup_markdown_memory_captures_uncheckpointed_wal_data() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: v\n").unwrap();

        let db_path = workspace.join("memory").join("brain.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        crate::memory::SqliteMemory::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at) \
             VALUES ('id-wal', 'wal_only_key', 'wal only value', 'core', 't', 't')",
            [],
        )
        .unwrap();
        // `conn` stays open and un-checkpointed: the row lives only in the
        // `-wal` file at the moment the backup runs.

        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        drop(conn);

        let backup_db = backup_dir.join("memory").join("brain.db");
        assert!(
            backup_db.exists(),
            "the backup must include a brain.db copy"
        );
        let backup_conn = rusqlite::Connection::open(&backup_db).unwrap();
        let content: String = backup_conn
            .query_row(
                "SELECT content FROM memories WHERE key = 'wal_only_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            content, "wal only value",
            "un-checkpointed WAL data must survive the backup"
        );
    }

    /// A running daemon holds the database's write lock while it stores a
    /// note. The import must wait for that lock, not fail and leave the notes
    /// to the next start. A deferred transaction reads first and asks for the
    /// write lock on its first `INSERT`; SQLite does not run the busy handler
    /// for that upgrade, so it returned `SQLITE_BUSY` at once.
    #[tokio::test]
    async fn markdown_import_waits_for_a_writer_that_holds_the_database() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **busy_key**: busy value\n").unwrap();

        // The schema exists before the writer starts, so the import's own
        // schema step has nothing to create and takes no write lock: the
        // transaction is the first thing that needs one.
        let db_path = workspace.join("memory").join("brain.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
            crate::memory::SqliteMemory::init_schema(&conn).unwrap();
        }
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();

        // The writer owns the lock before the import starts and gives it up
        // after a fixed delay far below the import's 5 s busy timeout.
        let writer = rusqlite::Connection::open(&db_path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            writer.execute_batch("COMMIT;").unwrap();
        });

        let outcome = super::import_markdown_backup_into_sqlite(&workspace, &backup_dir);
        release.join().unwrap();

        outcome.expect("the import waits for the writer instead of failing");
        assert!(
            backup_dir.join("IMPORTED").exists(),
            "a finished import is marked done"
        );
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let content: String = conn
            .query_row(
                "SELECT content FROM memories WHERE key = 'busy_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content, "busy value");
    }

    /// `FULL` makes the commit durable on its own, which the `IMPORTED` marker
    /// relies on. The import's connection is the only place the setting
    /// lives, so it is read back from there.
    #[tokio::test]
    async fn markdown_import_connection_commits_with_full_synchronous() {
        let tmp = tempfile::TempDir::new().unwrap();

        let conn = super::open_markdown_import_connection(tmp.path()).unwrap();

        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2, "0 = OFF, 1 = NORMAL, 2 = FULL");
    }

    /// The start-up compares the live notes with an existing backup. A FIFO
    /// named like a note, newer than the backup, must not be opened: opening
    /// one for reading blocks until a writer appears, and the start-up waits
    /// with it.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_markdown_newer_than_backup_does_not_open_a_fifo_named_like_a_note() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: v\n").unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        // Make every live file newer than the backup without waiting for the
        // clock.
        std::fs::OpenOptions::new()
            .write(true)
            .open(backup_dir.join("BACKUP_COMPLETE"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        // The live `MEMORY.md` is a newer note too, so only the FIFO is left.
        std::fs::remove_file(workspace.join("MEMORY.md")).unwrap();
        let fifo =
            crate::migration::test_fifo::Fifo::create(&workspace.join("memory").join("pipe.md"));

        assert!(
            !super::live_markdown_newer_than_backup(&workspace, &backup_dir),
            "a FIFO is not a note"
        );
        assert!(!fifo.was_opened(), "the FIFO must not be opened");

        // The check still sees a real note next to the FIFO.
        std::fs::write(
            workspace.join("memory").join("2026-09-03.md"),
            "- **fresh**: written after the backup\n",
        )
        .unwrap();
        assert!(super::live_markdown_newer_than_backup(
            &workspace,
            &backup_dir
        ));
    }

    /// The live `MEMORY.md` is read before the import rewrites it. One that
    /// became a FIFO after the backup is left alone, and the import, whose
    /// rows are already committed, still finishes.
    #[cfg(unix)]
    #[tokio::test]
    async fn import_leaves_a_live_memory_md_that_became_a_fifo_unopened() {
        use std::os::unix::fs::FileTypeExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: v\n").unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        std::fs::remove_file(workspace.join("MEMORY.md")).unwrap();
        let fifo = crate::migration::test_fifo::Fifo::create(&workspace.join("MEMORY.md"));

        super::import_markdown_backup_into_sqlite(&workspace, &backup_dir)
            .expect("the rows import even though the projection step is refused");

        assert!(!fifo.was_opened(), "the FIFO must not be opened");
        assert!(backup_dir.join("IMPORTED").exists());
        assert!(std::fs::symlink_metadata(workspace.join("MEMORY.md"))
            .unwrap()
            .file_type()
            .is_fifo());
    }

    /// A live `MEMORY.md` edited after the backup was taken (a slow first
    /// start, a crash and a manual fix, a later retry) must survive: the
    /// projection step only rewrites the live file when it is still
    /// byte-identical to the backup's copy, and otherwise leaves it alone.
    /// The row import itself must still succeed either way.
    #[tokio::test]
    async fn import_leaves_a_live_memory_md_edited_after_the_backup_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: v\n").unwrap();

        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();

        // The operator edits MEMORY.md after the backup was taken.
        std::fs::write(
            workspace.join("MEMORY.md"),
            "- **k**: v\n\noperator's new note\n",
        )
        .unwrap();
        let live_before = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();

        assert!(
            super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).is_ok(),
            "the row import must succeed even when the live MEMORY.md has since changed"
        );

        let live_after = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert_eq!(
            live_before, live_after,
            "an operator edit made after the backup must survive the projection step"
        );
    }

    /// Once a backup's rows are imported, the `IMPORTED` marker is written
    /// immediately — before the projection step runs, and independent of
    /// whether it succeeds. A later sweep must never re-run the import for
    /// that backup, even though its first projection attempt was skipped
    /// (S4's byte-identity guard, exercised here as the forcing failure):
    /// otherwise markdown-wins would keep re-applying over edits and
    /// deletes made after the import first completed.
    #[tokio::test]
    async fn a_completed_import_is_never_retried_even_when_its_projection_was_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: markdown value\n").unwrap();

        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();

        // Force the projection step to be skipped: the live file no longer
        // matches what the backup froze.
        std::fs::write(workspace.join("MEMORY.md"), "operator's own new note\n").unwrap();

        assert!(
            super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).is_ok(),
            "the row import must succeed even though the projection step is skipped"
        );

        let db_path = workspace.join("memory").join("brain.db");
        assert!(
            backup_dir.join("IMPORTED").exists(),
            "the row import must be marked done even though the projection was skipped"
        );

        // The operator edits the imported row directly afterward.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE memories SET content = 'operator edited value' WHERE key = 'k'",
                [],
            )
            .unwrap();
        }

        // Simulate the next start: the sweep must see IMPORTED and skip
        // this backup entirely, not re-apply markdown-wins over the edit.
        super::retry_unimported_markdown_imports(&workspace, false);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let content: String = conn
            .query_row("SELECT content FROM memories WHERE key = 'k'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            content, "operator edited value",
            "a completed import must never be retried, even if its projection step was skipped"
        );

        // The operator's own MEMORY.md edit must also have survived.
        let memory_md = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert_eq!(memory_md, "operator's own new note\n");
    }

    /// The one-time import is gated on the pre-migration backend having been
    /// `markdown`: a config that migrates from schema 33 while already on
    /// `sqlite` must not import anything and must leave `MEMORY.md` as the
    /// operator left it.
    #[test]
    async fn load_or_init_does_not_import_when_pre_migration_backend_was_sqlite() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();
        fs::write(
            &config_path,
            "schema_version = 33\n\n[memory]\nbackend = \"sqlite\"\n",
        )
        .await
        .unwrap();

        let final_workspace = workspace_dir.join("workspace");
        fs::create_dir_all(&final_workspace).await.unwrap();
        fs::write(final_workspace.join("MEMORY.md"), "- **k**: v\n")
            .await
            .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.memory.backend, "sqlite");
        assert!(
            !final_workspace.join("memory").join("brain.db").exists(),
            "no import must run for a config that was already sqlite"
        );
        let memory_md = fs::read_to_string(final_workspace.join("MEMORY.md"))
            .await
            .unwrap();
        assert_eq!(
            memory_md, "- **k**: v\n",
            "MEMORY.md must be untouched when the pre-migration backend was not markdown"
        );
        assert!(
            !final_workspace.join("memory").join("migrations").exists(),
            "no backup directory should be created"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// The sweep is skipped entirely when the configured backend is not
    /// `sqlite` — even for a backup that is complete and pending. Importing
    /// into a store the config no longer reads would write `brain.db` and
    /// rewrite `MEMORY.md` for nothing.
    #[test]
    async fn load_or_init_skips_the_sweep_when_backend_is_not_sqlite() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();
        fs::write(
            &config_path,
            "schema_version = 33\n\n[memory]\nbackend = \"none\"\n",
        )
        .await
        .unwrap();

        // A complete, pending backup already sitting in the workspace, as
        // if left over from an earlier run on `sqlite` before the operator
        // switched the backend to `none`.
        let final_workspace = workspace_dir.join("workspace");
        let pending = final_workspace
            .join("memory")
            .join("migrations")
            .join("markdown-pending-4");
        fs::create_dir_all(&pending).await.unwrap();
        fs::write(pending.join("MEMORY.md"), "- **k**: v\n")
            .await
            .unwrap();
        fs::write(pending.join("BACKUP_COMPLETE"), "")
            .await
            .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.memory.backend, "none");
        assert!(
            !final_workspace.join("memory").join("brain.db").exists(),
            "the sweep must not run at all when the backend is not sqlite"
        );
        assert!(
            !pending.join("IMPORTED").exists(),
            "a pending backup must stay pending when the backend is not sqlite"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// A second `load_or_init` right after the one that imported markdown
    /// notes must not touch `brain.db` or `MEMORY.md` again: the config is
    /// already `sqlite` (so `migrated` is false), and the retry sweep must
    /// see the `IMPORTED` marker and skip the finished backup.
    #[test]
    async fn load_or_init_twice_leaves_imported_memory_unchanged() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();
        fs::write(
            &config_path,
            "schema_version = 33\n\n[memory]\nbackend = \"markdown\"\n",
        )
        .await
        .unwrap();

        let final_workspace = workspace_dir.join("workspace");
        fs::create_dir_all(&final_workspace).await.unwrap();
        fs::write(final_workspace.join("MEMORY.md"), "- **k**: v\n")
            .await
            .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let _first = Config::load_or_init().await.unwrap();

        let db_path = final_workspace.join("memory").join("brain.db");
        let count_after_first: i64 = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
                .unwrap()
        };
        let memory_md_after_first = fs::read_to_string(final_workspace.join("MEMORY.md"))
            .await
            .unwrap();
        assert_eq!(
            count_after_first, 1,
            "the markdown entry must be imported once"
        );

        // Simulate the operator deleting the imported memory afterward (e.g.
        // via `memory_forget`). The backup is marked IMPORTED after the
        // first load, so a correct retry sweep must never touch it again —
        // in particular, it must not resurrect a row the operator removed.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("DELETE FROM memories WHERE key = 'k'", [])
                .unwrap();
        }

        let _second = Config::load_or_init().await.unwrap();

        let count_after_second: i64 = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
                .unwrap()
        };
        let memory_md_after_second = fs::read_to_string(final_workspace.join("MEMORY.md"))
            .await
            .unwrap();

        assert_eq!(
            count_after_second, 0,
            "a second load must not retry a finished import and resurrect a deleted entry"
        );
        assert_eq!(
            memory_md_after_first, memory_md_after_second,
            "MEMORY.md must not change on a second load"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// If the markdown backup fails, the migrated config must NOT be
    /// written to disk: the config still loads as sqlite in memory (the
    /// daemon still starts), but the on-disk file keeps naming `markdown`
    /// so the next start's `migrate()` retries the whole bump — backup
    /// included — from scratch, via the same on-disk-state retry path an
    /// ordinary write-back failure already uses, rather than a new config
    /// key. No import must run either.
    #[test]
    async fn load_or_init_keeps_a_markdown_config_on_disk_when_the_backup_fails() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();
        fs::write(
            &config_path,
            "schema_version = 33\n\n[memory]\nbackend = \"markdown\"\n",
        )
        .await
        .unwrap();

        let final_workspace = workspace_dir.join("workspace");
        fs::create_dir_all(&final_workspace).await.unwrap();
        fs::write(final_workspace.join("MEMORY.md"), "- **k**: v\n")
            .await
            .unwrap();
        // Force `backup_markdown_memory` to fail deterministically: occupy
        // `memory/migrations` with a plain file so `create_dir_all` for the
        // backup directory cannot succeed.
        fs::create_dir_all(final_workspace.join("memory"))
            .await
            .unwrap();
        fs::write(
            final_workspace.join("memory").join("migrations"),
            "not a directory",
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let config = Config::load_or_init().await.unwrap();

        // The daemon still starts, with the config usable as sqlite for
        // this session.
        assert_eq!(config.memory.backend, "sqlite");

        // But the config on disk was not advanced.
        let on_disk = fs::read_to_string(&config_path).await.unwrap();
        assert!(
            on_disk.contains("markdown"),
            "the config on disk must stay markdown when the backup fails: {on_disk}"
        );
        assert!(
            !final_workspace.join("memory").join("brain.db").exists(),
            "no import must have run when the backup failed"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// A markdown backup failure holds back the credential strip too, not
    /// only the schema bump: `raw` is fully migrated to the current schema
    /// (sqlite) either way, so writing it back because of the credential
    /// strip alone would silently advance the on-disk schema and close off
    /// the only signal that makes the gate retry the backup — the
    /// operator's markdown notes would then never be imported at all.
    #[test]
    async fn load_or_init_does_not_strip_the_credential_when_the_backup_fails() {
        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();
        fs::write(
            &config_path,
            "schema_version = 33\n\
             api_url = \"sk-EXAMPLEKEY1234567890\"\n\n\
             [memory]\nbackend = \"markdown\"\n",
        )
        .await
        .unwrap();

        let final_workspace = workspace_dir.join("workspace");
        fs::create_dir_all(&final_workspace).await.unwrap();
        fs::write(final_workspace.join("MEMORY.md"), "- **k**: v\n")
            .await
            .unwrap();
        // Force `backup_markdown_memory` to fail deterministically: occupy
        // `memory/migrations` with a plain file so `create_dir_all` for the
        // backup directory cannot succeed.
        fs::create_dir_all(final_workspace.join("memory"))
            .await
            .unwrap();
        fs::write(
            final_workspace.join("memory").join("migrations"),
            "not a directory",
        )
        .await
        .unwrap();

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let _config = Config::load_or_init().await.unwrap();

        let on_disk = fs::read_to_string(&config_path).await.unwrap();
        assert!(
            on_disk.contains("markdown"),
            "the schema must not advance when the backup fails: {on_disk}"
        );
        assert!(
            on_disk.contains("sk-EXAMPLEKEY1234567890"),
            "the credential strip must not be persisted either while the backup fails, or \
             the schema bump above would be unreachable next time: {on_disk}"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// If the config write-back keeps failing (a read-only config
    /// directory, say), the on-disk schema never advances off `markdown`,
    /// so `migrated && markdown_pre_migration` is true again on every
    /// start. A second such start must NOT make a second complete backup
    /// and re-import from it — that would re-apply markdown-wins over rows
    /// the operator has since deleted, forever, on every restart.
    #[cfg(unix)]
    #[test]
    async fn load_or_init_makes_only_one_backup_when_the_write_back_keeps_failing() {
        use std::os::unix::fs::PermissionsExt;

        let _env_guard = env_override_lock().await;
        let temp_home =
            std::env::temp_dir().join(format!("rantaiclaw_test_home_{}", uuid::Uuid::new_v4()));
        let workspace_dir = temp_home.join("profile-a");
        let config_path = workspace_dir.join("config.toml");
        fs::create_dir_all(&workspace_dir).await.unwrap();
        fs::write(
            &config_path,
            "schema_version = 33\n\n[memory]\nbackend = \"markdown\"\n",
        )
        .await
        .unwrap();

        let final_workspace = workspace_dir.join("workspace");
        fs::create_dir_all(&final_workspace).await.unwrap();
        fs::write(final_workspace.join("MEMORY.md"), "- **k**: v\n")
            .await
            .unwrap();

        // `atomic_write_config` creates its temp file directly inside
        // `config_path`'s parent (`workspace_dir`), so removing write
        // permission there makes every write-back fail, deterministically,
        // without touching `final_workspace` (a subdirectory, unaffected).
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let probe = workspace_dir.join("probe-write-access");
        if std::fs::write(&probe, "x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o755))
                .unwrap();
            eprintln!(
                "skipping load_or_init_makes_only_one_backup_when_the_write_back_keeps_failing: \
                 running as root, chmod does not restrict writes here"
            );
            let _ = fs::remove_dir_all(temp_home).await;
            return;
        }

        let _g_home = crate::test_env::EnvGuard::set("HOME", &temp_home);
        let _g_config_dir = crate::test_env::EnvGuard::unset("RANTAICLAW_CONFIG_DIR");
        let _g_workspace = crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &workspace_dir);

        let _first = Config::load_or_init().await.unwrap();

        let db_path = final_workspace.join("memory").join("brain.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("DELETE FROM memories WHERE key = 'k'", [])
                .unwrap();
        }

        // The backup directory name is `markdown-<second-granularity \
        // timestamp>-<pid>`. Both loads run in this same test process, so
        // without a real gap the second load would compute the exact same
        // name as the first and harmlessly overwrite it in place — masking
        // the bug this test exists to catch. Advancing the clock past a
        // second boundary is what makes the second load's backup attempt
        // (skipped by the fix, made anyway without it) land on a distinct
        // directory, the same way a real restart minutes or days later would.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        let _second = Config::load_or_init().await.unwrap();

        // Restore permissions before any assertion, so the tempdir is
        // always cleanable even if an assertion below panics.
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let count: i64 = {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.query_row("SELECT COUNT(*) FROM memories WHERE key = 'k'", [], |row| {
                row.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            count, 0,
            "the deleted row must not be resurrected by a repeated backup+import"
        );

        let migrations_dir = final_workspace.join("memory").join("migrations");
        let backup_count = std::fs::read_dir(&migrations_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("markdown-"))
            .count();
        assert_eq!(
            backup_count, 1,
            "exactly one backup directory must exist after two loads with a failing write-back"
        );

        let _ = fs::remove_dir_all(temp_home).await;
    }

    /// A config that never touched `[memory].backend` but overrode the
    /// backend through `[storage.provider.config].provider = "postgres"`
    /// really did run against Postgres before the migration: that override
    /// took effect at runtime. The pre-migration postgres WARN must fire for
    /// it too, not only for a `[memory].backend = "postgres"` config.
    #[test]
    async fn raw_memory_backend_was_postgres_detects_storage_override() {
        let raw: toml::Value =
            toml::from_str("[storage.provider.config]\nprovider = \"postgres\"\n").unwrap();
        assert!(
            raw_memory_backend_was_postgres(&raw),
            "a postgres storage override must be detected even when [memory].backend is unset"
        );
    }

    // ── markdown import: newer rows, place, prose, pending retry ──────

    /// Captures everything logged while the returned guard is alive.
    #[derive(Clone, Default)]
    struct LogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogBuffer {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer lock").clone()).expect("utf-8 log")
        }
    }

    fn capture_logs() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        (buffer, tracing::subscriber::set_default(subscriber))
    }

    /// A `load_or_init` sandbox. `HOME`, `RANTAICLAW_CONFIG_DIR` and
    /// `RANTAICLAW_WORKSPACE` all point under one temp dir, so nothing the
    /// load, the import or a save writes can reach the operator's real
    /// config tree. The caller holds `env_override_lock()` for as long as the
    /// sandbox lives.
    struct LoadSandbox {
        _env: [crate::test_env::EnvGuard; 3],
        config_dir: PathBuf,
        workspace: PathBuf,
        _root: tempfile::TempDir,
    }

    fn load_sandbox(config_toml: &str) -> LoadSandbox {
        let root = tempfile::TempDir::new().unwrap();
        let config_dir = root.path().join("profile-a");
        let workspace = config_dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(config_dir.join("config.toml"), config_toml).unwrap();
        let env = [
            crate::test_env::EnvGuard::set("HOME", root.path()),
            crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", &config_dir),
            crate::test_env::EnvGuard::set("RANTAICLAW_WORKSPACE", &config_dir),
        ];
        LoadSandbox {
            _env: env,
            config_dir,
            workspace,
            _root: root,
        }
    }

    /// Open (creating) `brain.db` under `workspace` with the backend's schema.
    fn open_brain_db(workspace: &Path) -> rusqlite::Connection {
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        let conn = rusqlite::Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        crate::memory::SqliteMemory::init_schema(&conn).unwrap();
        conn
    }

    /// Store `key` the way the runtime does: stamped with the current time.
    fn store_runtime_row(workspace: &Path, key: &str, content: &str) {
        let conn = open_brain_db(workspace);
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at) \
             VALUES (?1, ?2, ?3, 'core', ?4, ?4)",
            rusqlite::params![uuid::Uuid::new_v4().to_string(), key, content, now],
        )
        .unwrap();
    }

    fn brain_db_content(workspace: &Path, key: &str) -> Option<String> {
        let conn = rusqlite::Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        conn.query_row(
            "SELECT content FROM memories WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        )
        .ok()
    }

    fn pending_marker(workspace: &Path) -> PathBuf {
        workspace.join("memory").join("migrations").join("PENDING")
    }

    fn config_backend_on_disk(config_dir: &Path) -> String {
        let on_disk: toml::Value =
            toml::from_str(&std::fs::read_to_string(config_dir.join("config.toml")).unwrap())
                .unwrap();
        on_disk["memory"]["backend"].as_str().unwrap().to_string()
    }

    /// The scenario: the backup is complete but the first import failed, then
    /// the daemon runs on sqlite and stores a new value under a key the
    /// markdown also holds. The retry must not put the old markdown value back.
    #[tokio::test]
    async fn retry_keeps_a_value_stored_after_the_backup_and_counts_the_skip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(
            workspace.join("MEMORY.md"),
            "- **k**: stale markdown text\n- **only_in_markdown**: from markdown\n",
        )
        .unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();

        // First import fails: a directory where brain.db must be opened.
        let db_path = workspace.join("memory").join("brain.db");
        std::fs::create_dir(&db_path).unwrap();
        assert!(super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).is_err());
        assert!(!backup_dir.join("IMPORTED").exists());
        std::fs::remove_dir(&db_path).unwrap();

        // The daemon starts on sqlite and stores a new value for `k`.
        std::thread::sleep(std::time::Duration::from_millis(50));
        store_runtime_row(&workspace, "k", "fresh runtime text");

        let (logs, _guard) = capture_logs();
        super::retry_unimported_markdown_imports(&workspace, false);

        assert_eq!(
            brain_db_content(&workspace, "k").as_deref(),
            Some("fresh runtime text"),
            "a retry must not overwrite a value stored after the backup"
        );
        assert_eq!(
            brain_db_content(&workspace, "only_in_markdown").as_deref(),
            Some("from markdown"),
            "the rest of the backup is still imported"
        );
        assert!(backup_dir.join("IMPORTED").exists());
        let logged = logs.contents();
        assert!(
            logged.contains("skipped_newer=1"),
            "the INFO line must count the row kept as newer: {logged}"
        );
        assert!(
            !logged.contains("stale markdown text") && !logged.contains("fresh runtime text"),
            "the log must carry the count only, never a memory value: {logged}"
        );
    }

    /// An empty `BACKUP_COMPLETE` (a backup made by the build that wrote no
    /// timestamp) falls back to the marker's mtime as the backup time.
    #[tokio::test]
    async fn an_empty_backup_marker_falls_back_to_its_mtime_for_the_newer_row_guard() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let backup_dir = workspace
            .join("memory")
            .join("migrations")
            .join("markdown-20200101-000000-1");
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(
            backup_dir.join("MEMORY.md"),
            "- **k**: old markdown\n- **old_key**: markdown wins\n",
        )
        .unwrap();
        let marker = backup_dir.join("BACKUP_COMPLETE");
        std::fs::write(&marker, "").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800))
            .unwrap();
        store_runtime_row(&workspace, "k", "kept");
        // Older than the marker's mtime: the import must replace it. With a
        // fallback to the epoch every row would count as newer and stay.
        open_brain_db(&workspace)
            .execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at) \
                 VALUES ('id-old', 'old_key', 'stale', 'core', \
                         '2019-06-01T00:00:00+00:00', '2019-06-01T00:00:00+00:00')",
                [],
            )
            .unwrap();

        super::retry_unimported_markdown_imports(&workspace, false);

        assert_eq!(
            brain_db_content(&workspace, "k").as_deref(),
            Some("kept"),
            "a row newer than the marker's mtime must survive the import"
        );
        assert_eq!(
            brain_db_content(&workspace, "old_key").as_deref(),
            Some("markdown wins"),
            "a row older than the marker's mtime must be replaced by the note"
        );
    }

    /// A row in a conversation's place that the operator's note lands on
    /// becomes the operator's note: no longer scoped to the conversation, and
    /// its vector (computed for the old text) is dropped for `reindex` to
    /// rebuild.
    #[tokio::test]
    async fn import_moves_a_conversation_row_to_the_operators_place_and_drops_its_vector() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);
        {
            let conn = open_brain_db(&workspace);
            conn.execute(
                "INSERT INTO memories (id, key, content, category, embedding, created_at, \
                     updated_at, session_id, embedding_model, embedding_dims) \
                 VALUES ('id-conv', 'user_lang', 'text of another conversation', 'conversation', \
                     X'0102', '2020-01-01T00:00:00+00:00', '2020-01-01T00:00:00+00:00', \
                     'conv-a', 'stub-embedder', 2)",
                [],
            )
            .unwrap();
        }

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let conn = rusqlite::Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let (content, category): (String, String) = conn
            .query_row(
                "SELECT content, category FROM memories WHERE key = 'user_lang'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(content, "prefers Rust");
        assert_eq!(category, "core");
        let scoped_or_embedded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE key = 'user_lang' \
                 AND session_id IS NULL AND embedding IS NULL \
                 AND embedding_model IS NULL AND embedding_dims IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            scoped_or_embedded, 1,
            "the imported note must have no session and no stale vector"
        );
    }

    /// The backup holds daily files but no `MEMORY.md`. In the session that
    /// follows a failed import, the runtime creates `MEMORY.md` and the
    /// operator writes prose outside the projection block. The retry cannot
    /// compare that file against a copy, so it must not rewrite it.
    #[tokio::test]
    async fn import_keeps_operator_prose_when_the_backup_had_no_memory_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(
            workspace.join("memory").join("2026-09-01.md"),
            "- **morning_mood**: curious\n",
        )
        .unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        assert!(!backup_dir.join("MEMORY.md").exists());

        let begin = crate::memory::snapshot::PROJECTION_BEGIN;
        let end = crate::memory::snapshot::PROJECTION_END;
        std::fs::write(
            workspace.join("MEMORY.md"),
            format!("operator prose outside the markers\n\n{begin}\n- stale: block\n{end}\n"),
        )
        .unwrap();

        super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).unwrap();

        let after = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert!(
            after.contains("operator prose outside the markers"),
            "the operator's prose must survive the retry: {after}"
        );
        assert!(
            after.contains(begin) && after.contains(end),
            "the projection block is still refreshed: {after}"
        );
    }

    /// When a key is in `MEMORY.md` and in a daily file, the curated file
    /// wins: the daily files are read first, `MEMORY.md` last.
    #[tokio::test]
    async fn import_lets_memory_md_win_over_a_daily_file_for_the_same_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: curated value\n").unwrap();
        std::fs::write(
            workspace.join("memory").join("2026-09-01.md"),
            "- **k**: daily value\n",
        )
        .unwrap();

        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        assert_eq!(
            brain_db_content(&workspace, "k").as_deref(),
            Some("curated value")
        );
    }

    /// The sweep lists pending backups oldest first, whatever order the
    /// directory listing returns them in. The list is built from an explicit
    /// newest-first input so the result does not depend on `read_dir` order.
    #[tokio::test]
    async fn pending_markdown_backups_are_listed_oldest_first_whatever_the_input_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let migrations = tmp.path().join("memory").join("migrations");
        let make = |name: &str, imported: bool| {
            let dir = migrations.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("BACKUP_COMPLETE"), "").unwrap();
            if imported {
                std::fs::write(dir.join("IMPORTED"), "").unwrap();
            }
            dir
        };
        let newest = make("markdown-20260301-000000-1", false);
        let middle = make("markdown-20260201-000000-1", false);
        let oldest = make("markdown-20260101-000000-1", false);
        let finished = make("markdown-20250101-000000-1", true);
        let unrelated = migrations.join("other-20250101");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(unrelated.join("BACKUP_COMPLETE"), "").unwrap();

        let listed = super::sorted_pending_markdown_backups(vec![
            newest.clone(),
            finished,
            middle.clone(),
            unrelated,
            oldest.clone(),
        ]);

        assert_eq!(listed, vec![oldest, middle, newest]);
    }

    /// A workspace that never used markdown has no `memory/migrations/`; the
    /// sweep looks once and creates nothing.
    #[tokio::test]
    async fn retry_leaves_a_workspace_that_never_used_markdown_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        super::retry_unimported_markdown_imports(&workspace, false);

        assert!(
            !workspace.join("memory").exists(),
            "the sweep must not create anything for a workspace with no backup"
        );
    }

    /// The gate's skip decision: a live markdown file with notes in it that
    /// is newer than the backup is reported; a `MEMORY.md` the import itself
    /// rewrote to the projection block is not.
    #[tokio::test]
    async fn live_markdown_newer_than_backup_reports_notes_written_after_the_backup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        assert!(
            !super::live_markdown_newer_than_backup(&workspace, &backup_dir),
            "files untouched since the backup are not newer"
        );

        super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).unwrap();
        assert!(
            !super::live_markdown_newer_than_backup(&workspace, &backup_dir),
            "the import's own projection rewrite of MEMORY.md is not a newer note"
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            workspace.join("memory").join("2026-09-02.md"),
            "- **later_note**: written after the backup\n",
        )
        .unwrap();
        assert!(
            super::live_markdown_newer_than_backup(&workspace, &backup_dir),
            "a daily file with notes written after the backup must be reported"
        );
    }

    /// The gate skips the backup when a complete one exists, and says so when
    /// the live markdown files hold notes newer than that backup.
    #[tokio::test]
    async fn load_or_init_warns_when_it_skips_the_backup_but_markdown_notes_are_newer() {
        let _env_guard = env_override_lock().await;
        let sandbox = load_sandbox("schema_version = 33\n\n[memory]\nbackend = \"markdown\"\n");
        let workspace = &sandbox.workspace;
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: v\n").unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(workspace)
            .unwrap()
            .unwrap();
        std::fs::write(backup_dir.join("IMPORTED"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(
            workspace.join("memory").join("2026-09-02.md"),
            "- **later_note**: text written after the rollback\n",
        )
        .unwrap();

        let (logs, _guard) = capture_logs();
        let _config = Config::load_or_init().await.unwrap();

        let logged = logs.contents();
        assert!(
            logged.contains("were not imported"),
            "the skip must warn that newer notes were not imported: {logged}"
        );
        assert!(
            logged.contains(&backup_dir.display().to_string()),
            "the warning must name the backup: {logged}"
        );
        assert!(
            !logged.contains("text written after the rollback"),
            "the warning must not carry note text: {logged}"
        );
    }

    /// The backup fails, then the daemon saves the migrated config in the same
    /// session. Without a marker the next start sees sqlite, ignores the
    /// partial directory and forgets the notes. With the marker the next load
    /// backs up and imports from the live files, keeps what the runtime stored
    /// meanwhile, and removes the marker.
    #[tokio::test]
    async fn a_failed_backup_is_retried_after_a_config_save_in_the_same_session() {
        let _env_guard = env_override_lock().await;
        let sandbox = load_sandbox("schema_version = 33\n\n[memory]\nbackend = \"markdown\"\n");
        let workspace = &sandbox.workspace;
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(
            workspace.join("MEMORY.md"),
            "- **k**: old\n- **only_in_markdown**: from markdown\n",
        )
        .unwrap();
        // A brain.db that is not a database makes the `VACUUM INTO` step of the
        // backup fail after the markdown files were copied.
        let db_path = workspace.join("memory").join("brain.db");
        std::fs::write(&db_path, "not a database").unwrap();

        let config = Config::load_or_init().await.unwrap();
        assert_eq!(config.memory.backend, "sqlite");
        assert!(
            pending_marker(workspace).exists(),
            "a failed backup must leave a PENDING marker"
        );
        assert_eq!(config_backend_on_disk(&sandbox.config_dir), "markdown");

        // Pairing, `permissions add` or the TUI save the migrated config.
        config.save().await.unwrap();
        assert_eq!(config_backend_on_disk(&sandbox.config_dir), "sqlite");

        // The cause is gone and the daemon has run on sqlite: a fresh brain.db
        // holds a value stored after the failed backup.
        std::fs::remove_file(&db_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        store_runtime_row(workspace, "k", "new");

        let (logs, _guard) = capture_logs();
        let _second = Config::load_or_init().await.unwrap();

        assert_eq!(
            brain_db_content(workspace, "only_in_markdown").as_deref(),
            Some("from markdown"),
            "the next load must import the notes from the live files"
        );
        assert_eq!(
            brain_db_content(workspace, "k").as_deref(),
            Some("new"),
            "a value stored after the failed backup must not be overwritten"
        );
        assert!(
            !pending_marker(workspace).exists(),
            "the marker is removed once the import is done"
        );
        let logged = logs.contents();
        assert!(
            logged.contains("PENDING"),
            "each load with a pending import must warn with the marker path: {logged}"
        );
    }

    /// The pending retry does not depend on the backend the config names.
    #[tokio::test]
    async fn a_pending_markdown_import_is_retried_even_when_the_backend_is_not_sqlite() {
        let _env_guard = env_override_lock().await;
        let sandbox = load_sandbox(&format!(
            "schema_version = {}\n\n[memory]\nbackend = \"none\"\n",
            crate::config::migrations::CURRENT_VERSION
        ));
        let workspace = &sandbox.workspace;
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: from markdown\n").unwrap();
        std::fs::create_dir_all(workspace.join("memory").join("migrations")).unwrap();
        std::fs::write(pending_marker(workspace), "2020-01-01T00:00:00+00:00").unwrap();

        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.memory.backend, "none");
        assert_eq!(
            brain_db_content(workspace, "k").as_deref(),
            Some("from markdown")
        );
        assert!(!pending_marker(workspace).exists());
    }

    fn markdown_backup_dirs(workspace: &Path) -> Vec<String> {
        let Ok(read_dir) = std::fs::read_dir(workspace.join("memory").join("migrations")) else {
            return Vec::new();
        };
        read_dir
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("markdown-"))
            .collect()
    }

    /// The marker says a backup or an import is still owed. An import that
    /// failed leaves it in place, and only the retry that succeeds removes it.
    #[tokio::test]
    async fn a_failed_import_keeps_the_pending_marker_until_a_retry_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: from markdown\n").unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        crate::migration::record_pending_markdown_import(&workspace).unwrap();
        // A directory where brain.db must be opened makes the import fail.
        let db_path = workspace.join("memory").join("brain.db");
        std::fs::create_dir(&db_path).unwrap();

        super::retry_unimported_markdown_imports(&workspace, false);

        assert!(
            pending_marker(&workspace).exists(),
            "the marker must stay while the import keeps failing"
        );
        assert!(!backup_dir.join("IMPORTED").exists());

        std::fs::remove_dir(&db_path).unwrap();
        super::retry_unimported_markdown_imports(&workspace, false);

        assert!(backup_dir.join("IMPORTED").exists());
        assert!(!pending_marker(&workspace).exists());
        assert_eq!(
            brain_db_content(&workspace, "k").as_deref(),
            Some("from markdown")
        );
    }

    /// The markdown files are gone by the time the owed backup retries: there
    /// is nothing left to back up, so the marker has nothing left to say.
    #[tokio::test]
    async fn a_pending_marker_is_removed_when_no_markdown_files_are_left_to_back_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory").join("migrations")).unwrap();
        std::fs::write(pending_marker(&workspace), "2020-01-01T00:00:00+00:00").unwrap();

        super::retry_unimported_markdown_imports(&workspace, false);

        assert!(!pending_marker(&workspace).exists());
        assert!(markdown_backup_dirs(&workspace).is_empty());
    }

    /// A load whose backup failed knows it: the sweep in the same load does not
    /// run the same backup a second time.
    #[tokio::test]
    async fn the_sweep_makes_no_backup_of_its_own_after_the_load_backup_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: from markdown\n").unwrap();
        std::fs::create_dir_all(workspace.join("memory").join("migrations")).unwrap();
        std::fs::write(pending_marker(&workspace), "2020-01-01T00:00:00+00:00").unwrap();

        super::retry_unimported_markdown_imports(&workspace, true);

        assert!(markdown_backup_dirs(&workspace).is_empty());
        assert!(pending_marker(&workspace).exists());
        assert!(!workspace.join("memory").join("brain.db").exists());
    }

    /// A load whose backup fails leaves no partial directory, and the sweep in
    /// the same load does not try the backup again.
    #[tokio::test]
    async fn a_load_whose_backup_fails_makes_one_attempt_and_leaves_no_partial_directory() {
        let _env_guard = env_override_lock().await;
        let sandbox = load_sandbox("schema_version = 33\n\n[memory]\nbackend = \"markdown\"\n");
        let workspace = &sandbox.workspace;
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("MEMORY.md"), "- **k**: from markdown\n").unwrap();
        // A brain.db that is not a database makes the `VACUUM INTO` step fail
        // after the markdown files were copied.
        std::fs::write(workspace.join("memory").join("brain.db"), "not a database").unwrap();

        let (logs, _guard) = capture_logs();
        let config = Config::load_or_init().await.unwrap();

        assert_eq!(config.memory.backend, "sqlite");
        assert!(pending_marker(workspace).exists());
        assert!(
            markdown_backup_dirs(workspace).is_empty(),
            "a failed backup must not leave its directory: {:?}",
            markdown_backup_dirs(workspace)
        );
        let logged = logs.contents();
        assert_eq!(
            logged.matches("failed to back up markdown memory").count(),
            1,
            "the load makes exactly one backup attempt: {logged}"
        );
        assert!(
            !logged.contains("backup failed again"),
            "the sweep must not repeat the backup the load just failed: {logged}"
        );
    }

    /// The import commits under `synchronous = FULL`, so the checkpoint after
    /// it is a courtesy. A reader that keeps a snapshot open makes it report
    /// busy, and the import that already committed must still be marked done:
    /// otherwise the next start imports again and brings back every row the
    /// operator deleted in between.
    #[tokio::test]
    async fn a_busy_wal_checkpoint_does_not_fail_an_import_that_committed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::write(
            workspace.join("MEMORY.md"),
            "- **k**: from markdown\n- **gone**: deleted later\n",
        )
        .unwrap();
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();

        // A second connection in WAL mode with a read transaction left open.
        let reader = open_brain_db(&workspace);
        reader.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        store_runtime_row(&workspace, "existing", "row");
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM memories;")
            .unwrap();

        super::import_markdown_backup_into_sqlite(&workspace, &backup_dir)
            .expect("a busy checkpoint must not fail the import");
        assert!(backup_dir.join("IMPORTED").exists());
        reader.execute_batch("COMMIT;").unwrap();
        drop(reader);

        // The operator deletes a row; a second load must not bring it back.
        open_brain_db(&workspace)
            .execute("DELETE FROM memories WHERE key = 'gone'", [])
            .unwrap();
        super::retry_unimported_markdown_imports(&workspace, false);

        assert_eq!(brain_db_content(&workspace, "gone"), None);
        assert_eq!(
            brain_db_content(&workspace, "k").as_deref(),
            Some("from markdown")
        );
    }

    /// The runtime keeps a key in the place that stored it, but the import is
    /// the one exception: the operator's note wins over a conversation's row
    /// under the same key, even a newer one, and the row moves to the shared
    /// place. The INFO line counts those moves.
    #[tokio::test]
    async fn import_overwrites_and_moves_a_newer_row_held_by_another_conversation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);
        open_brain_db(&workspace)
            .execute(
                "INSERT INTO memories (id, key, content, category, created_at, updated_at, \
                     session_id) \
                 VALUES ('id-conv', 'user_lang', 'text of another conversation', \
                     'conversation', '2999-01-01T00:00:00+00:00', '2999-01-01T00:00:00+00:00', \
                     'conv-a')",
                [],
            )
            .unwrap();

        let (logs, _guard) = capture_logs();
        super::import_markdown_memory_into_sqlite(&workspace).unwrap();

        let conn = rusqlite::Connection::open(workspace.join("memory").join("brain.db")).unwrap();
        let (content, session): (String, Option<String>) = conn
            .query_row(
                "SELECT content, session_id FROM memories WHERE key = 'user_lang'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(content, "prefers Rust");
        assert_eq!(session, None, "the row moves to the shared place");
        let memory_md = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert!(
            memory_md.contains("- user_lang: prefers Rust"),
            "the operator's line stays in MEMORY.md: {memory_md}"
        );
        let logged = logs.contents();
        assert!(
            logged.contains("moved_from_other_place=1") && logged.contains("skipped_newer=0"),
            "the INFO line counts the move and no skip: {logged}"
        );
    }

    /// A key kept because a newer shared row holds it is still only in the
    /// operator's `MEMORY.md`. Rewriting that file to the projection block
    /// would drop the line for good, so the rewrite is skipped.
    #[tokio::test]
    async fn import_keeps_memory_md_lines_when_a_key_was_skipped_as_newer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = markdown_workspace(&tmp);
        let backup_dir = crate::migration::backup_markdown_memory(&workspace)
            .unwrap()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        store_runtime_row(&workspace, "user_lang", "fresh runtime text");

        super::import_markdown_backup_into_sqlite(&workspace, &backup_dir).unwrap();

        assert_eq!(
            brain_db_content(&workspace, "user_lang").as_deref(),
            Some("fresh runtime text")
        );
        let memory_md = std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap();
        assert!(
            memory_md.contains("- **user_lang**: prefers Rust"),
            "the operator's line must stay in MEMORY.md: {memory_md}"
        );
    }
}
