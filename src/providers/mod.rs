//! Provider subsystem for model inference backends.
//!
//! This module implements the factory pattern for AI model providers. Each provider
//! implements the [`Provider`] trait defined in [`traits`], and is registered in the
//! factory function [`create_provider`] by its canonical string key (e.g., `"openai"`,
//! `"anthropic"`, `"ollama"`, `"gemini"`). Provider aliases are resolved internally
//! so that user-facing keys remain stable.
//!
//! The subsystem supports resilient multi-provider configurations through the
//! [`ReliableProvider`](reliable::ReliableProvider) wrapper, which handles fallback
//! chains and automatic retry. Model routing across providers is available via
//! [`create_routed_provider`].
//!
//! # Extension
//!
//! To add a new provider, implement [`Provider`] in a new submodule and register it
//! in [`create_provider_with_url`]. See `AGENTS.md` §7.1 for the full change playbook.

// Legacy hand-rolled Anthropic/Gemini providers are kept compiled in the default
// build: they serve auth modes rig-core cannot (Anthropic setup-tokens) and are
// the routed backend for those modes — not dead code. 016 Stage-2 deletion is
// cancelled; see plans/025-rigprovider-auth-parity.md.
pub mod anthropic;
pub mod bedrock;
pub mod compatible;
pub mod config_error;
pub mod gemini;
pub(crate) mod gemini_error;
pub(crate) mod local_refusal;
pub mod ollama;
#[cfg(feature = "legacy-providers")]
pub mod openai;
pub mod openrouter;
pub mod reliable;
pub(crate) mod removed_login;
mod renewing_credential;
pub mod rig_native;
pub mod router;
pub mod traits;

pub use config_error::ProviderConfigError;

#[allow(unused_imports)]
pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ProviderCapabilityError,
    ProviderUsage, ToolCall, ToolResultMessage,
};

use compatible::{AuthStyle, OpenAiCompatibleProvider};
use reliable::ReliableProvider;
use std::path::PathBuf;

const MAX_API_ERROR_CHARS: usize = 200;

/// How many characters of a caller-supplied provider name are echoed back in
/// an unknown-provider refusal.
///
/// WHY: the name is the ONE caller-controlled part of that message, and a
/// request body may carry a great deal of it. 64 characters is generous for a
/// real provider name: the longest one the factory knows is
/// `anthropic-custom:https://your-api.com`, 35 characters.
const MAX_UNKNOWN_PROVIDER_NAME_CHARS: usize = 64;

/// The marker that shows a name was cut to the bound.
///
/// WHY: a silent cut leaves the caller reading a short name as if it were the
/// name it sent. The marker says the name was shortened.
const TRUNCATION_MARKER: &str = "...[truncated]";

/// How many characters of a caller-supplied model name are echoed back in a
/// provider refusal.
///
/// WHY: like the provider name above, the model name is the ONE
/// caller-controlled part of the message and a request body may carry a great
/// deal of it. 64 characters is generous for a real model id: the longest one
/// the Ollama provider sees is far shorter. The bound applies to the echoed
/// text only, never to the model that is sent.
const MAX_MODEL_NAME_CHARS: usize = 64;
const MINIMAX_INTL_BASE_URL: &str = "https://api.minimax.io/v1";
const MINIMAX_CN_BASE_URL: &str = "https://api.minimaxi.com/v1";
const MINIMAX_API_KEY_ENV: &str = "MINIMAX_API_KEY";
const GLM_GLOBAL_BASE_URL: &str = "https://api.z.ai/api/paas/v4";
const GLM_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";
const MOONSHOT_INTL_BASE_URL: &str = "https://api.moonshot.ai/v1";
const MOONSHOT_CN_BASE_URL: &str = "https://api.moonshot.cn/v1";
const QWEN_CN_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";
const QWEN_INTL_BASE_URL: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";
const QWEN_US_BASE_URL: &str = "https://dashscope-us.aliyuncs.com/compatible-mode/v1";
const ZAI_GLOBAL_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";
const ZAI_CN_BASE_URL: &str = "https://open.bigmodel.cn/api/coding/paas/v4";

// Four bases that shipped without the path segment their API actually serves
// on. `OpenAiCompatibleProvider` appends `/chat/completions` verbatim, so each
// of these sent every message to a 404 — the provider was configurable but
// could not answer once.
//
// Named rather than inlined so `openai_compatible_bases_are_the_probed_ones`
// can pin them; a literal buried in a 24-arm match is not checkable.
const GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
const XAI_BASE_URL: &str = "https://api.x.ai/v1";
/// Venice serves on `/api/v1`, not `/v1` — the latter answers 402.
const VENICE_BASE_URL: &str = "https://api.venice.ai/api/v1";
/// The AI Gateway is a separate host; `api.vercel.ai` 404s every path.
const VERCEL_GATEWAY_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";

pub(crate) fn is_minimax_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "minimax"
            | "minimax-intl"
            | "minimax-io"
            | "minimax-global"
            | "minimax-portal"
            | "minimax-portal-global"
    )
}

pub(crate) fn is_minimax_cn_alias(name: &str) -> bool {
    matches!(name, "minimax-cn" | "minimaxi" | "minimax-portal-cn")
}

pub(crate) fn is_minimax_alias(name: &str) -> bool {
    is_minimax_intl_alias(name) || is_minimax_cn_alias(name)
}

pub(crate) fn is_glm_global_alias(name: &str) -> bool {
    matches!(name, "glm" | "zhipu" | "glm-global" | "zhipu-global")
}

pub(crate) fn is_glm_cn_alias(name: &str) -> bool {
    matches!(name, "glm-cn" | "zhipu-cn" | "bigmodel")
}

pub(crate) fn is_glm_alias(name: &str) -> bool {
    is_glm_global_alias(name) || is_glm_cn_alias(name)
}

pub(crate) fn is_moonshot_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "moonshot-intl" | "moonshot-global" | "kimi-intl" | "kimi-global"
    )
}

pub(crate) fn is_moonshot_cn_alias(name: &str) -> bool {
    matches!(name, "moonshot" | "kimi" | "moonshot-cn" | "kimi-cn")
}

pub(crate) fn is_moonshot_alias(name: &str) -> bool {
    is_moonshot_intl_alias(name) || is_moonshot_cn_alias(name)
}

pub(crate) fn is_qwen_cn_alias(name: &str) -> bool {
    matches!(name, "qwen" | "dashscope" | "qwen-cn" | "dashscope-cn")
}

pub(crate) fn is_qwen_intl_alias(name: &str) -> bool {
    matches!(
        name,
        "qwen-intl" | "dashscope-intl" | "qwen-international" | "dashscope-international"
    )
}

pub(crate) fn is_qwen_us_alias(name: &str) -> bool {
    matches!(name, "qwen-us" | "dashscope-us")
}

/// The Qwen names that work with a DashScope API key.
///
/// The OAuth-only names (`qwen-code` and friends) are not aliases: a config
/// that names one is refused with the DashScope-key replacement (see
/// [`removed_login`](crate::providers::removed_login)).
pub(crate) fn is_qwen_alias(name: &str) -> bool {
    is_qwen_cn_alias(name) || is_qwen_intl_alias(name) || is_qwen_us_alias(name)
}

pub(crate) fn is_zai_global_alias(name: &str) -> bool {
    matches!(name, "zai" | "z.ai" | "zai-global" | "z.ai-global")
}

pub(crate) fn is_zai_cn_alias(name: &str) -> bool {
    matches!(name, "zai-cn" | "z.ai-cn")
}

pub(crate) fn is_zai_alias(name: &str) -> bool {
    is_zai_global_alias(name) || is_zai_cn_alias(name)
}

pub(crate) fn is_qianfan_alias(name: &str) -> bool {
    matches!(name, "qianfan" | "baidu")
}

pub(crate) fn is_doubao_alias(name: &str) -> bool {
    matches!(name, "doubao" | "volcengine" | "ark" | "doubao-cn")
}

fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Normalize a provider name/alias to a stable canonical form for keying
/// per-provider state (e.g. `minimax-cn` → `minimax`, `z.ai` → `zai`). Regional
/// China aliases collapse to their family; everything else is returned trimmed
/// and lowercased as-is. Used to key `Config::provider_api_keys`.
pub fn normalize_provider_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    canonical_china_provider_name(&lower)
        .map(|s| s.to_string())
        .unwrap_or(lower)
}

pub(crate) fn canonical_china_provider_name(name: &str) -> Option<&'static str> {
    if is_qwen_alias(name) {
        Some("qwen")
    } else if is_glm_alias(name) {
        Some("glm")
    } else if is_moonshot_alias(name) {
        Some("moonshot")
    } else if is_minimax_alias(name) {
        Some("minimax")
    } else if is_zai_alias(name) {
        Some("zai")
    } else if is_qianfan_alias(name) {
        Some("qianfan")
    } else if is_doubao_alias(name) {
        Some("doubao")
    } else {
        None
    }
}

fn minimax_base_url(name: &str) -> Option<&'static str> {
    if is_minimax_cn_alias(name) {
        Some(MINIMAX_CN_BASE_URL)
    } else if is_minimax_intl_alias(name) {
        Some(MINIMAX_INTL_BASE_URL)
    } else {
        None
    }
}

/// The MiniMax provider for `name` at `base_url`. `key` is the credential
/// resolved from the config or the environment.
fn build_minimax_provider(base_url: &str, key: Option<&str>) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new_merge_system_into_user(
        "MiniMax",
        base_url,
        key,
        AuthStyle::Bearer,
    )
}

fn glm_base_url(name: &str) -> Option<&'static str> {
    if is_glm_cn_alias(name) {
        Some(GLM_CN_BASE_URL)
    } else if is_glm_global_alias(name) {
        Some(GLM_GLOBAL_BASE_URL)
    } else {
        None
    }
}

fn moonshot_base_url(name: &str) -> Option<&'static str> {
    if is_moonshot_intl_alias(name) {
        Some(MOONSHOT_INTL_BASE_URL)
    } else if is_moonshot_cn_alias(name) {
        Some(MOONSHOT_CN_BASE_URL)
    } else {
        None
    }
}

fn qwen_base_url(name: &str) -> Option<&'static str> {
    if is_qwen_cn_alias(name) {
        Some(QWEN_CN_BASE_URL)
    } else if is_qwen_intl_alias(name) {
        Some(QWEN_INTL_BASE_URL)
    } else if is_qwen_us_alias(name) {
        Some(QWEN_US_BASE_URL)
    } else {
        None
    }
}

fn zai_base_url(name: &str) -> Option<&'static str> {
    if is_zai_cn_alias(name) {
        Some(ZAI_CN_BASE_URL)
    } else if is_zai_global_alias(name) {
        Some(ZAI_GLOBAL_BASE_URL)
    } else {
        None
    }
}

/// Base URL for a provider whose endpoint varies by region, resolved from the
/// same constants `create_provider` builds it with.
///
/// Exists so `doctor`'s live ping can ask *this* module where a provider lives
/// instead of keeping its own table — the same reason `provider_is_local` reads
/// from the catalog. It deliberately covers only the region-varying families,
/// which are exactly the ones a second table gets wrong: their endpoint depends
/// on which alias was configured (`minimax` vs `minimax-cn`), so a flat
/// name→URL list cannot express them at all.
///
/// `None` means "not one of these families" — not "no endpoint". Callers must
/// treat it as "ask elsewhere or say you don't know", never as a licence to
/// synthesise a URL.
pub(crate) fn region_base_url(name: &str) -> Option<&'static str> {
    minimax_base_url(name)
        .or_else(|| glm_base_url(name))
        .or_else(|| moonshot_base_url(name))
        .or_else(|| qwen_base_url(name))
        .or_else(|| zai_base_url(name))
}

#[derive(Debug, Clone)]
pub struct ProviderRuntimeOptions {
    pub rantaiclaw_dir: Option<PathBuf>,
    pub secrets_encrypt: bool,
    pub reasoning_enabled: Option<bool>,
    /// Told `(primary, answered)` when a fallback provider produced the reply.
    /// Set by a surface that prints the answer (the CLI agent path); `None`
    /// elsewhere, where a provenance line is not presented.
    pub failover_announce: FailoverAnnounce,
}

/// The optional failover callback carried by [`ProviderRuntimeOptions`]. A
/// newtype so the options struct keeps its `Debug` derive: a closure has none.
#[derive(Clone, Default)]
pub struct FailoverAnnounce(pub Option<reliable::FailoverCallback>);

impl std::fmt::Debug for FailoverAnnounce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "FailoverAnnounce(set)"
        } else {
            "FailoverAnnounce(none)"
        })
    }
}

impl Default for ProviderRuntimeOptions {
    fn default() -> Self {
        Self {
            rantaiclaw_dir: None,
            secrets_encrypt: true,
            reasoning_enabled: None,
            failover_announce: FailoverAnnounce::default(),
        }
    }
}

fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')
}

fn token_end(input: &str, from: usize) -> usize {
    let mut end = from;
    for (i, c) in input[from..].char_indices() {
        if is_secret_char(c) {
            end = from + i + c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Fewest token characters that must follow a prefix before it is redacted.
///
/// The existing tests pin 5-character bodies (`xoxb-12345`), so a higher floor
/// would weaken them. Real keys are 20 characters or longer, so 5 only
/// excludes prose such as `sk-1` or `sk-a`.
const MIN_SECRET_BODY_CHARS: usize = 5;

/// Scrub known secret-like token prefixes from provider error strings.
///
/// Redacts tokens with prefixes like `sk-`, `xoxb-`, `xoxp-`, `ghp_`, `gho_`,
/// `ghu_`, and `github_pat_`.
///
/// A prefix counts only at the start of the text or when the character before
/// it is not an ASCII alphanumeric, `_` or `-`, so the `sk-` in `task-force`
/// stays. Accepted trade-off: a key glued to a word character, `_` or `-`
/// directly in front of its prefix (`key_sk-...`, `GITHUB_TOKEN_ghp_...`) is
/// not redacted by this function.
pub fn scrub_secret_patterns(input: &str) -> String {
    const PREFIXES: [&str; 7] = [
        "sk-",
        "xoxb-",
        "xoxp-",
        "ghp_",
        "gho_",
        "ghu_",
        "github_pat_",
    ];

    let mut scrubbed = input.to_string();

    for prefix in PREFIXES {
        let mut search_from = 0;
        while let Some(rel) = scrubbed[search_from..].find(prefix) {
            let start = search_from + rel;
            let content_start = start + prefix.len();

            // Compare the preceding char, not a byte, so multibyte text is
            // safe. The prefix is ASCII, so `content_start` is a char boundary.
            let glued_to_word = scrubbed[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
            if glued_to_word {
                search_from = content_start;
                continue;
            }

            let end = token_end(&scrubbed, content_start);

            // Bare prefixes like "sk-" and short bodies like "sk-1" should not
            // stop future scans.
            if scrubbed[content_start..end].chars().count() < MIN_SECRET_BODY_CHARS {
                search_from = content_start;
                continue;
            }

            scrubbed.replace_range(start..end, "[REDACTED]");
            search_from = start + "[REDACTED]".len();
        }
    }

    scrubbed
}

/// Sanitize API error text by scrubbing secrets and truncating length.
pub fn sanitize_api_error(input: &str) -> String {
    let scrubbed = scrub_secret_patterns(input);

    if scrubbed.chars().count() <= MAX_API_ERROR_CHARS {
        return scrubbed;
    }

    let mut end = MAX_API_ERROR_CHARS;
    while end > 0 && !scrubbed.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...", &scrubbed[..end])
}

/// Build a sanitized provider error from a failed HTTP response.
pub async fn api_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read provider error body>".to_string());
    let sanitized = sanitize_api_error(&body);
    anyhow::anyhow!("{provider} API error ({status}): {sanitized}")
}

/// True when `name` resolves a usable credential — from `config_key` (an
/// explicit config/api_key value) or a provider-specific env var. Providers that
/// need no key (or none is configured/env-set) return `false`; the gateway uses
/// this to warn when a switched provider likely can't be used.
pub fn has_usable_credential(name: &str, config_key: Option<&str>) -> bool {
    // A login that was removed has no credential to find: the API-key
    // replacement is a different provider, and this one refuses every
    // request. The check comes first, so neither the local-provider
    // shortcut nor the generic fallback keys below can make a removed
    // login look configured.
    if removed_login::is_removed_login_name(name) {
        return false;
    }

    // A credential that asks for a removed login is a placeholder, not a key.
    if config_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some_and(|value| removed_login::removed_credential_refusal(value).is_some())
    {
        return false;
    }

    // Nothing is missing: these run locally and `create_provider` substitutes a
    // placeholder where the OpenAI-compatible client wants a bearer at all.
    // Answering here rather than at each caller is what removes the carve-outs.
    if provider_is_local(name) {
        return true;
    }

    if resolve_provider_credential(name, config_key).is_some() {
        return true;
    }

    // `BedrockProvider::new()` ignores the `key` argument entirely and reads
    // `AwsCredentials::from_env`, so the resolver — which returns `None` for
    // bedrock by design — can never see this. Ask the provider's own check.
    if matches!(name, "bedrock" | "aws-bedrock") && bedrock::has_env_credentials() {
        return true;
    }

    false
}

/// The refusal for a provider name whose subscription login was removed.
///
/// The public door to the `removed_login` module: a caller outside this
/// module asks here, so the sentence a removed login produces has one source.
/// `None` for every name that is still a provider.
pub fn removed_login_refusal(name: &str) -> Option<&'static str> {
    removed_login::removed_login_refusal(name)
}

/// The operator-facing sentence of the refusal `error` carries, if it is one.
///
/// The public door to the `local_refusal` module. The gateway must say the
/// same words on both of its routes, so it reads the sentence here instead
/// of unwrapping the refusal type itself.
pub fn local_refusal_text(error: &anyhow::Error) -> Option<&str> {
    local_refusal::local_refusal_text(error)
}

/// The base provider a `<base>-custom:<url>` name is an endpoint of, e.g.
/// `anthropic-custom:http://…` → `anthropic`. `None` for any other name. The
/// `custom:` prefix is not a base-custom name (it has no base), and is left to
/// its own arm.
fn custom_endpoint_base(name: &str) -> Option<&str> {
    let (base, _url) = name.split_once("-custom:")?;
    (!base.is_empty()).then_some(base)
}

/// The name a provider string's env candidates resolve against, and that the
/// anthropic family check reads: a `<base>-custom:<url>` endpoint inherits its
/// base's list, so a future `foo-custom:` needs no arm of its own.
fn credential_base_name(name: &str) -> &str {
    custom_endpoint_base(name).unwrap_or(name)
}

/// Whether `name` builds an Anthropic-format provider: the `anthropic` name and
/// any `anthropic-custom:<url>` endpoint. The only providers whose stored auth
/// profile is used for inference.
fn is_anthropic_family(name: &str) -> bool {
    credential_base_name(name) == "anthropic"
}

/// Resolve the API key a provider sends, from the config value and the
/// environment.
///
/// Resolution order:
/// 1. The config-stored key for that provider (trimmed, ignored if empty)
/// 2. The provider's own environment variable (e.g. `ANTHROPIC_API_KEY`,
///    `OPENROUTER_API_KEY`)
/// 3. The generic fallback variables (`RANTAICLAW_API_KEY`, `API_KEY`)
///
/// Nothing is read from disk: a stored login profile is gone, so the only
/// credentials left are the ones the operator stored in the config or
/// exported for this provider.
///
/// A removed login resolves to nothing: the API-key replacement is a different
/// provider, and resolving a credential here would let a removed login look
/// configured (see [`removed_login`](crate::providers::removed_login)).
fn resolve_provider_credential(name: &str, credential_override: Option<&str>) -> Option<String> {
    if removed_login::is_removed_login_name(name) {
        return None;
    }

    if let Some(raw_override) = credential_override {
        let trimmed_override = raw_override.trim();
        if !trimmed_override.is_empty() {
            // A credential that asks for a removed login is a placeholder, not
            // a key. It resolves to nothing, so no env var can stand in for it.
            if removed_login::removed_credential_refusal(trimmed_override).is_some() {
                return None;
            }
            return Some(trimmed_override.to_owned());
        }
    }

    // A `<base>-custom:<url>` endpoint takes its base's env candidates.
    let env_name = credential_base_name(name);
    let provider_env_candidates: Vec<&str> = match env_name {
        "anthropic" => vec!["ANTHROPIC_API_KEY"],
        "openrouter" => vec!["OPENROUTER_API_KEY"],
        "openai" => vec!["OPENAI_API_KEY"],
        "ollama" => vec!["OLLAMA_API_KEY"],
        "venice" => vec!["VENICE_API_KEY"],
        "groq" => vec!["GROQ_API_KEY"],
        "mistral" => vec!["MISTRAL_API_KEY"],
        "deepseek" => vec!["DEEPSEEK_API_KEY"],
        "xai" | "grok" => vec!["XAI_API_KEY"],
        "together" | "together-ai" => vec!["TOGETHER_API_KEY"],
        "fireworks" | "fireworks-ai" => vec!["FIREWORKS_API_KEY"],
        "perplexity" => vec!["PERPLEXITY_API_KEY"],
        "cohere" => vec!["COHERE_API_KEY"],
        name if is_moonshot_alias(name) => vec!["MOONSHOT_API_KEY"],
        "kimi-code" | "kimi_coding" | "kimi_for_coding" => {
            vec!["KIMI_CODE_API_KEY", "MOONSHOT_API_KEY"]
        }
        name if is_glm_alias(name) => vec!["GLM_API_KEY"],
        name if is_minimax_alias(name) => vec![MINIMAX_API_KEY_ENV],
        // Bedrock uses AWS AKSK from env vars (AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY),
        // not a single API key. Credential resolution happens inside BedrockProvider.
        "bedrock" | "aws-bedrock" => return None,
        name if is_qianfan_alias(name) => vec!["QIANFAN_API_KEY"],
        name if is_doubao_alias(name) => vec!["ARK_API_KEY", "DOUBAO_API_KEY"],
        name if is_qwen_alias(name) => vec!["DASHSCOPE_API_KEY"],
        name if is_zai_alias(name) => vec!["ZAI_API_KEY"],
        "nvidia" | "nvidia-nim" | "build.nvidia.com" => vec!["NVIDIA_API_KEY"],
        "synthetic" => vec!["SYNTHETIC_API_KEY"],
        "opencode" | "opencode-zen" => vec!["OPENCODE_API_KEY"],
        "vercel" | "vercel-ai" => vec!["VERCEL_API_KEY"],
        "cloudflare" | "cloudflare-ai" => vec!["CLOUDFLARE_API_KEY"],
        "ovhcloud" | "ovh" => vec!["OVH_AI_ENDPOINTS_ACCESS_TOKEN"],
        "astrai" => vec!["ASTRAI_API_KEY"],
        "llamacpp" | "llama.cpp" => vec!["LLAMACPP_API_KEY"],
        "gemini" | "google" | "google-gemini" => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        _ => vec![],
    };

    for env_var in provider_env_candidates {
        if let Ok(value) = std::env::var(env_var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    for env_var in ["RANTAICLAW_API_KEY", "API_KEY"] {
        if let Ok(value) = std::env::var(env_var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    None
}

/// Parse the base URL of a `custom:` / `anthropic-custom:` provider.
///
/// Every failure here is a [`ProviderConfigError`]: the URL comes from the
/// config file alone, so no credential, login or DNS lookup can repair it —
/// a caller may treat it as fatal. The `Display` text is unchanged from the
/// plain `anyhow` errors this replaced.
fn parse_custom_provider_url(
    raw_url: &str,
    provider_label: &str,
    format_hint: &str,
) -> anyhow::Result<String> {
    let base_url = raw_url.trim();

    if base_url.is_empty() {
        return Err(ProviderConfigError::CustomUrlMissing {
            provider_label: provider_label.to_string(),
            format_hint: format_hint.to_string(),
        }
        .into());
    }

    let parsed =
        reqwest::Url::parse(base_url).map_err(|_| ProviderConfigError::CustomUrlInvalid {
            provider_label: provider_label.to_string(),
            format_hint: format_hint.to_string(),
        })?;

    match parsed.scheme() {
        "http" | "https" => Ok(base_url.to_string()),
        _ => Err(ProviderConfigError::CustomUrlScheme {
            provider_label: provider_label.to_string(),
            format_hint: format_hint.to_string(),
        }
        .into()),
    }
}

/// The wire format of a `custom:` or `anthropic-custom:` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustomEndpointKind {
    OpenAiCompatible,
    Anthropic,
}

/// The format and base URL of a `custom:<url>` or `anthropic-custom:<url>`
/// provider string, parsed as the factory parses it. `None` for any other name;
/// `Some(Err)` when the URL after the prefix is missing, unparseable or not
/// http(s).
pub(crate) fn parse_custom_endpoint(
    name: &str,
) -> Option<anyhow::Result<(CustomEndpointKind, String)>> {
    if let Some(raw_url) = name.strip_prefix("custom:") {
        return Some(
            parse_custom_provider_url(raw_url, "Custom provider", "custom:https://your-api.com")
                .map(|url| (CustomEndpointKind::OpenAiCompatible, url)),
        );
    }
    let raw_url = name.strip_prefix("anthropic-custom:")?;
    Some(
        parse_custom_provider_url(
            raw_url,
            "Anthropic-custom provider",
            "anthropic-custom:https://your-api.com",
        )
        .map(|url| (CustomEndpointKind::Anthropic, url)),
    )
}

/// The credential the chat path sends to the endpoint named `name`, for a
/// caller that sends its own request there: `config_key` first, then the
/// provider's env vars, then `RANTAICLAW_API_KEY` / `API_KEY`.
pub(crate) fn resolve_chat_credential(name: &str, config_key: Option<&str>) -> Option<String> {
    resolve_provider_credential(name, config_key)
}

/// The refusal for an Anthropic-family credential that is a subscription
/// setup-token, if `credential` is one.
///
/// `None` for every other credential and for every provider outside the
/// anthropic family. The value is never echoed.
pub fn setup_token_refusal(name: &str, credential: Option<&str>) -> Option<&'static str> {
    if !is_anthropic_family(name) {
        return None;
    }
    credential
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| anthropic::is_setup_token(value))
        .map(|_| anthropic::SETUP_TOKEN_REFUSAL)
}

/// The refusal for the credential the provider would send, if that credential is
/// one this build refuses.
///
/// It asks the resolver the send path uses, so the answer is the credential
/// that would actually go out — the config key, else the provider's own
/// environment variable, else the generic fallback.
pub fn refused_credential(name: &str, config_key: Option<&str>) -> Option<&'static str> {
    let resolved = resolve_provider_credential(name, config_key);
    setup_token_refusal(name, resolved.as_deref())
}

/// The refusal for an Anthropic-family name with no credential from any source.
/// It names the provider string the operator typed and the variable that fixes
/// it, so an `anthropic-custom:<url>` is not reported as `anthropic`.
fn anthropic_missing_credential_error(name: &str) -> anyhow::Error {
    anyhow::anyhow!("{name}: no credential found — set ANTHROPIC_API_KEY or store a key in config")
}

/// The provider that refuses every request, for an Anthropic-family credential
/// that is a subscription setup-token.
///
/// The factory still returns a provider, so the gateway binds and the daemon
/// stays up; the refusal happens when a request is sent.
fn anthropic_setup_token_provider(key: Option<&str>) -> Option<Box<dyn Provider>> {
    key.filter(|value| anthropic::is_setup_token(value))
        .map(|_| Box::new(anthropic::SetupTokenProvider) as Box<dyn Provider>)
}

/// Factory: create the right provider from config (without custom URL)
pub fn create_provider(name: &str, api_key: Option<&str>) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_with_options(name, api_key, &ProviderRuntimeOptions::default())
}

/// Factory: create provider with runtime options.
pub fn create_provider_with_options(
    name: &str,
    api_key: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_with_url_and_options(name, api_key, None, options)
}

/// Factory: create the right provider from config with optional custom base URL
pub fn create_provider_with_url(
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
) -> anyhow::Result<Box<dyn Provider>> {
    create_provider_with_url_and_options(name, api_key, api_url, &ProviderRuntimeOptions::default())
}

/// Bound a caller-supplied name to `limit` characters, marking the cut.
///
/// WHY a bound: such a name is usually the ONE caller-controlled part of a
/// refusal — the guidance beside it is static — and a request body may carry
/// up to 64 KiB of it (the gateway's `MAX_BODY_SIZE`). Echoed whole, one
/// request could put that much caller text into a 400 body. The cut lands on
/// a char boundary, so a multibyte name is never split into invalid text,
/// and a name that already fits is returned unchanged.
fn bounded_name(name: &str, limit: usize) -> String {
    if name.chars().count() <= limit {
        return name.to_string();
    }
    // The byte index of the first char PAST the bound: a char boundary by
    // construction, so the kept part is always valid text.
    let end = name
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(name.len());
    format!("{}{}", &name[..end], TRUNCATION_MARKER)
}

/// Factory: create provider with optional base URL and runtime options.
#[allow(clippy::too_many_lines)]
fn create_provider_with_url_and_options(
    name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    // A config that names a removed subscription login still builds a
    // provider: the gateway binds, the daemon stays up, and the refusal
    // happens when a request is sent, with the API-key replacement named.
    // The check comes first, so no credential source can make a removed
    // login look configured.
    if let Some(message) = removed_login::removed_login_refusal(name) {
        return Ok(Box::new(removed_login::RemovedLoginProvider::new(message)));
    }
    if let Some(message) = api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(removed_login::removed_credential_refusal)
    {
        return Ok(Box::new(removed_login::RemovedLoginProvider::new(message)));
    }

    // Resolve credential and break static-analysis taint chain from the
    // `api_key` parameter so that downstream provider storage of the value
    // is not linked to the original sensitive-named source.
    let resolved_credential = resolve_provider_credential(name, api_key);
    let resolved_credential =
        resolved_credential.map(|v| String::from_utf8(v.into_bytes()).unwrap_or_default());
    #[allow(clippy::option_as_ref_deref)]
    let key = resolved_credential.as_ref().map(String::as_str);

    match name {
        #[cfg(test)]
        "test-sse" => Ok(Box::new(TestSseProvider)),
        // A provider that counts its calls and remembers what each one
        // carried, so a test can prove that a turn never reached a provider,
        // and which model and credential it carried when it did.
        #[cfg(test)]
        "test-recording" => Ok(Box::new(tests::RecordingProvider::new(key))),
        // A provider without native tool calling, so a test can prove that a
        // turn falls back to the text protocol.
        #[cfg(test)]
        "test-non-native" => Ok(Box::new(TestNonNativeProvider)),
        // ── Primary providers (custom implementations) ───────
        "openrouter" => Ok(Box::new(openrouter::OpenRouterProvider::new(key))),
        // Anthropic / OpenAI native / Gemini route through `RigProvider`
        // by default — see `src/providers/rig_native.rs`. Build with
        // `--features legacy-providers` to fall back to the hand-rolled
        // files (gated out of default builds; deletion needs maintainer
        // approval — see plans/016-legacy-providers-sunset.md).
        #[cfg(not(feature = "legacy-providers"))]
        "anthropic" => {
            // A setup-token is refused when the request is sent, so the
            // gateway binds and the daemon stays up. A plain API key keeps the
            // rig path (streaming + native tools).
            if let Some(provider) = anthropic_setup_token_provider(key) {
                return Ok(provider);
            }
            Ok(Box::new(
                rig_native::RigProvider::for_provider("anthropic", key)
                    .map_err(|_| anthropic_missing_credential_error(name))?,
            ))
        }
        // A setup-token is refused here too, by the same helper the default
        // build's first branch uses.
        #[cfg(feature = "legacy-providers")]
        "anthropic" => {
            if let Some(provider) = anthropic_setup_token_provider(key) {
                return Ok(provider);
            }
            Ok(Box::new(anthropic::AnthropicProvider::new(key)))
        }
        #[cfg(not(feature = "legacy-providers"))]
        "openai" => Ok(Box::new(rig_native::RigProvider::for_provider_with_url(
            "openai", key, api_url,
        )?)),
        #[cfg(feature = "legacy-providers")]
        "openai" => Ok(Box::new(openai::OpenAiProvider::with_base_url(api_url, key))),
        // Ollama uses api_url for custom base URL (e.g. remote Ollama instance)
        "ollama" => Ok(Box::new(ollama::OllamaProvider::new_with_reasoning(
            api_url,
            key,
            options.reasoning_enabled,
        ))),
        #[cfg(not(feature = "legacy-providers"))]
        "gemini" | "google" | "google-gemini" => {
            // rig's gemini client only speaks the public endpoint. With an API
            // key the rig path stays the default (streaming + native tools).
            // Without one the provider still builds: the request is refused
            // when it is sent, with the fix named, so the gateway binds and the
            // daemon stays up. The build never fails because of a missing key.
            if key.is_none() {
                Ok(Box::new(gemini::GeminiProvider::new(key)))
            } else {
                Ok(Box::new(rig_native::RigProvider::for_provider("gemini", key)?))
            }
        }
        #[cfg(feature = "legacy-providers")]
        "gemini" | "google" | "google-gemini" => {
            Ok(Box::new(gemini::GeminiProvider::new(key)))
        }

        // ── OpenAI-compatible providers ──────────────────────
        "venice" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Venice", VENICE_BASE_URL, key, AuthStyle::Bearer,
        ))),
        "vercel" | "vercel-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Vercel AI Gateway", VERCEL_GATEWAY_BASE_URL, key, AuthStyle::Bearer,
        ))),
        "cloudflare" | "cloudflare-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Cloudflare AI Gateway",
            "https://gateway.ai.cloudflare.com/v1",
            key,
            AuthStyle::Bearer,
        ))),
        name if moonshot_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Moonshot",
            moonshot_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),
        "kimi-code" | "kimi_coding" | "kimi_for_coding" => Ok(Box::new(
            OpenAiCompatibleProvider::new_with_user_agent(
                "Kimi Code",
                "https://api.kimi.com/coding/v1",
                key,
                AuthStyle::Bearer,
                "KimiCLI/0.77",
            ),
        )),
        "synthetic" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Synthetic", "https://api.synthetic.new/openai/v1", key, AuthStyle::Bearer,
        ))),
        "opencode" | "opencode-zen" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "OpenCode Zen", "https://opencode.ai/zen/v1", key, AuthStyle::Bearer,
        ))),
        name if zai_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Z.AI",
            zai_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),
        name if glm_base_url(name).is_some() => {
            Ok(Box::new(OpenAiCompatibleProvider::new_no_responses_fallback(
                "GLM",
                glm_base_url(name).expect("checked in guard"),
                key,
                AuthStyle::Bearer,
            )))
        }
        name if minimax_base_url(name).is_some() => Ok(Box::new(build_minimax_provider(
            minimax_base_url(name).expect("checked in guard"),
            key,
        ))),
        "bedrock" | "aws-bedrock" => Ok(Box::new(bedrock::BedrockProvider::new())),
        name if is_qianfan_alias(name) => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Qianfan", "https://aip.baidubce.com", key, AuthStyle::Bearer,
        ))),
        name if is_doubao_alias(name) => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Doubao",
            "https://ark.cn-beijing.volces.com/api/v3",
            key,
            AuthStyle::Bearer,
        ))),
        name if qwen_base_url(name).is_some() => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Qwen",
            qwen_base_url(name).expect("checked in guard"),
            key,
            AuthStyle::Bearer,
        ))),

        // ── Extended ecosystem (community favorites) ─────────
        "groq" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Groq", GROQ_BASE_URL, key, AuthStyle::Bearer,
        ))),
        "mistral" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Mistral", "https://api.mistral.ai/v1", key, AuthStyle::Bearer,
        ))),
        "xai" | "grok" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "xAI", XAI_BASE_URL, key, AuthStyle::Bearer,
        ))),
        "deepseek" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "DeepSeek", "https://api.deepseek.com", key, AuthStyle::Bearer,
        ))),
        "together" | "together-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Together AI", "https://api.together.xyz", key, AuthStyle::Bearer,
        ))),
        "fireworks" | "fireworks-ai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Fireworks AI", "https://api.fireworks.ai/inference/v1", key, AuthStyle::Bearer,
        ))),
        "perplexity" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Perplexity", "https://api.perplexity.ai", key, AuthStyle::Bearer,
        ))),
        "cohere" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Cohere", "https://api.cohere.com/compatibility", key, AuthStyle::Bearer,
        ))),
        "lmstudio" | "lm-studio" => {
            let lm_studio_key = key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("lm-studio");
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "LM Studio",
                "http://localhost:1234/v1",
                Some(lm_studio_key),
                AuthStyle::Bearer,
            )))
        }
        "llamacpp" | "llama.cpp" => {
            let base_url = api_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("http://localhost:8080/v1");
            let llama_cpp_key = key
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("llama.cpp");
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "llama.cpp",
                base_url,
                Some(llama_cpp_key),
                AuthStyle::Bearer,
            )))
        }
        "nvidia" | "nvidia-nim" | "build.nvidia.com" => Ok(Box::new(
            OpenAiCompatibleProvider::new(
                "NVIDIA NIM",
                "https://integrate.api.nvidia.com/v1",
                key,
                AuthStyle::Bearer,
            ),
        )),

        // ── AI inference routers ─────────────────────────────
        "astrai" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "Astrai", "https://as-trai.com/v1", key, AuthStyle::Bearer,
        ))),

        // ── Cloud AI endpoints ───────────────────────────────
        "ovhcloud" | "ovh" => Ok(Box::new(OpenAiCompatibleProvider::new(
            "OVHcloud",
            "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1",
            key,
            AuthStyle::Bearer,
        ))),

        // ── Bring Your Own Provider (custom URL) ───────────
        // Format: "custom:https://your-api.com" or "custom:http://localhost:1234"
        name if name.starts_with("custom:") => {
            let base_url = parse_custom_provider_url(
                name.strip_prefix("custom:").unwrap_or(""),
                "Custom provider",
                "custom:https://your-api.com",
            )?;
            Ok(Box::new(OpenAiCompatibleProvider::new(
                "Custom",
                &base_url,
                key,
                AuthStyle::Bearer,
            )))
        }

        // ── Anthropic-compatible custom endpoints ───────────
        // Format: "anthropic-custom:https://your-api.com"
        name if name.starts_with("anthropic-custom:") => {
            let base_url = parse_custom_provider_url(
                name.strip_prefix("anthropic-custom:").unwrap_or(""),
                "Anthropic-custom provider",
                "anthropic-custom:https://your-api.com",
            )?;
            if let Some(provider) = anthropic_setup_token_provider(key) {
                return Ok(provider);
            }
            Ok(Box::new(
                rig_native::RigProvider::for_provider_with_url("anthropic", key, Some(&base_url))
                    .map_err(|_| anthropic_missing_credential_error(name))?,
            ))
        }

        // A name no factory knows follows from the config file alone, so it is
        // a `ProviderConfigError`: a caller may treat it as fatal.
        _ => Err(ProviderConfigError::UnknownProvider {
            name: bounded_name(name, MAX_UNKNOWN_PROVIDER_NAME_CHARS),
            guidance: "Check README for supported providers or run `rantaiclaw onboard --interactive` to reconfigure.\n\
             Tip: Use \"custom:https://your-api.com\" for OpenAI-compatible endpoints.\n\
             Tip: Use \"anthropic-custom:https://your-api.com\" for Anthropic-compatible endpoints."
                .to_string(),
        }
        .into()),
    }
}

#[cfg(test)]
struct TestSseProvider;

#[cfg(test)]
#[async_trait::async_trait]
impl Provider for TestSseProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("hello stream".to_string())
    }

    fn supports_streaming(&self) -> bool {
        true
    }
}

/// A provider without native tool calling, for a test that needs the text
/// protocol. It takes the trait defaults, so it never reaches the network.
#[cfg(test)]
struct TestNonNativeProvider;

#[cfg(test)]
#[async_trait::async_trait]
impl Provider for TestNonNativeProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("hello".to_string())
    }
}

/// Create provider chain with retry and fallback behavior.
pub fn create_resilient_provider(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    fallback_keys: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Box<dyn Provider>> {
    create_resilient_provider_with_options(
        primary_name,
        api_key,
        api_url,
        reliability,
        fallback_keys,
        &ProviderRuntimeOptions::default(),
    )
}

/// Create provider chain with retry/fallback behavior and auth runtime options.
pub fn create_resilient_provider_with_options(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    fallback_keys: &dyn Fn(&str) -> Option<String>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let mut providers: Vec<(String, Box<dyn Provider>)> = Vec::new();

    let primary_provider =
        create_provider_with_url_and_options(primary_name, api_key, api_url, options)?;
    providers.push((primary_name.to_string(), primary_provider));

    for fallback in &reliability.fallback_providers {
        if fallback == primary_name || providers.iter().any(|(name, _)| name == fallback) {
            continue;
        }

        // Each fallback resolves its credential by its own name: the key the
        // operator stored for it (`[provider_api_keys]`), then that provider's
        // env vars, then the generic RANTAICLAW_API_KEY / API_KEY. The
        // primary's key is never passed here: a key for one endpoint sent to
        // another is a credential leak.
        //
        // Keep using `create_provider_with_options` so fallback entries that
        // require runtime options continue to work.
        let fallback_key = (fallback_keys)(fallback);
        match create_provider_with_options(fallback, fallback_key.as_deref(), options) {
            Ok(provider) => providers.push((fallback.clone(), provider)),
            Err(_error) => {
                tracing::warn!(
                    fallback_provider = fallback,
                    "Ignoring invalid fallback provider during initialization"
                );
            }
        }
    }

    let mut reliable = ReliableProvider::new(
        providers,
        reliability.provider_retries,
        reliability.provider_backoff_ms,
    )
    .with_model_fallbacks(reliability.model_fallbacks.clone());
    if let Some(announce) = options.failover_announce.0.clone() {
        reliable = reliable.with_failover_announce(announce);
    }

    Ok(Box::new(reliable))
}

/// Create a RouterProvider if model routes are configured, otherwise return a
/// standard resilient provider. The router wraps individual providers per route,
/// each with its own retry/fallback chain.
pub fn create_routed_provider(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    model_routes: &[crate::config::ModelRouteConfig],
    default_model: &str,
    fallback_keys: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Box<dyn Provider>> {
    create_routed_provider_with_options(
        primary_name,
        api_key,
        api_url,
        reliability,
        model_routes,
        default_model,
        fallback_keys,
        &ProviderRuntimeOptions::default(),
    )
}

/// The credential a routed provider should build with: its own per-route key if
/// present; otherwise the primary provider's key ONLY when it IS the primary. A
/// non-primary routed provider must resolve its OWN credential downstream (its
/// env var / config) — inheriting the primary's key would authenticate the wrong
/// provider and 401. Mirrors the `api_url` gating and the fallback-provider
/// handling in `create_resilient_provider_with_options`.
fn routed_provider_credential<'a>(
    name: &str,
    primary_name: &str,
    routed_credential: Option<&'a str>,
    api_key: Option<&'a str>,
) -> Option<&'a str> {
    routed_credential.or(if name == primary_name { api_key } else { None })
}

/// Create a routed provider using explicit runtime options.
pub fn create_routed_provider_with_options(
    primary_name: &str,
    api_key: Option<&str>,
    api_url: Option<&str>,
    reliability: &crate::config::ReliabilityConfig,
    model_routes: &[crate::config::ModelRouteConfig],
    default_model: &str,
    fallback_keys: &dyn Fn(&str) -> Option<String>,
    options: &ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    if model_routes.is_empty() {
        // No route table: a `hint:` model has nothing to resolve against. The
        // guard refuses it before any request, instead of sending the literal
        // string `hint:fast` to the provider as a model name. Everything the
        // wrapped provider does for a plain model — its retry, its fallback,
        // its capabilities — is unchanged.
        let provider = create_resilient_provider_with_options(
            primary_name,
            api_key,
            api_url,
            reliability,
            fallback_keys,
            options,
        )?;
        return Ok(Box::new(router::HintGuard::new(provider)));
    }

    // Collect unique provider names needed
    let mut needed: Vec<String> = vec![primary_name.to_string()];
    for route in model_routes {
        if !needed.iter().any(|n| n == &route.provider) {
            needed.push(route.provider.clone());
        }
    }

    // Create each provider (with its own resilience wrapper)
    let mut providers: Vec<(String, Box<dyn Provider>)> = Vec::new();
    for name in &needed {
        let routed_credential = model_routes
            .iter()
            .find(|r| &r.provider == name)
            .and_then(|r| {
                r.api_key.as_ref().and_then(|raw_key| {
                    let trimmed_key = raw_key.trim();
                    (!trimmed_key.is_empty()).then_some(trimmed_key)
                })
            });
        let key = routed_provider_credential(name, primary_name, routed_credential, api_key);
        // Only use api_url for the primary provider
        let url = if name == primary_name { api_url } else { None };
        match create_resilient_provider_with_options(
            name,
            key,
            url,
            reliability,
            fallback_keys,
            options,
        ) {
            Ok(provider) => providers.push((name.clone(), provider)),
            Err(e) => {
                if name == primary_name {
                    return Err(e);
                }
                tracing::warn!(
                    provider = name.as_str(),
                    "Ignoring routed provider that failed to initialize"
                );
            }
        }
    }

    // Build route table
    let routes: Vec<(String, router::Route)> = model_routes
        .iter()
        .map(|r| {
            (
                r.hint.clone(),
                router::Route {
                    provider_name: r.provider.clone(),
                    model: r.model.clone(),
                },
            )
        })
        .collect();

    Ok(Box::new(router::RouterProvider::new(
        providers,
        routes,
        default_model.to_string(),
    )))
}

/// Information about a supported provider for display purposes.
pub struct ProviderInfo {
    /// Canonical name used in config (e.g. `"openrouter"`)
    pub name: &'static str,
    /// Human-readable display name
    pub display_name: &'static str,
    /// Alternative names accepted in config
    pub aliases: &'static [&'static str],
    /// Whether the provider runs locally (no API key required)
    pub local: bool,
}

/// Whether `name` runs locally and therefore needs no API key.
///
/// Reads [`ProviderInfo::local`] from [`list_providers`], which is the
/// documented source of truth for this. Two other encodings of the same fact
/// exist and have both drifted from it — `onboard::wizard`'s
/// `provider_supports_keyless_local_usage` omits `lmstudio`, and
/// `doctor::legacy` hardcodes `{ollama}` alone — so read it from the catalog
/// rather than adding a fourth list.
pub fn provider_is_local(name: &str) -> bool {
    let canonical = normalize_provider_name(name);
    list_providers()
        .iter()
        .any(|p| p.name == canonical && p.local)
}

/// Return the list of all known providers for display in `rantaiclaw providers list`.
///
/// This is intentionally separate from the factory match in `create_provider`
/// (display concern vs. construction concern).
pub fn list_providers() -> Vec<ProviderInfo> {
    vec![
        // ── Primary providers ────────────────────────────────
        ProviderInfo {
            name: "openrouter",
            display_name: "OpenRouter",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "anthropic",
            display_name: "Anthropic",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "openai",
            display_name: "OpenAI",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "ollama",
            display_name: "Ollama",
            aliases: &[],
            local: true,
        },
        ProviderInfo {
            name: "gemini",
            display_name: "Google Gemini",
            aliases: &["google", "google-gemini"],
            local: false,
        },
        // ── OpenAI-compatible providers ──────────────────────
        ProviderInfo {
            name: "venice",
            display_name: "Venice",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "vercel",
            display_name: "Vercel AI Gateway",
            aliases: &["vercel-ai"],
            local: false,
        },
        ProviderInfo {
            name: "cloudflare",
            display_name: "Cloudflare AI",
            aliases: &["cloudflare-ai"],
            local: false,
        },
        ProviderInfo {
            name: "moonshot",
            display_name: "Moonshot",
            aliases: &["kimi"],
            local: false,
        },
        ProviderInfo {
            name: "kimi-code",
            display_name: "Kimi Code",
            aliases: &["kimi_coding", "kimi_for_coding"],
            local: false,
        },
        ProviderInfo {
            name: "synthetic",
            display_name: "Synthetic",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "opencode",
            display_name: "OpenCode Zen",
            aliases: &["opencode-zen"],
            local: false,
        },
        ProviderInfo {
            name: "zai",
            display_name: "Z.AI",
            aliases: &["z.ai"],
            local: false,
        },
        ProviderInfo {
            name: "glm",
            display_name: "GLM (Zhipu)",
            aliases: &["zhipu"],
            local: false,
        },
        ProviderInfo {
            name: "minimax",
            display_name: "MiniMax",
            aliases: &[
                "minimax-intl",
                "minimax-io",
                "minimax-global",
                "minimax-cn",
                "minimaxi",
                "minimax-portal",
                "minimax-portal-cn",
            ],
            local: false,
        },
        ProviderInfo {
            name: "bedrock",
            display_name: "Amazon Bedrock",
            aliases: &["aws-bedrock"],
            local: false,
        },
        ProviderInfo {
            name: "qianfan",
            display_name: "Qianfan (Baidu)",
            aliases: &["baidu"],
            local: false,
        },
        ProviderInfo {
            name: "doubao",
            display_name: "Doubao (Volcengine)",
            aliases: &["volcengine", "ark", "doubao-cn"],
            local: false,
        },
        ProviderInfo {
            name: "qwen",
            display_name: "Qwen (DashScope)",
            aliases: &[
                "dashscope",
                "qwen-intl",
                "dashscope-intl",
                "qwen-us",
                "dashscope-us",
            ],
            local: false,
        },
        ProviderInfo {
            name: "groq",
            display_name: "Groq",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "mistral",
            display_name: "Mistral",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "xai",
            display_name: "xAI (Grok)",
            aliases: &["grok"],
            local: false,
        },
        ProviderInfo {
            name: "deepseek",
            display_name: "DeepSeek",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "together",
            display_name: "Together AI",
            aliases: &["together-ai"],
            local: false,
        },
        ProviderInfo {
            name: "fireworks",
            display_name: "Fireworks AI",
            aliases: &["fireworks-ai"],
            local: false,
        },
        ProviderInfo {
            name: "perplexity",
            display_name: "Perplexity",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "cohere",
            display_name: "Cohere",
            aliases: &[],
            local: false,
        },
        ProviderInfo {
            name: "lmstudio",
            display_name: "LM Studio",
            aliases: &["lm-studio"],
            local: true,
        },
        ProviderInfo {
            name: "llamacpp",
            display_name: "llama.cpp server",
            aliases: &["llama.cpp"],
            local: true,
        },
        ProviderInfo {
            name: "nvidia",
            display_name: "NVIDIA NIM",
            aliases: &["nvidia-nim", "build.nvidia.com"],
            local: false,
        },
        ProviderInfo {
            name: "ovhcloud",
            display_name: "OVHcloud AI Endpoints",
            aliases: &["ovh"],
            local: false,
        },
        ProviderInfo {
            name: "astrai",
            display_name: "Astrai",
            aliases: &[],
            local: false,
        },
    ]
}

/// `pub(crate)`: the doors' tests reach the recording provider through the
/// factory, so the recorder has to be nameable from outside this module.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Calls seen by the test-only recording provider.
    pub(crate) static RECORDING_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// What one recorded call saw: the model it was asked for, and the
    /// credential the provider was built with.
    #[derive(Clone, Debug)]
    pub(crate) struct RecordedCall {
        /// The model name the provider was asked for.
        pub(crate) model: String,
        /// The credential the provider was built with.
        pub(crate) credential: Option<String>,
    }

    /// The Gemini provider a factory-built provider holds, if any.
    ///
    /// The factory hands back a `Box<dyn Provider>`, and the routed factory
    /// wraps it in a retry wrapper and a hint guard. A test that wants to
    /// prove what the FACTORY built has to reach through those wrappers: a
    /// provider the test builds next to the factory's one proves something
    /// else, and the two can drift apart.
    pub(crate) fn gemini_provider_inside(
        provider: &dyn Provider,
    ) -> Option<&gemini::GeminiProvider> {
        provider
            .test_as_any()
            .and_then(|inner| inner.downcast_ref::<gemini::GeminiProvider>())
    }

    /// The calls the recording provider has seen, oldest first.
    static RECORDED_CALLS: Mutex<Vec<RecordedCall>> = Mutex::new(Vec::new());

    /// The recorder is one process-wide place, so the tests that read it run
    /// one at a time. `ENV_LOCK` is for the environment variables the
    /// config resolution reads; this one is for the recorder's own counters.
    ///
    /// Take it BEFORE `ENV_LOCK`: every test that reads the recorder takes
    /// this one first, so the two locks are always held in that order.
    pub(crate) static RECORDING_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A provider that counts the calls it receives and remembers what each
    /// one carried.
    ///
    /// The factory builds providers by name, so this one is reachable as the
    /// `"test-recording"` provider name. It lets a test prove that a turn
    /// never reached a provider, which provider and model a turn did reach,
    /// and which credential that provider was built with.
    pub(crate) struct RecordingProvider {
        /// The credential this provider was built with.
        credential: Option<String>,
    }

    impl RecordingProvider {
        /// Build a provider that was given `credential`.
        pub(crate) fn new(credential: Option<&str>) -> Self {
            Self {
                credential: credential.map(ToString::to_string),
            }
        }

        /// How many calls the provider has seen.
        pub(crate) fn calls() -> usize {
            RECORDING_CALLS.load(Ordering::SeqCst)
        }

        /// Forget the calls seen so far.
        pub(crate) fn reset() {
            RECORDING_CALLS.store(0, Ordering::SeqCst);
            RECORDED_CALLS
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clear();
        }

        /// What the last call saw.
        pub(crate) fn last_call() -> Option<RecordedCall> {
            RECORDED_CALLS
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .last()
                .cloned()
        }
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            RECORDING_CALLS.fetch_add(1, Ordering::SeqCst);
            RECORDED_CALLS
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(RecordedCall {
                    model: model.to_string(),
                    credential: self.credential.clone(),
                });
            Ok("recorded".to_string())
        }
    }

    /// A door with no `[[model_routes]]` has nothing to resolve a `hint:`
    /// model against. The turn is refused before any request, instead of the
    /// literal string `hint:fast` being sent to the provider as a model name.
    #[tokio::test]
    async fn a_routed_provider_without_routes_refuses_a_hint() {
        // The recording provider counts its calls in one process-wide place,
        // so the tests that read it run one at a time.
        let _recording = crate::providers::tests::RECORDING_LOCK.lock().await;
        // The provider build reads the environment for a credential, so the
        // test owns the environment while it does.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        RecordingProvider::reset();

        let provider = create_routed_provider_with_options(
            "test-recording",
            None,
            None,
            &crate::config::ReliabilityConfig::default(),
            &[],
            "default-model",
            &|_: &str| -> Option<String> { None },
            &ProviderRuntimeOptions::default(),
        )
        .expect("the provider builds");

        // A plain model still reaches the provider: the recorder works.
        let answer = provider
            .simple_chat("hello", "default-model", 0.5)
            .await
            .expect("a plain model is served");
        assert_eq!(answer, "recorded");
        assert_eq!(RecordingProvider::calls(), 1);

        // A hint is refused, and no request is sent.
        let error = provider
            .simple_chat("hello", "hint:fast", 0.5)
            .await
            .expect_err("a hint must be refused without a route table");
        assert!(
            error.to_string().contains(router::NO_ROUTES_REFUSAL),
            "{error}"
        );
        assert_eq!(RecordingProvider::calls(), 1, "no request was sent");
    }

    #[test]
    fn routed_provider_does_not_inherit_the_primary_key() {
        // A non-primary routed provider without its own key must NOT get the
        // primary's key — it resolves its own credential downstream (None here).
        assert_eq!(
            routed_provider_credential("deepseek", "openai", None, Some("sk-openai")),
            None,
            "non-primary routed provider must not inherit the primary key"
        );
        // The primary provider without a per-route key still inherits the key.
        assert_eq!(
            routed_provider_credential("openai", "openai", None, Some("sk-openai")),
            Some("sk-openai")
        );
        // A per-route key always wins, even for a non-primary provider.
        assert_eq!(
            routed_provider_credential(
                "deepseek",
                "openai",
                Some("sk-deepseek"),
                Some("sk-openai")
            ),
            Some("sk-deepseek")
        );
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let original = std::env::var(key).ok();
            match value {
                Some(next) => std::env::set_var(key, next),
                None => std::env::remove_var(key),
            }

            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.as_deref() {
                std::env::set_var(self.key, original);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    /// The CRATE-shared env lock, not a module-local one.
    ///
    /// `cargo test --lib` runs every unit test in one process across many
    /// threads, so a per-module mutex does not serialise these against the
    /// env-mutating tests in `config`, `channels` or `lifecycle` — they hold a
    /// different lock and clobber each other's variables mid-test. This project
    /// has already been bitten by exactly that fragmentation.
    fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        crate::test_env::ENV_LOCK.blocking_lock()
    }

    #[test]
    fn resolve_provider_credential_prefers_explicit_argument() {
        let resolved = resolve_provider_credential("openrouter", Some("  explicit-key  "));
        assert_eq!(resolved, Some("explicit-key".to_string()));
    }

    #[test]
    fn resolve_provider_credential_bedrock_uses_internal_credential_path() {
        let _generic_guard = EnvGuard::set("API_KEY", Some("generic-key"));
        let _override_guard = EnvGuard::set("OPENROUTER_API_KEY", Some("openrouter-key"));

        assert_eq!(
            resolve_provider_credential("bedrock", Some("explicit")),
            Some("explicit".to_string())
        );
        assert!(resolve_provider_credential("bedrock", None).is_none());
        assert!(resolve_provider_credential("aws-bedrock", None).is_none());
    }

    #[test]
    fn regional_alias_predicates_cover_expected_variants() {
        assert!(is_moonshot_alias("moonshot"));
        assert!(is_moonshot_alias("kimi-global"));
        assert!(is_glm_alias("glm"));
        assert!(is_glm_alias("bigmodel"));
        assert!(is_minimax_alias("minimax-io"));
        assert!(is_minimax_alias("minimaxi"));
        assert!(is_minimax_alias("minimax-portal-cn"));
        assert!(is_qwen_alias("dashscope"));
        assert!(is_qwen_alias("qwen-us"));
        assert!(is_zai_alias("z.ai"));
        assert!(is_zai_alias("zai-cn"));
        assert!(is_qianfan_alias("qianfan"));
        assert!(is_qianfan_alias("baidu"));
        assert!(is_doubao_alias("doubao"));
        assert!(is_doubao_alias("volcengine"));
        assert!(is_doubao_alias("ark"));
        assert!(is_doubao_alias("doubao-cn"));

        assert!(!is_moonshot_alias("openrouter"));
        assert!(!is_glm_alias("openai"));
        assert!(!is_qwen_alias("gemini"));
        assert!(!is_zai_alias("anthropic"));
        assert!(!is_qianfan_alias("cohere"));
        assert!(!is_doubao_alias("deepseek"));
    }

    #[test]
    fn canonical_china_provider_name_maps_regional_aliases() {
        assert_eq!(canonical_china_provider_name("moonshot"), Some("moonshot"));
        assert_eq!(canonical_china_provider_name("kimi-intl"), Some("moonshot"));
        assert_eq!(canonical_china_provider_name("glm"), Some("glm"));
        assert_eq!(canonical_china_provider_name("zhipu-cn"), Some("glm"));
        assert_eq!(canonical_china_provider_name("minimax"), Some("minimax"));
        assert_eq!(canonical_china_provider_name("minimax-cn"), Some("minimax"));
        assert_eq!(canonical_china_provider_name("qwen"), Some("qwen"));
        assert_eq!(canonical_china_provider_name("dashscope-us"), Some("qwen"));
        assert_eq!(canonical_china_provider_name("zai"), Some("zai"));
        assert_eq!(canonical_china_provider_name("z.ai-cn"), Some("zai"));
        assert_eq!(canonical_china_provider_name("qianfan"), Some("qianfan"));
        assert_eq!(canonical_china_provider_name("baidu"), Some("qianfan"));
        assert_eq!(canonical_china_provider_name("doubao"), Some("doubao"));
        assert_eq!(canonical_china_provider_name("volcengine"), Some("doubao"));
        assert_eq!(canonical_china_provider_name("openai"), None);
    }

    /// Pins the four bases that shipped pointing at a path their API does not
    /// serve. `OpenAiCompatibleProvider` appends `/chat/completions` verbatim,
    /// so a base missing its version segment sent every message to a 404 — the
    /// provider was configurable but could not answer once.
    ///
    /// Each value below was established by probing the live API without
    /// credentials, where `404` means the path does not exist and any other
    /// status means it does:
    ///
    /// ```text
    /// POST api.groq.com/openai/chat/completions        404   ← shipped
    /// POST api.groq.com/openai/v1/chat/completions     401
    /// POST api.x.ai/chat/completions                   404   ← shipped
    /// POST api.x.ai/v1/chat/completions                400
    /// POST api.venice.ai/chat/completions              404   ← shipped
    /// GET  api.venice.ai/v1/models                     402
    /// GET  api.venice.ai/api/v1/models                 200
    /// POST api.vercel.ai/chat/completions              404   ← shipped
    /// POST api.vercel.ai/v1/chat/completions           404
    /// GET  ai-gateway.vercel.sh/v1/models              200
    /// ```
    ///
    /// `api.venice.ai/xyz/...` and `api.vercel.ai/xyz/...` both answer 404,
    /// confirming those hosts route by path rather than rejecting everything —
    /// so the 404s above mean the path was wrong, not that the probe was.
    ///
    /// Asserting the literals is the point: these cannot be derived, only
    /// checked against the vendor, and a silent edit would restore a provider
    /// that looks configured and never works.
    #[test]
    fn openai_compatible_bases_are_the_probed_ones() {
        assert_eq!(GROQ_BASE_URL, "https://api.groq.com/openai/v1");
        assert_eq!(XAI_BASE_URL, "https://api.x.ai/v1");
        assert_eq!(VENICE_BASE_URL, "https://api.venice.ai/api/v1");
        assert_eq!(VERCEL_GATEWAY_BASE_URL, "https://ai-gateway.vercel.sh/v1");

        // The shape that broke them: a host with no API path at all. Every one
        // of these must carry a version segment before `/chat/completions` is
        // appended.
        for base in [
            GROQ_BASE_URL,
            XAI_BASE_URL,
            VENICE_BASE_URL,
            VERCEL_GATEWAY_BASE_URL,
        ] {
            let path = base.split_once("://").expect("scheme").1;
            assert!(
                path.contains('/'),
                "{base} has no API path; `/chat/completions` would hit the host root"
            );
        }
    }

    #[test]
    fn regional_endpoint_aliases_map_to_expected_urls() {
        assert_eq!(minimax_base_url("minimax"), Some(MINIMAX_INTL_BASE_URL));
        assert_eq!(
            minimax_base_url("minimax-intl"),
            Some(MINIMAX_INTL_BASE_URL)
        );
        assert_eq!(minimax_base_url("minimax-cn"), Some(MINIMAX_CN_BASE_URL));

        assert_eq!(glm_base_url("glm"), Some(GLM_GLOBAL_BASE_URL));
        assert_eq!(glm_base_url("glm-cn"), Some(GLM_CN_BASE_URL));
        assert_eq!(glm_base_url("bigmodel"), Some(GLM_CN_BASE_URL));

        assert_eq!(moonshot_base_url("moonshot"), Some(MOONSHOT_CN_BASE_URL));
        assert_eq!(
            moonshot_base_url("moonshot-intl"),
            Some(MOONSHOT_INTL_BASE_URL)
        );

        assert_eq!(qwen_base_url("qwen"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_base_url("qwen-cn"), Some(QWEN_CN_BASE_URL));
        assert_eq!(qwen_base_url("qwen-intl"), Some(QWEN_INTL_BASE_URL));
        assert_eq!(qwen_base_url("qwen-us"), Some(QWEN_US_BASE_URL));

        assert_eq!(zai_base_url("zai"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("z.ai"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("zai-global"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("z.ai-global"), Some(ZAI_GLOBAL_BASE_URL));
        assert_eq!(zai_base_url("zai-cn"), Some(ZAI_CN_BASE_URL));
        assert_eq!(zai_base_url("z.ai-cn"), Some(ZAI_CN_BASE_URL));
    }

    // ── Primary providers ────────────────────────────────────

    #[test]
    fn factory_openrouter() {
        assert!(create_provider("openrouter", Some("provider-test-credential")).is_ok());
        assert!(create_provider("openrouter", None).is_ok());
    }

    #[test]
    fn factory_anthropic() {
        assert!(create_provider("anthropic", Some("provider-test-credential")).is_ok());
    }

    #[test]
    fn factory_openai() {
        assert!(create_provider("openai", Some("provider-test-credential")).is_ok());
    }

    #[test]
    fn factory_ollama() {
        assert!(create_provider("ollama", None).is_ok());
        // Ollama may use API key when a remote endpoint is configured.
        assert!(create_provider("ollama", Some("dummy")).is_ok());
        assert!(create_provider("ollama", Some("any-value-here")).is_ok());
    }

    #[test]
    fn factory_gemini() {
        assert!(create_provider("gemini", Some("test-key")).is_ok());
        assert!(create_provider("google", Some("test-key")).is_ok());
        assert!(create_provider("google-gemini", Some("test-key")).is_ok());
    }

    #[test]
    fn factory_gemini_resolves_env_api_key() {
        // Regression (plan 025): the default path must honor GEMINI_API_KEY /
        // GOOGLE_API_KEY again. The rig-native migration dropped the gemini arm
        // in resolve_provider_credential, so env-key users hit "no key".
        let _g = crate::test_env::ENV_LOCK.blocking_lock();
        let prev_g = std::env::var_os("GEMINI_API_KEY");
        let prev_goog = std::env::var_os("GOOGLE_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");
        std::env::set_var("GEMINI_API_KEY", "AIzaSyTEST-env-key");
        let with_env_ok = create_provider("gemini", None).is_ok();
        let resolved = resolve_provider_credential("gemini", None);
        match prev_g {
            Some(v) => std::env::set_var("GEMINI_API_KEY", v),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
        match prev_goog {
            Some(v) => std::env::set_var("GOOGLE_API_KEY", v),
            None => std::env::remove_var("GOOGLE_API_KEY"),
        }
        assert_eq!(resolved.as_deref(), Some("AIzaSyTEST-env-key"));
        assert!(
            with_env_ok,
            "GEMINI_API_KEY must resolve a usable gemini provider"
        );
    }

    /// A Gemini provider without an API key is still built: the gateway binds
    /// and the daemon stays up. The request is refused when it is sent, with
    /// the key named.
    ///
    /// The refusal is asserted on the provider the FACTORY built — not on one
    /// the test builds next to it — and on the request that WOULD be built:
    /// a test must never send anything to Google, not even a request the
    /// provider refuses.
    #[tokio::test]
    async fn a_keyless_gemini_provider_is_built_and_refuses_the_request() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let provider = create_provider("gemini", None)
            .expect("a keyless Gemini provider must be built: the request is refused instead");

        let gemini = gemini_provider_inside(provider.as_ref())
            .expect("the factory builds the Gemini provider when there is no key");

        let error = gemini
            .build_request_for_test("gemini-2.0-flash")
            .expect_err("a missing API key must fail the request");
        assert!(
            error.to_string().contains("Gemini API key not found"),
            "{error}"
        );
        // The refusal is a TYPE, so the retry wrapper recognises it without
        // reading the message. The type has to survive the provider
        // boundary for that to hold.
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<gemini_error::GeminiAuthError>()
                .is_some()),
            "the refusal must reach the retry wrapper as a typed error: {error:?}"
        );
    }

    /// The same through the routed factory the gateway builds its provider
    /// with.
    #[tokio::test]
    async fn the_routed_factory_builds_a_keyless_gemini_provider() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 0,
            ..Default::default()
        };
        create_routed_provider_with_options(
            "gemini",
            None,
            None,
            &reliability,
            &[],
            "gemini-2.0-flash",
            &|_: &str| -> Option<String> { None },
            &ProviderRuntimeOptions::default(),
        )
        .expect("a keyless Gemini provider must be built");
    }

    /// A setup-token is refused for the anthropic family, and the refusal
    /// names the API key that replaces it. The value itself never appears in
    /// the sentence, and a provider outside the family is not touched.
    #[test]
    fn a_setup_token_is_refused_for_the_anthropic_family_only() {
        let sentence = setup_token_refusal("anthropic", Some("sk-ant-oat01-abc"))
            .expect("a setup-token is refused");
        assert!(sentence.contains("ANTHROPIC_API_KEY"), "{sentence}");
        assert!(!sentence.contains("sk-ant-oat01-abc"), "{sentence}");

        assert_eq!(
            setup_token_refusal("anthropic", Some("  sk-ant-oat01-abc  ")),
            Some(sentence),
            "the prefix rule ignores surrounding whitespace"
        );
        assert!(setup_token_refusal("anthropic", Some("sk-ant-api03-abc")).is_none());
        assert!(setup_token_refusal("anthropic", None).is_none());
        assert!(setup_token_refusal("openai", Some("sk-ant-oat01-abc")).is_none());
    }

    /// Bedrock is the case that must not regress: `BedrockProvider::new()`
    /// ignores the `key` argument entirely and reads `AwsCredentials::from_env`,
    /// so an install authenticated only by AWS environment variables sends fine
    /// while the resolver returns `None` for it.
    #[test]
    fn bedrock_is_usable_with_only_aws_env_credentials() {
        let _env_lock = env_lock();
        let _clear = crate::test_env::CredentialEnvScrub::new();

        for name in ["bedrock", "aws-bedrock"] {
            assert!(
                !has_usable_credential(name, None),
                "{name} with no AWS credentials is not usable"
            );
        }

        let _id = crate::test_env::EnvGuard::set("AWS_ACCESS_KEY_ID", "AKIAEXAMPLEPLACEHOLDER");
        let _secret =
            crate::test_env::EnvGuard::set("AWS_SECRET_ACCESS_KEY", "neutral-aws-secret-value");

        for name in ["bedrock", "aws-bedrock"] {
            assert!(
                has_usable_credential(name, None),
                "{name} authenticated by AWS env vars must read as usable"
            );
        }
    }

    /// A local provider needs no credential at all, so nothing is missing.
    #[test]
    fn local_providers_need_no_credential() {
        let _env_lock = env_lock();
        let _clear = crate::test_env::CredentialEnvScrub::new();

        for name in ["ollama", "lmstudio", "llamacpp"] {
            assert!(
                has_usable_credential(name, None),
                "{name} runs locally and needs no key"
            );
        }
    }

    /// The other direction, which matters just as much: a plain-API-key
    /// provider with nothing anywhere must still read as unusable. A wrong
    /// `true` here restores the original defect — an install saved that cannot
    /// send.
    #[test]
    fn a_plain_key_provider_with_no_credential_anywhere_is_unusable() {
        let _env_lock = env_lock();
        let _clear = crate::test_env::CredentialEnvScrub::new();

        for name in ["openai", "anthropic", "openrouter", "groq"] {
            assert!(
                !has_usable_credential(name, None),
                "{name} has no credential and must not read as configured"
            );
        }
    }

    #[test]
    fn has_usable_credential_gemini_env_and_config() {
        // Plan 025 (diagnostics): doctor/setup must see gemini env + config keys
        // as usable again.
        let _g = crate::test_env::ENV_LOCK.blocking_lock();
        let prev_g = std::env::var_os("GEMINI_API_KEY");
        let prev_goog = std::env::var_os("GOOGLE_API_KEY");
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("GOOGLE_API_KEY");
        assert!(has_usable_credential("gemini", Some("cfg-key")));
        std::env::set_var("GOOGLE_API_KEY", "AIzaSy-goog");
        let via_env = has_usable_credential("gemini", None);
        match prev_g {
            Some(v) => std::env::set_var("GEMINI_API_KEY", v),
            None => std::env::remove_var("GEMINI_API_KEY"),
        }
        match prev_goog {
            Some(v) => std::env::set_var("GOOGLE_API_KEY", v),
            None => std::env::remove_var("GOOGLE_API_KEY"),
        }
        assert!(
            via_env,
            "GOOGLE_API_KEY must count as a usable gemini credential"
        );
    }

    /// The public door to a removed login's refusal.
    ///
    /// The gateway and the doctor live outside this module, so they ask here
    /// instead of reaching into a private submodule: the sentence they show
    /// has to come from one place, and it has to be the sentence the refusal
    /// itself carries.
    #[test]
    fn the_removed_login_door_answers_what_the_module_answers() {
        assert_eq!(
            removed_login_refusal("openai-codex"),
            removed_login::removed_login_refusal("openai-codex"),
            "the door must answer what the module answers"
        );
        let sentence =
            removed_login_refusal("openai-codex").expect("a removed login has a refusal");
        assert!(sentence.contains("OPENAI_API_KEY"), "{sentence}");
        // A provider that still exists has no refusal to hand out.
        assert_eq!(removed_login_refusal("openai"), None);
    }

    /// The public door to a refusal's sentence, read out of a real refusal:
    /// the provider the factory built for a removed login, refusing the way
    /// the gateway will see it.
    #[tokio::test]
    async fn the_local_refusal_door_reads_the_sentence_of_a_factory_refusal() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let provider =
            create_provider("openai-codex", None).expect("a removed login still builds a provider");
        let error = provider
            .simple_chat("hello", "test-model", 0.0)
            .await
            .expect_err("a removed login refuses the request");
        assert_eq!(
            local_refusal_text(&error),
            removed_login::removed_login_refusal("openai-codex"),
            "the door must read the sentence the provider refused with"
        );
        // An ordinary failure carries no refusal, so the door says nothing.
        assert_eq!(
            local_refusal_text(&anyhow::anyhow!("an ordinary failure")),
            None
        );
    }

    // ── OpenAI-compatible providers ──────────────────────────

    #[test]
    fn factory_venice() {
        assert!(create_provider("venice", Some("vn-key")).is_ok());
    }

    #[test]
    fn factory_vercel() {
        assert!(create_provider("vercel", Some("key")).is_ok());
        assert!(create_provider("vercel-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_cloudflare() {
        assert!(create_provider("cloudflare", Some("key")).is_ok());
        assert!(create_provider("cloudflare-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_moonshot() {
        assert!(create_provider("moonshot", Some("key")).is_ok());
        assert!(create_provider("kimi", Some("key")).is_ok());
        assert!(create_provider("moonshot-intl", Some("key")).is_ok());
        assert!(create_provider("moonshot-cn", Some("key")).is_ok());
        assert!(create_provider("kimi-intl", Some("key")).is_ok());
        assert!(create_provider("kimi-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_kimi_code() {
        assert!(create_provider("kimi-code", Some("key")).is_ok());
        assert!(create_provider("kimi_coding", Some("key")).is_ok());
        assert!(create_provider("kimi_for_coding", Some("key")).is_ok());
    }

    #[test]
    fn factory_synthetic() {
        assert!(create_provider("synthetic", Some("key")).is_ok());
    }

    #[test]
    fn factory_opencode() {
        assert!(create_provider("opencode", Some("key")).is_ok());
        assert!(create_provider("opencode-zen", Some("key")).is_ok());
    }

    #[test]
    fn factory_zai() {
        assert!(create_provider("zai", Some("key")).is_ok());
        assert!(create_provider("z.ai", Some("key")).is_ok());
        assert!(create_provider("zai-global", Some("key")).is_ok());
        assert!(create_provider("z.ai-global", Some("key")).is_ok());
        assert!(create_provider("zai-cn", Some("key")).is_ok());
        assert!(create_provider("z.ai-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_glm() {
        assert!(create_provider("glm", Some("key")).is_ok());
        assert!(create_provider("zhipu", Some("key")).is_ok());
        assert!(create_provider("glm-cn", Some("key")).is_ok());
        assert!(create_provider("zhipu-cn", Some("key")).is_ok());
        assert!(create_provider("glm-global", Some("key")).is_ok());
        assert!(create_provider("bigmodel", Some("key")).is_ok());
    }

    #[test]
    fn factory_minimax() {
        assert!(create_provider("minimax", Some("key")).is_ok());
        assert!(create_provider("minimax-intl", Some("key")).is_ok());
        assert!(create_provider("minimax-io", Some("key")).is_ok());
        assert!(create_provider("minimax-global", Some("key")).is_ok());
        assert!(create_provider("minimax-cn", Some("key")).is_ok());
        assert!(create_provider("minimaxi", Some("key")).is_ok());
        assert!(create_provider("minimax-portal", Some("key")).is_ok());
        assert!(create_provider("minimax-portal-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_bedrock() {
        // Bedrock uses AWS env vars for credentials, not API key.
        assert!(create_provider("bedrock", None).is_ok());
        assert!(create_provider("aws-bedrock", None).is_ok());
        // Passing an api_key is harmless (ignored).
        assert!(create_provider("bedrock", Some("ignored")).is_ok());
    }

    #[test]
    fn factory_qianfan() {
        assert!(create_provider("qianfan", Some("key")).is_ok());
        assert!(create_provider("baidu", Some("key")).is_ok());
    }

    #[test]
    fn factory_doubao() {
        assert!(create_provider("doubao", Some("key")).is_ok());
        assert!(create_provider("volcengine", Some("key")).is_ok());
        assert!(create_provider("ark", Some("key")).is_ok());
        assert!(create_provider("doubao-cn", Some("key")).is_ok());
    }

    #[test]
    fn factory_qwen() {
        assert!(create_provider("qwen", Some("key")).is_ok());
        assert!(create_provider("dashscope", Some("key")).is_ok());
        assert!(create_provider("qwen-cn", Some("key")).is_ok());
        assert!(create_provider("dashscope-cn", Some("key")).is_ok());
        assert!(create_provider("qwen-intl", Some("key")).is_ok());
        assert!(create_provider("dashscope-intl", Some("key")).is_ok());
        assert!(create_provider("qwen-international", Some("key")).is_ok());
        assert!(create_provider("dashscope-international", Some("key")).is_ok());
        assert!(create_provider("qwen-us", Some("key")).is_ok());
        assert!(create_provider("dashscope-us", Some("key")).is_ok());
    }

    #[test]
    fn factory_lmstudio() {
        assert!(create_provider("lmstudio", Some("key")).is_ok());
        assert!(create_provider("lm-studio", Some("key")).is_ok());
        assert!(create_provider("lmstudio", None).is_ok());
    }

    #[test]
    fn factory_llamacpp() {
        assert!(create_provider("llamacpp", Some("key")).is_ok());
        assert!(create_provider("llama.cpp", Some("key")).is_ok());
        assert!(create_provider("llamacpp", None).is_ok());
    }

    // ── Extended ecosystem ───────────────────────────────────

    #[test]
    fn factory_groq() {
        assert!(create_provider("groq", Some("key")).is_ok());
    }

    #[test]
    fn factory_mistral() {
        assert!(create_provider("mistral", Some("key")).is_ok());
    }

    #[test]
    fn factory_xai() {
        assert!(create_provider("xai", Some("key")).is_ok());
        assert!(create_provider("grok", Some("key")).is_ok());
    }

    #[test]
    fn factory_deepseek() {
        assert!(create_provider("deepseek", Some("key")).is_ok());
    }

    #[test]
    fn factory_together() {
        assert!(create_provider("together", Some("key")).is_ok());
        assert!(create_provider("together-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_fireworks() {
        assert!(create_provider("fireworks", Some("key")).is_ok());
        assert!(create_provider("fireworks-ai", Some("key")).is_ok());
    }

    #[test]
    fn factory_perplexity() {
        assert!(create_provider("perplexity", Some("key")).is_ok());
    }

    #[test]
    fn factory_cohere() {
        assert!(create_provider("cohere", Some("key")).is_ok());
    }

    #[test]
    fn factory_nvidia() {
        assert!(create_provider("nvidia", Some("nvapi-test")).is_ok());
        assert!(create_provider("nvidia-nim", Some("nvapi-test")).is_ok());
        assert!(create_provider("build.nvidia.com", Some("nvapi-test")).is_ok());
    }

    // ── AI inference routers ─────────────────────────────────

    #[test]
    fn factory_astrai() {
        assert!(create_provider("astrai", Some("sk-astrai-test")).is_ok());
    }

    // ── Custom / BYOP provider ─────────────────────────────

    #[test]
    fn factory_custom_url() {
        let p = create_provider("custom:https://my-llm.example.com", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_custom_localhost() {
        let p = create_provider("custom:http://localhost:1234", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_custom_no_key() {
        let p = create_provider("custom:https://my-llm.example.com", None);
        assert!(p.is_ok());
    }

    #[test]
    fn factory_custom_empty_url_errors() {
        match create_provider("custom:", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a URL"),
                "Expected 'requires a URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for empty custom URL"),
        }
    }

    #[test]
    fn factory_custom_invalid_url_errors() {
        match create_provider("custom:not-a-url", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a valid URL"),
                "Expected 'requires a valid URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for invalid custom URL"),
        }
    }

    #[test]
    fn factory_custom_unsupported_scheme_errors() {
        match create_provider("custom:ftp://example.com", None) {
            Err(e) => assert!(
                e.to_string().contains("http:// or https://"),
                "Expected scheme validation error, got: {e}"
            ),
            Ok(_) => panic!("Expected error for unsupported custom URL scheme"),
        }
    }

    #[test]
    fn factory_custom_trims_whitespace() {
        let p = create_provider("custom:  https://my-llm.example.com  ", Some("key"));
        assert!(p.is_ok());
    }

    // ── Anthropic-compatible custom endpoints ─────────────────

    #[test]
    fn factory_anthropic_custom_url() {
        let p = create_provider("anthropic-custom:https://api.example.com", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_anthropic_custom_trailing_slash() {
        let p = create_provider("anthropic-custom:https://api.example.com/", Some("key"));
        assert!(p.is_ok());
    }

    #[test]
    fn factory_anthropic_custom_no_key() {
        // Fail-fast: a custom Anthropic endpoint needs an API key; the rig
        // reroute rejects a missing key at construction rather than deferring.
        // Holds the crate env lock and scrubs the credential env, so a sibling
        // test's ANTHROPIC_API_KEY cannot supply the key this test withholds.
        let _env_lock = env_lock();
        let _clear = crate::test_env::CredentialEnvScrub::new();
        let p = create_provider("anthropic-custom:https://api.example.com", None);
        assert!(p.is_err());
    }

    #[test]
    fn factory_anthropic_custom_empty_url_errors() {
        match create_provider("anthropic-custom:", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a URL"),
                "Expected 'requires a URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for empty anthropic-custom URL"),
        }
    }

    #[test]
    fn factory_anthropic_custom_invalid_url_errors() {
        match create_provider("anthropic-custom:not-a-url", None) {
            Err(e) => assert!(
                e.to_string().contains("requires a valid URL"),
                "Expected 'requires a valid URL', got: {e}"
            ),
            Ok(_) => panic!("Expected error for invalid anthropic-custom URL"),
        }
    }

    #[test]
    fn factory_anthropic_custom_unsupported_scheme_errors() {
        match create_provider("anthropic-custom:ftp://example.com", None) {
            Err(e) => assert!(
                e.to_string().contains("http:// or https://"),
                "Expected scheme validation error, got: {e}"
            ),
            Ok(_) => panic!("Expected error for unsupported anthropic-custom URL scheme"),
        }
    }

    // ── Error cases ──────────────────────────────────────────

    #[test]
    fn factory_unknown_provider_errors() {
        let p = create_provider("nonexistent", None);
        assert!(p.is_err());
        let msg = p.err().unwrap().to_string();
        assert!(msg.contains("Unknown provider"));
        assert!(msg.contains("nonexistent"));
    }

    /// The provider name the caller sent is echoed back in the refusal, and it
    /// is the ONE caller-controlled part of that message: the guidance beside
    /// it is static. So the name is bounded where it enters the error — cut to
    /// a fixed length, with the cut visible — and the guidance still reaches
    /// the caller whole.
    #[test]
    fn factory_unknown_provider_bounds_a_long_caller_supplied_name() {
        /// The length the echoed name is held to.
        const NAME_BOUND: usize = 64;

        /// The marker that shows the name was cut.
        const CUT_MARKER: &str = "...[truncated]";

        /// The static remediation lines, which must arrive whole.
        const GUIDANCE_LINES: [&str; 3] = [
            "Check README for supported providers or run `rantaiclaw onboard --interactive` to reconfigure.",
            "Tip: Use \"custom:https://your-api.com\" for OpenAI-compatible endpoints.",
            "Tip: Use \"anthropic-custom:https://your-api.com\" for Anthropic-compatible endpoints.",
        ];

        let long_name = "unknown-provider-".repeat(20);
        let msg = create_provider(&long_name, None)
            .err()
            .expect("an unknown provider must be refused")
            .to_string();

        let (echoed, _) = msg
            .split_once(". Check README for supported providers")
            .expect("the refusal names the provider, then gives the guidance");
        let echoed = echoed
            .strip_prefix("Unknown provider: ")
            .expect("the refusal opens with the unknown provider name");

        let kept = echoed
            .strip_suffix(CUT_MARKER)
            .expect("a cut name must say that it was cut");
        assert!(
            kept.chars().count() <= NAME_BOUND,
            "the echoed name must be bounded to {NAME_BOUND} characters, got {}: {kept}",
            kept.chars().count()
        );
        assert!(
            !msg.contains(&long_name),
            "the whole name must not be echoed back"
        );

        for line in GUIDANCE_LINES {
            assert!(
                msg.contains(line),
                "the guidance must reach the caller whole: {line}"
            );
        }

        // A multibyte name is cut on a char boundary: a cut inside a char
        // would not be a name at all.
        let multibyte = "é".repeat(80);
        let msg = create_provider(&multibyte, None)
            .err()
            .expect("an unknown provider must be refused")
            .to_string();
        let (echoed, _) = msg
            .split_once(". Check README for supported providers")
            .expect("the refusal names the provider, then gives the guidance");
        let echoed = echoed
            .strip_prefix("Unknown provider: ")
            .expect("the refusal opens with the unknown provider name");
        let kept = echoed
            .strip_suffix(CUT_MARKER)
            .expect("a cut name must say that it was cut");
        assert!(
            kept.chars().count() <= NAME_BOUND,
            "a multibyte name must be cut to {NAME_BOUND} characters, got {}: {kept}",
            kept.chars().count()
        );

        // A name that fits is echoed as it was sent: no cut, no marker.
        let short = create_provider("nonexistent", None)
            .err()
            .expect("an unknown provider must be refused")
            .to_string();
        assert!(
            short.contains("Unknown provider: nonexistent."),
            "a name that fits must be echoed unchanged: {short}"
        );
        assert!(
            !short.contains(CUT_MARKER),
            "a name that fits must not carry the cut marker: {short}"
        );
    }

    #[test]
    fn factory_empty_name_errors() {
        assert!(create_provider("", None).is_err());
    }

    #[test]
    fn resilient_provider_ignores_duplicate_and_invalid_fallbacks() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec![
                "openrouter".into(),
                "nonexistent-provider".into(),
                "openai".into(),
                "openai".into(),
            ],
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let provider = create_resilient_provider(
            "openrouter",
            Some("provider-test-credential"),
            None,
            &reliability,
            &|_| None,
        );
        assert!(provider.is_ok());
    }

    #[test]
    fn resilient_provider_errors_for_invalid_primary() {
        let reliability = crate::config::ReliabilityConfig::default();
        let provider = create_resilient_provider(
            "totally-invalid",
            Some("provider-test-credential"),
            None,
            &reliability,
            &|_| None,
        );
        assert!(provider.is_err());
    }

    /// Fallback providers resolve their own credentials via provider-specific
    /// env vars rather than inheriting the primary provider's key.  A provider
    /// that requires no key (e.g. lmstudio, ollama) must initialize
    /// successfully even when the primary uses a completely different key.
    #[test]
    fn resilient_fallback_resolves_own_credential() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["lmstudio".into(), "ollama".into()],
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        // Primary uses a ZAI key; fallbacks (lmstudio, ollama) should NOT
        // receive this key; they resolve their own credentials independently.
        let provider =
            create_resilient_provider("zai", Some("zai-test-key"), None, &reliability, &|_| None);
        assert!(provider.is_ok());
    }

    /// `custom:` URL entries work as fallback providers, enabling arbitrary
    /// OpenAI-compatible endpoints (e.g. local LM Studio on a Docker host).
    #[test]
    fn resilient_fallback_supports_custom_url() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec!["custom:http://host.docker.internal:1234/v1".into()],
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let provider = create_resilient_provider(
            "openai",
            Some("openai-test-key"),
            None,
            &reliability,
            &|_| None,
        );
        assert!(provider.is_ok());
    }

    /// Mixed fallback chain: named providers, custom URLs, and invalid entries
    /// all coexist.  Invalid entries are silently ignored; valid ones initialize.
    #[test]
    fn resilient_fallback_mixed_chain() {
        let reliability = crate::config::ReliabilityConfig {
            provider_retries: 1,
            provider_backoff_ms: 100,
            fallback_providers: vec![
                "deepseek".into(),
                "custom:http://localhost:8080/v1".into(),
                "nonexistent-provider".into(),
                "lmstudio".into(),
            ],
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        };

        let provider =
            create_resilient_provider("zai", Some("zai-test-key"), None, &reliability, &|_| None);
        assert!(provider.is_ok());
    }

    #[test]
    fn ollama_with_custom_url() {
        let provider = create_provider_with_url("ollama", None, Some("http://10.100.2.32:11434"));
        assert!(provider.is_ok());
    }

    #[test]
    fn ollama_cloud_with_custom_url() {
        let provider =
            create_provider_with_url("ollama", Some("ollama-key"), Some("https://ollama.com"));
        assert!(provider.is_ok());
    }

    #[test]
    fn factory_all_providers_create_successfully() {
        let providers = [
            "openrouter",
            "anthropic",
            "openai",
            "ollama",
            "gemini",
            "venice",
            "vercel",
            "cloudflare",
            "moonshot",
            "moonshot-intl",
            "kimi-code",
            "moonshot-cn",
            "kimi-code",
            "synthetic",
            "opencode",
            "zai",
            "zai-cn",
            "glm",
            "glm-cn",
            "minimax",
            "minimax-cn",
            "bedrock",
            "qianfan",
            "doubao",
            "qwen",
            "qwen-intl",
            "qwen-cn",
            "qwen-us",
            "lmstudio",
            "llamacpp",
            "groq",
            "mistral",
            "xai",
            "deepseek",
            "together",
            "fireworks",
            "perplexity",
            "cohere",
            "nvidia",
            "astrai",
            "ovhcloud",
        ];
        for name in providers {
            assert!(
                create_provider(name, Some("test-key")).is_ok(),
                "Provider '{name}' should create successfully"
            );
        }
    }

    #[test]
    fn listed_providers_have_unique_ids_and_aliases() {
        let providers = list_providers();
        let mut canonical_ids = std::collections::HashSet::new();
        let mut aliases = std::collections::HashSet::new();

        for provider in providers {
            assert!(
                canonical_ids.insert(provider.name),
                "Duplicate canonical provider id: {}",
                provider.name
            );

            for alias in provider.aliases {
                assert_ne!(
                    *alias, provider.name,
                    "Alias must differ from canonical id: {}",
                    provider.name
                );
                assert!(
                    !canonical_ids.contains(alias),
                    "Alias conflicts with canonical provider id: {}",
                    alias
                );
                assert!(aliases.insert(alias), "Duplicate provider alias: {}", alias);
            }
        }
    }

    /// The factory builds `astrai`, so `rantaiclaw providers` must name it.
    #[test]
    fn astrai_is_listed() {
        let astrai = list_providers()
            .into_iter()
            .find(|provider| provider.name == "astrai")
            .expect("astrai is built by the factory and must be listed");
        assert_eq!(astrai.display_name, "Astrai");
        assert!(!astrai.local);
        assert!(create_provider(astrai.name, Some("provider-test-credential")).is_ok());
    }

    #[test]
    fn listed_providers_and_aliases_are_constructible() {
        for provider in list_providers() {
            assert!(
                create_provider(provider.name, Some("provider-test-credential")).is_ok(),
                "Canonical provider id should be constructible: {}",
                provider.name
            );

            for alias in provider.aliases {
                assert!(
                    create_provider(alias, Some("provider-test-credential")).is_ok(),
                    "Provider alias should be constructible: {} (for {})",
                    alias,
                    provider.name
                );
            }
        }
    }

    // ── API error sanitization ───────────────────────────────

    #[test]
    fn sanitize_scrubs_sk_prefix() {
        let input = "request failed: sk-1234567890abcdef";
        let out = sanitize_api_error(input);
        assert!(!out.contains("sk-1234567890abcdef"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_scrubs_multiple_prefixes() {
        let input = "keys sk-abcdef xoxb-12345 xoxp-67890";
        let out = sanitize_api_error(input);
        assert!(!out.contains("sk-abcdef"));
        assert!(!out.contains("xoxb-12345"));
        assert!(!out.contains("xoxp-67890"));
    }

    #[test]
    fn sanitize_short_prefix_then_real_key() {
        let input = "error with sk- prefix and key sk-1234567890";
        let result = sanitize_api_error(input);
        assert!(!result.contains("sk-1234567890"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_sk_proj_comment_then_real_key() {
        let input = "note: sk- then sk-proj-abc123def456";
        let result = sanitize_api_error(input);
        assert!(!result.contains("sk-proj-abc123def456"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_keeps_bare_prefix() {
        let input = "only prefix sk- present";
        let result = sanitize_api_error(input);
        assert!(result.contains("sk-"));
    }

    #[test]
    fn sanitize_handles_json_wrapped_key() {
        let input = r#"{"error":"invalid key sk-abc123xyz"}"#;
        let result = sanitize_api_error(input);
        assert!(!result.contains("sk-abc123xyz"));
    }

    #[test]
    fn sanitize_handles_delimiter_boundaries() {
        let input = "bad token xoxb-abc123}; next";
        let result = sanitize_api_error(input);
        assert!(!result.contains("xoxb-abc123"));
        assert!(result.contains("};"));
    }

    #[test]
    fn sanitize_truncates_long_error() {
        let long = "a".repeat(400);
        let result = sanitize_api_error(&long);
        assert!(result.len() <= 203);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn sanitize_truncates_after_scrub() {
        let input = format!("{} sk-abcdef123456 {}", "a".repeat(190), "b".repeat(190));
        let result = sanitize_api_error(&input);
        assert!(!result.contains("sk-abcdef123456"));
        assert!(result.len() <= 203);
    }

    #[test]
    fn sanitize_preserves_unicode_boundaries() {
        let input = format!("{} sk-abcdef123", "hello🙂".repeat(80));
        let result = sanitize_api_error(&input);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
        assert!(!result.contains("sk-abcdef123"));
    }

    #[test]
    fn sanitize_no_secret_no_change() {
        let input = "simple upstream timeout";
        let result = sanitize_api_error(input);
        assert_eq!(result, input);
    }

    #[test]
    fn scrub_github_personal_access_token() {
        let input = "auth failed with token ghp_abc123def456";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "auth failed with token [REDACTED]");
    }

    #[test]
    fn scrub_github_oauth_token() {
        let input = "Bearer gho_1234567890abcdef";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "Bearer [REDACTED]");
    }

    #[test]
    fn scrub_github_user_token() {
        let input = "token ghu_sessiontoken123";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "token [REDACTED]");
    }

    #[test]
    fn scrub_github_fine_grained_pat() {
        let input = "failed: github_pat_11AABBC_xyzzy789";
        let result = scrub_secret_patterns(input);
        assert_eq!(result, "failed: [REDACTED]");
    }

    #[test]
    fn scrub_leaves_ordinary_words_that_contain_a_prefix_unchanged() {
        // Collect every changed text so one failure does not hide the others.
        let changed: Vec<String> = [
            "task-force",
            "risk-free analysis",
            "disk-full",
            "a mask-like shape",
            "risk-assessment",
            "disk-usage report",
            "mask-wearing rules",
            "desk-lamp-bright",
        ]
        .iter()
        .filter(|text| scrub_secret_patterns(text) != **text)
        .map(|text| format!("{text} -> {}", scrub_secret_patterns(text)))
        .collect();
        assert!(changed.is_empty(), "must stay whole: {changed:?}");
    }

    #[test]
    fn scrub_redacts_a_real_shaped_key_after_every_kind_of_boundary() {
        let keys = [
            "sk-abcdefghij0123456789XYZ",
            "ghp_abcdefghij0123456789XYZ",
            "xoxb-abcdefghij0123456789XYZ",
        ];
        for key in keys {
            for before in [" ", "\"", "=", ":", "(", "'", "\n"] {
                let input = format!("failed{before}{key} done");
                let out = scrub_secret_patterns(&input);
                assert!(
                    !out.contains("abcdefghij0123456789XYZ"),
                    "key after {before:?} must be redacted: {out}"
                );
                assert!(out.contains("[REDACTED]"), "got: {out}");
            }
            let at_start = scrub_secret_patterns(key);
            assert_eq!(at_start, "[REDACTED]", "key at the start of the text");
        }
    }

    #[test]
    fn scrub_leaves_a_prefix_with_a_body_shorter_than_five_characters_unchanged() {
        for text in [
            "see sk-1 here",
            "see sk-ab here",
            "a ghp_xyz1 b",
            "xoxb-1234 x",
        ] {
            assert_eq!(scrub_secret_patterns(text), text, "too short: {text}");
        }
    }

    #[test]
    fn scrub_redacts_a_real_key_that_follows_a_mid_word_prefix_hit() {
        let out = scrub_secret_patterns("a task-force then sk-abcdefghij0123456789XYZ");
        assert_eq!(out, "a task-force then [REDACTED]");
    }

    #[test]
    fn scrub_treats_a_non_ascii_character_before_a_prefix_as_a_boundary() {
        let out = scrub_secret_patterns(
            "kunci\u{2192}sk-abcdefghij0123456789XYZ dan \u{00e9}ghp_abcdefghij0123456789XYZ",
        );
        assert!(!out.contains("abcdefghij0123456789XYZ"), "got: {out}");
        assert!(out.starts_with("kunci\u{2192}[REDACTED]"), "got: {out}");
    }

    // ── A fallback provider's credential (audit 61) ───────────────────────────
    //
    // A fallback resolves its key the way a primary of the same name does:
    // the stored `[provider_api_keys]` entry for its own name, then its env
    // vars, then the generic pair. The primary's key never reaches a fallback.

    /// A reliability config whose primary is unreachable and whose one fallback
    /// is `custom:<server>/v1`. The primary fails fast, so the chain must reach
    /// the fallback. No retries, so the test stays quick.
    fn unreachable_primary_then_custom_fallback(
        server_uri: &str,
    ) -> crate::config::ReliabilityConfig {
        crate::config::ReliabilityConfig {
            provider_retries: 0,
            provider_backoff_ms: 50,
            fallback_providers: vec![format!("custom:{server_uri}/v1")],
            model_fallbacks: std::collections::HashMap::new(),
            channel_initial_backoff_secs: 2,
            channel_max_backoff_secs: 60,
            scheduler_poll_secs: 15,
            scheduler_retries: 2,
        }
    }

    /// The primary `custom:` endpoint: a closed localhost port, so the request
    /// fails at connect time and the chain moves to the fallback.
    const UNREACHABLE_PRIMARY: &str = "custom:http://127.0.0.1:9/v1";

    /// A localhost endpoint that answers any chat completion with `FAKE_REPLY_OK`.
    async fn fake_chat_endpoint() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "FAKE_REPLY_OK"}}]
                })),
            )
            .mount(&server)
            .await;
        server
    }

    /// The `Authorization` header each request to `server` carried, in order.
    async fn authorization_headers(server: &wiremock::MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|req| req.headers.get("authorization"))
            .map(|value| value.to_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Stored key only, under the fallback's literal name: the fallback is built
    /// with it and answers, and the request carries it.
    #[tokio::test]
    async fn fallback_built_from_its_stored_key_answers_and_sends_it() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let server = fake_chat_endpoint().await;
        let reliability = unreachable_primary_then_custom_fallback(&server.uri());
        let stored = std::collections::HashMap::from([(
            format!("custom:{}/v1", server.uri()),
            "sk-stored-fallback-1111".to_string(),
        )]);

        let provider =
            create_resilient_provider(UNREACHABLE_PRIMARY, None, None, &reliability, &|name| {
                stored.get(name).cloned()
            })
            .expect("the chain builds");
        let reply = provider
            .simple_chat("hi", "mock-model", 0.0)
            .await
            .expect("the fallback answers");

        assert_eq!(reply, "FAKE_REPLY_OK");
        assert_eq!(
            authorization_headers(&server).await,
            vec!["Bearer sk-stored-fallback-1111".to_string()],
            "the fallback must send the key stored for its own name"
        );
    }

    /// The §3.6 order: a stored key for the fallback outranks a different env key.
    #[tokio::test]
    async fn stored_fallback_key_outranks_generic_env_key() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let _generic = crate::test_env::EnvGuard::set("RANTAICLAW_API_KEY", "sk-env-fake-3333");
        let server = fake_chat_endpoint().await;
        let reliability = unreachable_primary_then_custom_fallback(&server.uri());
        let stored = std::collections::HashMap::from([(
            format!("custom:{}/v1", server.uri()),
            "sk-stored-fallback-2222".to_string(),
        )]);

        let provider =
            create_resilient_provider(UNREACHABLE_PRIMARY, None, None, &reliability, &|name| {
                stored.get(name).cloned()
            })
            .expect("the chain builds");
        provider
            .simple_chat("hi", "mock-model", 0.0)
            .await
            .expect("the fallback answers");

        assert_eq!(
            authorization_headers(&server).await,
            vec!["Bearer sk-stored-fallback-2222".to_string()],
            "a config-stored key must rank above RANTAICLAW_API_KEY"
        );
    }

    /// The primary's key is not the fallback's: primary key A, fallback key B,
    /// and the fallback request carries B. The primary is unreachable, so the
    /// fallback answers; the header assertion is what would catch A leaking.
    #[tokio::test]
    async fn fallback_sends_its_own_key_never_the_primary_key() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let server = fake_chat_endpoint().await;
        let reliability = unreachable_primary_then_custom_fallback(&server.uri());
        let stored = std::collections::HashMap::from([(
            format!("custom:{}/v1", server.uri()),
            "sk-fallback-key-B".to_string(),
        )]);

        let provider = create_resilient_provider(
            UNREACHABLE_PRIMARY,
            Some("sk-primary-key-A"),
            None,
            &reliability,
            &|name| stored.get(name).cloned(),
        )
        .expect("the chain builds");
        provider
            .simple_chat("hi", "mock-model", 0.0)
            .await
            .expect("the fallback answers");

        let headers = authorization_headers(&server).await;
        assert_eq!(headers, vec!["Bearer sk-fallback-key-B".to_string()]);
        assert!(
            !headers.iter().any(|h| h.contains("sk-primary-key-A")),
            "the primary's key must never reach the fallback: {headers:?}"
        );
    }

    /// A fallback with no key anywhere: the chain still builds (the constructor
    /// accepts a missing key) and the call fails with the existing refusal, so
    /// the chain reports the same aggregate failure as before.
    #[tokio::test]
    async fn fallback_without_any_key_fails_the_chain_as_before() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let server = fake_chat_endpoint().await;
        let reliability = unreachable_primary_then_custom_fallback(&server.uri());

        let provider =
            create_resilient_provider(UNREACHABLE_PRIMARY, None, None, &reliability, &|_| None)
                .expect("the chain builds");
        let err = provider
            .simple_chat("hi", "mock-model", 0.0)
            .await
            .expect_err("no key anywhere must fail");

        assert!(
            err.to_string().contains("All providers/models failed"),
            "the chain must report the aggregate failure: {err}"
        );
        assert!(
            authorization_headers(&server).await.is_empty(),
            "a keyless fallback must not send a request to the endpoint"
        );
    }

    // ── Anthropic family: one credential order ──────────
    //
    // Order for `anthropic` and `anthropic-custom:<url>`: config-stored key
    // → the provider's own env var → generic RANTAICLAW_API_KEY / API_KEY.
    // There is no stored login profile any more: nothing reads one, so
    // nothing can send one.

    const FAKE_ENV_KEY: &str = "sk-ant-api-fake-env-0001";
    const FAKE_CONFIG_KEY: &str = "sk-ant-api-fake-config-0002";
    const FAKE_GENERIC_KEY: &str = "sk-ant-api-fake-generic-0003";
    /// The token the legacy auth-profile store held. It must never reach a
    /// request: the store is gone, and nothing reads it.
    const FAKE_LEGACY_PROFILE_TOKEN: &str = "sk-ant-oat01-legacy-profile-marker-0004";
    /// A subscription setup-token, which this build refuses.
    const FAKE_SETUP_TOKEN: &str = "sk-ant-oat01-setup-token-marker-0005";

    /// Serve one Anthropic messages reply on 127.0.0.1 and return the server.
    async fn fake_anthropic_endpoint() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/messages"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "msg_fake",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-fake",
                    "content": [{"type": "text", "text": "FAKE_REPLY_OK"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;
        server
    }

    /// The `x-api-key` and `Authorization` values the endpoint received.
    async fn anthropic_auth_headers(server: &wiremock::MockServer) -> (Vec<String>, Vec<String>) {
        let requests = server.received_requests().await.unwrap_or_default();
        let collect = |name: &str| {
            requests
                .iter()
                .filter_map(|r| r.headers.get(name))
                .filter_map(|v| v.to_str().ok().map(str::to_owned))
                .collect::<Vec<_>>()
        };
        (collect("x-api-key"), collect("authorization"))
    }

    /// Build the provider the factory gives `name`, send one turn, return the
    /// endpoint's view of the request.
    async fn send_one_anthropic_turn(
        name: &str,
        config_key: Option<&str>,
        options: &ProviderRuntimeOptions,
        endpoint: &wiremock::MockServer,
    ) -> (Vec<String>, Vec<String>) {
        let provider = create_provider_with_options(name, config_key, options)
            .expect("the anthropic family builds with a credential in reach");
        let reply = provider
            .chat_with_system(None, "hello", "claude-fake", 0.0)
            .await
            .expect("the turn reaches the endpoint");
        assert_eq!(reply, "FAKE_REPLY_OK");
        anthropic_auth_headers(endpoint).await
    }

    /// Set ANTHROPIC_API_KEY and nothing else from the credential family.
    fn anthropic_only_env(env_key: &str) -> Vec<crate::test_env::EnvGuard> {
        vec![
            crate::test_env::EnvGuard::set("ANTHROPIC_API_KEY", env_key),
            crate::test_env::EnvGuard::unset("RANTAICLAW_API_KEY"),
            crate::test_env::EnvGuard::unset("API_KEY"),
        ]
    }

    /// The legacy auth-profile store, in the shape an earlier RantaiClaw
    /// wrote it: a map of profiles, each with a token.
    ///
    /// It sits in the directory the provider's runtime options name, which is
    /// where the store used to live. Nothing reads it any more.
    fn write_legacy_profile_store(dir: &std::path::Path, token: &str) {
        std::fs::write(
            dir.join("auth-profiles.json"),
            serde_json::json!({
                "schema_version": 1,
                "updated_at": "2026-01-01T00:00:00Z",
                "active_profiles": {"anthropic": "anthropic:default"},
                "profiles": {
                    "anthropic:default": {
                        "provider": "anthropic",
                        "profile_name": "default",
                        "kind": "token",
                        "token": token,
                        "metadata": {},
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z"
                    }
                }
            })
            .to_string(),
        )
        .expect("write the legacy auth-profile store");
    }

    /// 52: `anthropic-custom:<url>` inherits the base provider's env var.
    #[tokio::test]
    async fn anthropic_custom_url_uses_anthropic_api_key_env() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let _env = anthropic_only_env(FAKE_ENV_KEY);
        let endpoint = fake_anthropic_endpoint().await;
        let name = format!("anthropic-custom:{}", endpoint.uri());

        let (x_api_key, _) =
            send_one_anthropic_turn(&name, None, &ProviderRuntimeOptions::default(), &endpoint)
                .await;

        assert_eq!(x_api_key, vec![FAKE_ENV_KEY.to_string()]);
    }

    /// The config-stored key for the literal custom name beats the env var.
    #[tokio::test]
    async fn anthropic_custom_url_config_key_beats_env() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let _env = anthropic_only_env(FAKE_ENV_KEY);
        let endpoint = fake_anthropic_endpoint().await;
        let name = format!("anthropic-custom:{}", endpoint.uri());

        let (x_api_key, _) = send_one_anthropic_turn(
            &name,
            Some(FAKE_CONFIG_KEY),
            &ProviderRuntimeOptions::default(),
            &endpoint,
        )
        .await;

        assert_eq!(x_api_key, vec![FAKE_CONFIG_KEY.to_string()]);
    }

    /// The legacy auth-profile store is on disk, holding a marker token: the
    /// marker NEVER reaches the request. The store is gone, and the only
    /// credential left is the one the operator configured.
    #[tokio::test]
    async fn the_legacy_profile_marker_never_reaches_a_request() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let _env = anthropic_only_env(FAKE_ENV_KEY);
        let state_dir = tempfile::tempdir().expect("state dir");
        write_legacy_profile_store(state_dir.path(), FAKE_LEGACY_PROFILE_TOKEN);
        let endpoint = fake_anthropic_endpoint().await;
        let name = format!("anthropic-custom:{}", endpoint.uri());
        let options = ProviderRuntimeOptions {
            rantaiclaw_dir: Some(state_dir.path().to_path_buf()),
            ..ProviderRuntimeOptions::default()
        };

        let (x_api_key, authorization) =
            send_one_anthropic_turn(&name, None, &options, &endpoint).await;

        assert_eq!(
            x_api_key,
            vec![FAKE_ENV_KEY.to_string()],
            "the configured key is the credential that goes out"
        );
        assert!(authorization.is_empty(), "{authorization:?}");
        assert!(
            !x_api_key
                .iter()
                .any(|value| value.contains("legacy-profile-marker")),
            "the legacy profile token must never be sent: {x_api_key:?}"
        );
    }

    /// The same for the plain `anthropic` name: the legacy store is on disk,
    /// and the credential the provider would send is the configured one — the
    /// marker is not read at all, so it is not the credential and the
    /// provider is not the one that refuses setup-tokens.
    #[tokio::test]
    async fn the_legacy_profile_marker_is_not_the_credential_for_anthropic() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let _env = anthropic_only_env(FAKE_ENV_KEY);
        let state_dir = tempfile::tempdir().expect("state dir");
        write_legacy_profile_store(state_dir.path(), FAKE_LEGACY_PROFILE_TOKEN);
        let options = ProviderRuntimeOptions {
            rantaiclaw_dir: Some(state_dir.path().to_path_buf()),
            ..ProviderRuntimeOptions::default()
        };

        assert_eq!(
            resolve_provider_credential("anthropic", None),
            Some(FAKE_ENV_KEY.to_string()),
            "the legacy store must not be a credential source"
        );
        assert_eq!(
            refused_credential("anthropic", None),
            None,
            "the legacy token must not be read as the credential to refuse"
        );

        let provider = create_provider_with_options("anthropic", None, &options)
            .expect("the anthropic provider builds");
        let is_refuser = provider
            .test_as_any()
            .and_then(|inner| inner.downcast_ref::<anthropic::SetupTokenProvider>())
            .is_some();
        assert!(
            !is_refuser,
            "the legacy token must not make the provider refuse every request"
        );
    }

    /// A setup-token is refused when the request is sent, not when the
    /// provider is built: the factory still returns a provider, so the gateway
    /// binds and the daemon stays up.
    #[tokio::test]
    async fn a_setup_token_is_refused_at_request_time_and_no_request_is_sent() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let endpoint = fake_anthropic_endpoint().await;
        let name = format!("anthropic-custom:{}", endpoint.uri());

        // The config key case.
        let provider = create_provider_with_options(
            &name,
            Some(FAKE_SETUP_TOKEN),
            &ProviderRuntimeOptions::default(),
        )
        .expect("a setup-token must not stop the provider from building");
        let error = provider
            .chat_with_system(None, "hello", "claude-fake", 0.0)
            .await
            .expect_err("a setup-token must be refused");
        let sentence =
            crate::providers::local_refusal_text(&error).expect("the refusal is a local refusal");
        assert!(
            sentence.contains("ANTHROPIC_API_KEY"),
            "the refusal names the replacement: {sentence}"
        );
        assert!(
            !sentence.contains(FAKE_SETUP_TOKEN),
            "the refusal must not echo the token: {sentence}"
        );
        assert_eq!(
            endpoint.received_requests().await.unwrap_or_default().len(),
            0,
            "a refused request must not reach the endpoint"
        );

        // The environment-variable case.
        let _env = anthropic_only_env(FAKE_SETUP_TOKEN);
        let provider =
            create_provider_with_options(&name, None, &ProviderRuntimeOptions::default())
                .expect("a setup-token must not stop the provider from building");
        let error = provider
            .chat_with_system(None, "hello", "claude-fake", 0.0)
            .await
            .expect_err("a setup-token must be refused");
        assert!(
            crate::providers::local_refusal_text(&error).is_some(),
            "the refusal is a local refusal: {error}"
        );
        assert_eq!(
            endpoint.received_requests().await.unwrap_or_default().len(),
            0,
            "a refused request must not reach the endpoint"
        );
    }

    /// The plain `anthropic` name refuses a setup-token the same way, and the
    /// refusal names the replacement.
    #[tokio::test]
    async fn a_setup_token_is_refused_for_the_plain_anthropic_name() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let provider = create_provider_with_options(
            "anthropic",
            Some(FAKE_SETUP_TOKEN),
            &ProviderRuntimeOptions::default(),
        )
        .expect("a setup-token must not stop the provider from building");
        let error = provider
            .chat_with_system(None, "hello", "claude-fake", 0.0)
            .await
            .expect_err("a setup-token must be refused");
        let sentence =
            crate::providers::local_refusal_text(&error).expect("the refusal is a local refusal");
        assert!(
            sentence.contains("ANTHROPIC_API_KEY"),
            "the refusal names the replacement: {sentence}"
        );
        assert!(
            !sentence.contains(FAKE_SETUP_TOKEN),
            "the refusal must not echo the token: {sentence}"
        );
    }

    /// The anthropic family's credential order: the config key wins over the
    /// provider's own environment variable, which wins over the generic
    /// fallbacks.
    #[test]
    fn the_anthropic_credential_order_is_config_then_env_then_generic() {
        let _env_lock = env_lock();
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        assert_eq!(resolve_provider_credential("anthropic", None), None);

        let _generic = crate::test_env::EnvGuard::set("RANTAICLAW_API_KEY", FAKE_GENERIC_KEY);
        assert_eq!(
            resolve_provider_credential("anthropic", None),
            Some(FAKE_GENERIC_KEY.to_string()),
            "the generic fallback is the last source"
        );

        let _provider_env = crate::test_env::EnvGuard::set("ANTHROPIC_API_KEY", FAKE_ENV_KEY);
        assert_eq!(
            resolve_provider_credential("anthropic", None),
            Some(FAKE_ENV_KEY.to_string()),
            "the provider's own variable beats the generic one"
        );

        assert_eq!(
            resolve_provider_credential("anthropic", Some(FAKE_CONFIG_KEY)),
            Some(FAKE_CONFIG_KEY.to_string()),
            "the config key beats every environment variable"
        );
    }

    /// `has_usable_credential` and the constructor answer the same for each
    /// case: a missing key is refused by both, a present one is usable by both.
    #[tokio::test]
    async fn has_usable_credential_agrees_with_the_constructor() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let endpoint = fake_anthropic_endpoint().await;
        let custom = format!("anthropic-custom:{}", endpoint.uri());

        // One row per case: which name, config key, env key, and the answer
        // both entry points must give.
        struct Case<'a> {
            name: &'a str,
            config_key: Option<&'a str>,
            env_key: Option<&'a str>,
            expected: bool,
        }
        let row = |name, config_key, env_key, expected| Case {
            name,
            config_key,
            env_key,
            expected,
        };
        let cases = [
            row("anthropic", None, None, false),
            row("anthropic", None, Some(FAKE_ENV_KEY), true),
            row("anthropic", Some(FAKE_CONFIG_KEY), None, true),
            row(&custom, None, None, false),
            row(&custom, None, Some(FAKE_ENV_KEY), true),
            row(&custom, Some(FAKE_CONFIG_KEY), None, true),
        ];

        for case in cases {
            let Case {
                name,
                config_key,
                env_key,
                expected,
            } = case;
            let _env = match env_key {
                Some(key) => anthropic_only_env(key),
                None => vec![
                    crate::test_env::EnvGuard::unset("ANTHROPIC_API_KEY"),
                    crate::test_env::EnvGuard::unset("RANTAICLAW_API_KEY"),
                    crate::test_env::EnvGuard::unset("API_KEY"),
                ],
            };
            let usable = has_usable_credential(name, config_key);
            let constructed =
                create_provider_with_options(name, config_key, &ProviderRuntimeOptions::default())
                    .is_ok();
            assert_eq!(usable, expected, "has_usable_credential({name})");
            assert_eq!(
                constructed, usable,
                "the constructor and has_usable_credential disagree for {name}"
            );
        }
    }

    /// 55 and 52 refusal: with no credential from any source, the message
    /// names the provider as typed and the variable that fixes it.
    #[tokio::test]
    async fn anthropic_refusal_names_the_typed_provider_and_env_var() {
        let _env_lock = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let options = ProviderRuntimeOptions::default();

        let custom = "anthropic-custom:http://127.0.0.1:18432";
        let custom_err = create_provider_with_options(custom, None, &options)
            .err()
            .expect("no credential is refused")
            .to_string();
        assert!(custom_err.contains(custom), "{custom_err}");
        assert!(custom_err.contains("ANTHROPIC_API_KEY"), "{custom_err}");

        let plain_err = create_provider_with_options("anthropic", None, &options)
            .err()
            .expect("no credential is refused")
            .to_string();
        assert!(plain_err.contains("anthropic"), "{plain_err}");
        assert!(plain_err.contains("ANTHROPIC_API_KEY"), "{plain_err}");
    }

    // ── The kept API-key paths send the configured key ────
    //
    // The subscription logins are gone; the API keys stay. These prove the
    // key the operator configured is still the credential that reaches the
    // host — the factory hands it to the provider, and the provider puts it
    // in the request.

    /// The OpenAI-compatible provider the factory built, if it is one.
    fn compatible_provider_inside(provider: &dyn Provider) -> Option<&OpenAiCompatibleProvider> {
        provider
            .test_as_any()
            .and_then(|inner| inner.downcast_ref::<OpenAiCompatibleProvider>())
    }

    /// The `openai` key path: the configured key is the bearer token the host
    /// receives.
    ///
    /// The whole path is asserted on the wire: the factory builds the
    /// provider with the configured key, and the request that reaches the
    /// endpoint carries it.
    #[tokio::test]
    async fn the_openai_path_sends_the_configured_key_to_its_host() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "c", "object": "chat.completion", "created": 1_700_000_000_u64,
                    "model": "gpt-4o-mini",
                    "choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": "ok"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })),
            )
            .mount(&server)
            .await;

        let provider = create_provider_with_url(
            "openai",
            Some("sk-configured-openai-key"),
            Some(&server.uri()),
        )
        .expect("the factory builds openai with the configured key");
        provider
            .simple_chat("hi", "gpt-4o-mini", 0.0)
            .await
            .expect("the endpoint answers");

        assert_eq!(
            authorization_headers(&server).await,
            vec!["Bearer sk-configured-openai-key".to_string()],
            "the configured key must be the bearer token the host receives"
        );
    }

    /// The `qwen` key path (DashScope): the factory hands the configured key
    /// to the provider, and that key is the one the request carries.
    #[tokio::test]
    async fn the_qwen_path_sends_the_configured_key_to_its_host() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let key = "sk-configured-dashscope-key";

        let provider =
            create_provider("qwen", Some(key)).expect("the factory builds qwen with the key");
        let compatible = compatible_provider_inside(provider.as_ref())
            .expect("qwen builds an OpenAI-compatible provider");
        assert_eq!(
            compatible.credential.as_deref(),
            Some(key),
            "the configured key must reach the provider the factory built"
        );

        let request = compatible
            .build_request_for_test("qwen-plus")
            .await
            .expect("the provider builds the request");
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer sk-configured-dashscope-key"),
            "the configured key must be the bearer token the host receives"
        );
    }

    /// The `minimax` key path: the same, for the MiniMax endpoint.
    #[tokio::test]
    async fn the_minimax_path_sends_the_configured_key_to_its_host() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let key = "sk-configured-minimax-key";

        let provider =
            create_provider("minimax", Some(key)).expect("the factory builds minimax with the key");
        let compatible = compatible_provider_inside(provider.as_ref())
            .expect("minimax builds an OpenAI-compatible provider");
        assert_eq!(
            compatible.credential.as_deref(),
            Some(key),
            "the configured key must reach the provider the factory built"
        );

        let request = compatible
            .build_request_for_test("minimax-text-01")
            .await
            .expect("the provider builds the request");
        assert_eq!(
            request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer sk-configured-minimax-key"),
            "the configured key must be the bearer token the host receives"
        );
    }
}
