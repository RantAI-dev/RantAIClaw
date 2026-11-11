# RantaiClaw Providers Reference

This document maps provider IDs, aliases, and credential environment variables.

Last verified: **November 6, 2026**.

## How to List Providers

```bash
rantaiclaw providers
```

## Credential Resolution Order

Runtime resolution order is:

1. Explicit credential from config/CLI
2. Provider-specific env var(s)
3. Generic fallback env vars: `RANTAICLAW_API_KEY` then `API_KEY`

For resilient fallback chains (`reliability.fallback_providers`), each fallback
resolves its credential by its own name, in this order:

1. The key stored in `[provider_api_keys]` for that fallback's name (the canonical
   name, then the name as written in `fallback_providers`).
2. That provider's env vars, as in step 2 above.
3. The generic `RANTAICLAW_API_KEY` / `API_KEY`.

The primary provider's key is never reused for a fallback. A key for one endpoint
sent to another is a credential leak, so a fallback with no key anywhere is refused at
call time with `<provider> API key not set`, and the chain moves on.

When a fallback answers, the CLI agent (`agent -m` and the REPL) prints one line on
stdout before the reply, naming the provider it asked for and the fallback that
answered:

```text
note: the reply came from fallback provider custom:http://127.0.0.1:18431/v1 (asked for custom:http://127.0.0.1:9/v1)
```

A reply from the primary prints no such line. Channels and the gateway do not print
it; the retry and failover detail is on stderr as before.

Compatibility note: config-stored provider keys, including encrypted keys, remain
above environment variables in credential precedence. Use env vars when no key is
stored in config, or remove the config key before relying on an env override.

### Anthropic family credential order

`anthropic` and `anthropic-custom:<url>` resolve their credential from one list, in
this order:

1. The key stored in config for the name you typed: its `[provider_api_keys]` entry,
   under the canonical name or the name as written, and the top-level `api_key` only
   when that name is the default provider.
2. The env var `ANTHROPIC_API_KEY`.
3. The generic `RANTAICLAW_API_KEY`, then `API_KEY`.

There is no fourth source. RantaiClaw stores no subscription login, so no auth profile
is read, and `ANTHROPIC_OAUTH_TOKEN` is not read either. Whatever the source, the key
goes out as `x-api-key`.

`anthropic-custom:<url>` inherits the `ANTHROPIC_API_KEY` env var, so it works for a
custom Anthropic-format endpoint without a second key. Config is read under the custom
name as typed only: the `anthropic` entry in `[provider_api_keys]` and the top-level
`api_key` are not read for it. Every provider reads its own config entry; a key stored
for one provider is not sent to another, apart from the generic `RANTAICLAW_API_KEY` /
`API_KEY` fallback.

With no credential in any source, the run refuses and names the string you typed and
the variable to set, for example `anthropic-custom:<url>: no credential found — set
ANTHROPIC_API_KEY or store a key in config`.

A Claude subscription setup token is refused, and never sent. The prefix is the whole
rule: an Anthropic API key starts with `sk-ant-api` and a setup token with `sk-ant-oat`.
The refusal names the replacement:

```text
Anthropic setup-tokens are no longer accepted. Use an API key from the Claude Console: set ANTHROPIC_API_KEY or store the key in config.
```

The provider still builds, so the gateway binds and the daemon stays up; the refusal
comes when a request is sent.

## Provider Catalog

| Canonical ID | Aliases | Local | Provider-specific env var(s) |
|---|---|---:|---|
| `openrouter` | — | No | `OPENROUTER_API_KEY` |
| `anthropic` | — | No | `ANTHROPIC_API_KEY` |
| `openai` | — | No | `OPENAI_API_KEY` |
| `ollama` | — | Yes | `OLLAMA_API_KEY` (optional) |
| `gemini` | `google`, `google-gemini` | No | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| `venice` | — | No | `VENICE_API_KEY` |
| `vercel` | `vercel-ai` | No | `VERCEL_API_KEY` |
| `cloudflare` | `cloudflare-ai` | No | `CLOUDFLARE_API_KEY` |
| `moonshot` | `kimi` | No | `MOONSHOT_API_KEY` |
| `kimi-code` | `kimi_coding`, `kimi_for_coding` | No | `KIMI_CODE_API_KEY`, `MOONSHOT_API_KEY` |
| `synthetic` | — | No | `SYNTHETIC_API_KEY` |
| `opencode` | `opencode-zen` | No | `OPENCODE_API_KEY` |
| `zai` | `z.ai` | No | `ZAI_API_KEY` |
| `glm` | `zhipu` | No | `GLM_API_KEY` |
| `minimax` | `minimax-intl`, `minimax-io`, `minimax-global`, `minimax-cn`, `minimaxi`, `minimax-portal`, `minimax-portal-cn` | No | `MINIMAX_API_KEY` |
| `bedrock` | `aws-bedrock` | No | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (optional: `AWS_REGION`) |
| `qianfan` | `baidu` | No | `QIANFAN_API_KEY` |
| `doubao` | `volcengine`, `ark`, `doubao-cn` | No | `ARK_API_KEY`, `DOUBAO_API_KEY` |
| `qwen` | `dashscope`, `qwen-intl`, `dashscope-intl`, `qwen-us`, `dashscope-us` | No | `DASHSCOPE_API_KEY` |
| `groq` | — | No | `GROQ_API_KEY` |
| `mistral` | — | No | `MISTRAL_API_KEY` |
| `xai` | `grok` | No | `XAI_API_KEY` |
| `deepseek` | — | No | `DEEPSEEK_API_KEY` |
| `together` | `together-ai` | No | `TOGETHER_API_KEY` |
| `fireworks` | `fireworks-ai` | No | `FIREWORKS_API_KEY` |
| `perplexity` | — | No | `PERPLEXITY_API_KEY` |
| `cohere` | — | No | `COHERE_API_KEY` |
| `lmstudio` | `lm-studio` | Yes | (optional; local by default) |
| `llamacpp` | `llama.cpp` | Yes | `LLAMACPP_API_KEY` (optional; only if server auth is enabled) |
| `nvidia` | `nvidia-nim`, `build.nvidia.com` | No | `NVIDIA_API_KEY` |
| `ovhcloud` | `ovh` | No | `OVH_AI_ENDPOINTS_ACCESS_TOKEN` |
| `astrai` | — | No | `ASTRAI_API_KEY` |

The OpenAI-compatible, Anthropic, OpenRouter, Ollama and Bedrock providers
report the token counts that feed the daily token ceiling (`[cost] max_tokens_per_day`);
a provider that does not report them adds nothing to the day's total.

A request that a provider refuses before sending anything fails the turn at once, with no
retry and no backoff wait, because a retry would end in the same sentence. These refusals
are a missing AWS Bedrock or Gemini credential, and an Ollama `:cloud` model on a local
endpoint or without an API key.

### Removed Subscription Logins

RantaiClaw uses API keys only. Six subscription logins are gone, and every name that
stood for one is refused with the API-key replacement named:

| Removed login | Refusal |
|---|---|
| `copilot`, `github-copilot` | The GitHub Copilot login was removed. Copilot has no API-key path in RantaiClaw — choose another provider. |
| `openai-codex`, `openai_codex`, `codex` | The ChatGPT (OpenAI Codex) login was removed. Use the "openai" provider with an OPENAI_API_KEY instead. |
| `qwen-code`, `qwen-oauth`, `qwen_oauth` | The Qwen Code OAuth login was removed. Use the "qwen" provider with a DashScope API key (DASHSCOPE_API_KEY) instead. |
| `minimax-oauth`, `minimax-oauth-cn`, `minimax-oauth-global` | The MiniMax OAuth login was removed. Use the "minimax" provider with a MINIMAX_API_KEY instead. |

A config that still names one of them **loads**. The daemon starts, the gateway binds and
the console serves, so there is no restart loop; the refusal comes when a request is sent.
Nothing is sent to the vendor, the turn fails at once with the sentence above, and no retry
follows.

The same sentence answers a credential that asks for the login in the name of the key:
`api_key = "minimax-oauth"` and `api_key = "qwen-oauth"` are placeholders, not keys, and
the provider they sit on refuses with the sentence its own name would have produced.

`rantaiclaw doctor` reports a config that names one of them:

```text
default_provider "copilot" names a removed login: The GitHub Copilot login was removed. Copilot has no API-key path in RantaiClaw — choose another provider.
```

The gateway's chat route answers the same failure with `400` and the replacement sentence
in the error body. The streaming route answers `200` and carries the same sentence in its
error event, so a client that reads one route and compares it with the other finds one
message.

Under `[proxy]`, a service key of `provider.copilot` still loads — it stays a supported
key, so a config written before the removal is not rejected — but it is inert, and a
warning says why.

The sixth removed login is the Anthropic Claude subscription, and it is refused on the
credential rather than on a name: the `anthropic` provider stays, and it takes an API key
from the Claude Console. A setup token — a credential that starts with `sk-ant-oat` — is
refused with this sentence wherever it is found:

```text
Anthropic setup-tokens are no longer accepted. Use an API key from the Claude Console: set ANTHROPIC_API_KEY or store the key in config.
```

Nothing reaches Anthropic with such a credential, and a configured
`anthropic-custom:<url>` endpoint does not change that: the refusal is decided locally,
before a request is built. `rantaiclaw doctor` reports it as a failed credential check
rather than as a missing key, and the gateway's chat route answers the turn with `400`
and the same sentence in the error body. See
[Anthropic family credential order](#anthropic-family-credential-order).

### Gemini Notes

- Provider ID: `gemini` (aliases: `google`, `google-gemini`)
- Auth comes from an API key: the key stored in config, `GEMINI_API_KEY`, or `GOOGLE_API_KEY`
- The Gemini CLI login is gone. RantaiClaw uses API keys only, so `~/.gemini/oauth_creds.json` is
  never read and a login made with the `gemini` CLI cannot be reused.
- With no key the request is refused before anything is sent, with the fix named:

  ```text
  Gemini API key not found. Options:
  1. Set GEMINI_API_KEY env var
  2. Get an API key from https://aistudio.google.com/app/apikey
  3. Run `rantaiclaw onboard` to configure
  ```

- A refused request fails the turn at once — no retry, no backoff wait. The credential is decided
  locally, before anything is sent, so a retry could only end in the same sentence.
- API key requests use `generativelanguage.googleapis.com/v1beta`

### Image Input By Provider

Image markers (``[IMAGE:<source>]``) reach the model as image parts on these providers:

- `ollama` and `bedrock` send their own image parts.
- `anthropic`, `openai` and `gemini` (the rig path) send png, jpeg and webp; `anthropic` and `openai`
  also send gif. bmp is refused on all three.
- On a vision provider, a message whose image file cannot be read is refused before it enters the
  conversation, so the next message goes through.
- On the rig providers (`anthropic`, `openai`, `gemini`), a message naming an image type the
  provider's rig path cannot send (bmp on all three; gif on `gemini`) is refused before it enters the
  conversation, with the `MIME type is not allowed` error. The list is keyed on the primary
  provider. A fallback or model route that serves a turn is not checked against its own list, the
  same as the existing vision check.
- A provider that declares no vision input (for example `custom:<url>`) refuses the current turn's
  image with the capability error, and the conversation continues with the next message. Earlier
  images in the history are replaced with an omission note, not refused.
- `max_images` counts the images of the whole session, not of one message: a session already holding
  four images refuses the message that would add a fifth, whatever message carries it. The refusal
  says how many images the session holds, what the limit is, and that `/new` starts over.
- A refusal takes the message back in the terminal chat. The row leaves the screen and the session
  store, so `/undo`, `/retry` and a later resume cannot bring it back, its text returns to the input
  box, and the refusal is shown once, as one line. The console has no input box: it reports the
  refusal on the same event a failed turn uses, and stores nothing for the turn.
- The `[multimodal]` settings (`max_images`, `max_image_size_mb`, `allow_remote_fetch`) apply wherever
  a turn is sent: the terminal chat, the CLI, the channels, the gateway's chat turns and the console
  (`POST /api/v1/agent/chat`). Before, an agent turn prepared its images with the built-in defaults,
  so the section reached the channels and the gateway's chat turns only.

### Ollama Vision Notes

- Provider ID: `ollama`
- Vision input is supported through user message image markers: ``[IMAGE:<source>]``.
- After multimodal normalization, RantaiClaw sends image payloads through Ollama's native `messages[].images` field.
- If a non-vision provider is selected, RantaiClaw returns a structured capability error instead of silently ignoring images.

### Ollama Cloud Routing Notes

- Use `:cloud` model suffix only with a remote Ollama endpoint.
- Remote endpoint should be set in `api_url` (example: `https://ollama.com`).
- RantaiClaw normalizes a trailing `/api` in `api_url` automatically.
- If `default_model` ends with `:cloud` while `api_url` is local or unset, config validation fails early with an actionable error.
- Local Ollama model discovery intentionally excludes `:cloud` entries to avoid selecting cloud-only models in local mode.

### llama.cpp Server Notes

- Provider ID: `llamacpp` (alias: `llama.cpp`)
- Default endpoint: `http://localhost:8080/v1`
- API key is optional by default; set `LLAMACPP_API_KEY` only when `llama-server` is started with `--api-key`.
- Model discovery: `rantaiclaw models refresh --provider llamacpp`

### Bedrock Notes

- Provider ID: `bedrock` (alias: `aws-bedrock`)
- API: [Converse API](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html)
- Authentication: AWS AKSK (not a single API key). Set `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` environment variables.
- Optional: `AWS_SESSION_TOKEN` for temporary/STS credentials, `AWS_REGION` or `AWS_DEFAULT_REGION` (default: `us-east-1`).
- Default onboarding model: `anthropic.claude-sonnet-4-5-20250929-v1:0`
- Supports native tool calling and prompt caching (`cachePoint`).
- Cross-region inference profiles supported (e.g., `us.anthropic.claude-*`).
- Model IDs use Bedrock format: `anthropic.claude-sonnet-4-6`, `anthropic.claude-opus-4-6-v1`, etc.

### Ollama Reasoning Toggle

You can control Ollama reasoning/thinking behavior from `config.toml`:

```toml
[runtime]
reasoning_enabled = false
```

Behavior:

- `false`: sends `think: false` to Ollama `/api/chat` requests.
- `true`: sends `think: true`.
- Unset: omits `think` and keeps Ollama/model defaults.

### Kimi Code Notes

- Provider ID: `kimi-code`
- Endpoint: `https://api.kimi.com/coding/v1`
- Default onboarding model: `kimi-for-coding` (alternative: `kimi-k2.5`)
- Runtime auto-adds `User-Agent: KimiCLI/0.77` for compatibility.

### NVIDIA NIM Notes

- Canonical provider ID: `nvidia`
- Aliases: `nvidia-nim`, `build.nvidia.com`
- Base API URL: `https://integrate.api.nvidia.com/v1`
- Model discovery: `rantaiclaw models refresh --provider nvidia`

Recommended starter model IDs (verified against NVIDIA API catalog on February 18, 2026):

- `meta/llama-3.3-70b-instruct`
- `deepseek-ai/deepseek-v3.2`
- `nvidia/llama-3.3-nemotron-super-49b-v1.5`
- `nvidia/llama-3.1-nemotron-ultra-253b-v1`

## Custom Endpoints

- OpenAI-compatible endpoint:

```toml
default_provider = "custom:https://your-api.example.com"
```

- Anthropic-compatible endpoint:

```toml
default_provider = "anthropic-custom:https://your-api.example.com"
```

`rantaiclaw models refresh` lists the models of a custom endpoint:

- `custom:<url>` asks `<url>/models`, the sibling of the `/chat/completions` path that chat requests use. A base that already ends in `/chat/completions` is replaced by `/models`, and a bare origin such as `custom:http://host:8080` gives `http://host:8080/models`.
- `anthropic-custom:<url>` asks `<url>/v1/models`, the sibling of the `/v1/messages` path that chat requests use.
- A caller may name a custom endpoint only when it is configured. Wherever the id arrives from outside the config — the `--provider` flag of `rantaiclaw models refresh` or `rantaiclaw doctor models`, a gateway route, or a chat body — an unconfigured `custom:<url>` / `anthropic-custom:<url>` id is refused before anything is sent, and the refusal names `default_provider`, `[[model_routes]]`, `reliability.fallback_providers` and `provider_api_keys` as the places that accept it. Built-in names and `<base>-custom:` aliases aim at fixed hosts and keep working as before.
- The request carries the key chat requests send to that URL. For `custom:` that is the config key, then `RANTAICLAW_API_KEY`, then `API_KEY`. For `anthropic-custom:` it is the order in [Anthropic family credential order](#anthropic-family-credential-order). With no key, no request is sent, because chat does not run without one either.
- The list is cached under the provider string as written in config, so two custom endpoints keep separate lists.
- A failed request, a non-2xx answer, or an answer with no model IDs fails the refresh, leaves any earlier cache as it was, and ends with the `/model <id>` and `default_model` way forward.

## MiniMax API Key Setup (config.toml)

Set the MiniMax provider and its key in config:

```toml
default_provider = "minimax"
api_key = "sk-..."
```

Then provide the key through one of these:

- `MINIMAX_API_KEY` in the environment
- the `api_key` written above, or a `[provider_api_keys]` entry for the provider name

The MiniMax OAuth login is gone. `MINIMAX_OAUTH_TOKEN`, `MINIMAX_OAUTH_REFRESH_TOKEN`,
`MINIMAX_OAUTH_CLIENT_ID` and `MINIMAX_OAUTH_REGION` do nothing, and a config that names
`minimax-oauth` is refused at send time with the replacement named — see
[Removed Subscription Logins](#removed-subscription-logins).

Channel compatibility note:

- For MiniMax-backed channel conversations, runtime history is normalized to keep valid `user`/`assistant` turn order.
- Channel-specific delivery guidance (for example Telegram attachment markers) is merged into the leading system prompt instead of being appended as a trailing `system` turn.

## Qwen API Key Setup (config.toml)

Set the Qwen provider and its key in config:

```toml
default_provider = "qwen"
api_key = "sk-..."
```

Then provide the key through one of these:

- `DASHSCOPE_API_KEY` in the environment
- the `api_key` written above, or a `[provider_api_keys]` entry for the provider name

The Qwen Code OAuth login is gone. `QWEN_OAUTH_TOKEN`, `QWEN_OAUTH_REFRESH_TOKEN` and
`QWEN_OAUTH_RESOURCE_URL` do nothing, and `~/.qwen/oauth_creds.json` is never read. A
config that names `qwen-code`, or a credential placeholder such as `api_key = "qwen-oauth"`,
is refused at send time with the replacement named — see
[Removed Subscription Logins](#removed-subscription-logins).

## Model Routing (`hint:<name>`)

You can route model calls by hint using `[[model_routes]]`:

```toml
[[model_routes]]
hint = "reasoning"
provider = "openrouter"
model = "anthropic/claude-opus-4-20250514"

[[model_routes]]
hint = "fast"
provider = "groq"
model = "llama-3.3-70b-versatile"
```

Then call with a hint model name (for example from tool or integration paths):

```text
hint:reasoning
```

The hint resolves against the table at every door that runs an agent turn: the CLI and
TUI agent, cron, the daemon heartbeat, the console chat and the gateway webhook, the
chat channels and the delegate tool. A hint the table does not name is refused before
any request is sent, and the refusal names the hint and the hints that are configured —
`unknown route hint "nope"; configured hints: fast, reasoning`. A route whose provider
is not available is refused as configured but its provider is not available, so a
broken provider is not reported as an unknown hint. With no `[[model_routes]]` table at
all, a `hint:` model is refused with `no [[model_routes]] configured`. A route resolves
its own credential — its own `api_key` in the route entry, or that provider's own
environment variable — and never receives the default provider's key.

## Embedding Routing (`hint:<name>`)

You can route embedding calls with the same hint pattern using `[[embedding_routes]]`.
Set `[memory].embedding_model` to a `hint:<name>` value to activate routing.

```toml
[memory]
embedding_model = "hint:semantic"

[[embedding_routes]]
hint = "semantic"
provider = "openai"
model = "text-embedding-3-small"
dimensions = 1536

[[embedding_routes]]
hint = "archive"
provider = "custom:https://embed.example.com/v1"
model = "your-embedding-model-id"
dimensions = 1024
```

Supported embedding providers:

- `none`
- `openai`
- `custom:<url>` (OpenAI-compatible embeddings endpoint)

Optional per-route key override:

```toml
[[embedding_routes]]
hint = "semantic"
provider = "openai"
model = "text-embedding-3-small"
api_key = "sk-route-specific"
```

## Upgrading Models Safely

Use stable hints and update only route targets when providers deprecate model IDs.

Recommended workflow:

1. Keep call sites stable (`hint:reasoning`, `hint:semantic`).
2. Change only the target model under `[[model_routes]]` or `[[embedding_routes]]`.
3. Run:
   - `rantaiclaw doctor`
   - `rantaiclaw status`
4. Smoke test one representative flow (chat + memory retrieval) before rollout.

This minimizes breakage because integrations and prompts do not need to change when model IDs are upgraded.
