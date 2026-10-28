# RantaiClaw Providers Reference

This document maps provider IDs, aliases, and credential environment variables.

Last verified: **May 9, 2026**.

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
2. The active auth profile for `anthropic`, written by `auth paste-token` or
   `auth setup-token`. Its `auth_kind` decides the header: `authorization` goes out as
   `Authorization: Bearer`, `api-key` as `x-api-key`.
3. The env vars `ANTHROPIC_OAUTH_TOKEN`, then `ANTHROPIC_API_KEY`.
4. The generic `RANTAICLAW_API_KEY`, then `API_KEY`.

`anthropic-custom:<url>` inherits the `anthropic` env vars, so `ANTHROPIC_API_KEY`
works for a custom Anthropic-format endpoint without a second key. Config is read under
the custom name as typed only: the `anthropic` entry in `[provider_api_keys]` and the
top-level `api_key` are not read for it. The auth profile is used for inference for
`anthropic` only. A stored profile for any other provider is not used for inference;
their keys come from config or env.

With no credential in any source, the run refuses and names the string you typed and
the variable to set, for example `anthropic-custom:<url>: no credential found — set
ANTHROPIC_API_KEY, store a key in config, or paste an auth profile`.

## Provider Catalog

| Canonical ID | Aliases | Local | Provider-specific env var(s) |
|---|---|---:|---|
| `openrouter` | — | No | `OPENROUTER_API_KEY` |
| `anthropic` | — | No | `ANTHROPIC_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` |
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
| `minimax` | `minimax-intl`, `minimax-io`, `minimax-global`, `minimax-cn`, `minimaxi`, `minimax-oauth`, `minimax-oauth-cn`, `minimax-portal`, `minimax-portal-cn` | No | `MINIMAX_OAUTH_TOKEN`, `MINIMAX_API_KEY` |
| `bedrock` | `aws-bedrock` | No | `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (optional: `AWS_REGION`) |
| `qianfan` | `baidu` | No | `QIANFAN_API_KEY` |
| `doubao` | `volcengine`, `ark`, `doubao-cn` | No | `ARK_API_KEY`, `DOUBAO_API_KEY` |
| `qwen` | `dashscope`, `qwen-intl`, `dashscope-intl`, `qwen-us`, `dashscope-us`, `qwen-code`, `qwen-oauth`, `qwen_oauth` | No | `QWEN_OAUTH_TOKEN`, `DASHSCOPE_API_KEY` |
| `groq` | — | No | `GROQ_API_KEY` |
| `mistral` | — | No | `MISTRAL_API_KEY` |
| `xai` | `grok` | No | `XAI_API_KEY` |
| `deepseek` | — | No | `DEEPSEEK_API_KEY` |
| `together` | `together-ai` | No | `TOGETHER_API_KEY` |
| `fireworks` | `fireworks-ai` | No | `FIREWORKS_API_KEY` |
| `perplexity` | — | No | `PERPLEXITY_API_KEY` |
| `cohere` | — | No | `COHERE_API_KEY` |
| `copilot` | `github-copilot` | No | (use config/`API_KEY` fallback with GitHub token) |
| `lmstudio` | `lm-studio` | Yes | (optional; local by default) |
| `llamacpp` | `llama.cpp` | Yes | `LLAMACPP_API_KEY` (optional; only if server auth is enabled) |
| `nvidia` | `nvidia-nim`, `build.nvidia.com` | No | `NVIDIA_API_KEY` |

The OpenAI-compatible, Anthropic, OpenRouter, Ollama, Copilot and Bedrock providers
report the token counts that feed the daily token ceiling (`[cost] max_tokens_per_day`);
a provider that does not report them adds nothing to the day's total.

### Gemini Notes

- Provider ID: `gemini` (aliases: `google`, `google-gemini`)
- Auth can come from `GEMINI_API_KEY`, `GOOGLE_API_KEY`, or Gemini CLI OAuth cache (`~/.gemini/oauth_creds.json`)
- The Gemini CLI login is reused while its access token is valid — about an hour after the last
  `gemini` run. It is never refreshed. When the token has expired, run `gemini` again or set
  `GEMINI_API_KEY`: the expired token is refused locally, with that fix named, instead of being sent
  to Google and coming back as a 401.
- With Gemini CLI's encrypted credential storage there is no `oauth_creds.json` to reuse — the login
  lives in the OS keychain and the file is deleted after the migration — so nothing is found and a key
  is needed.
- API key requests use `generativelanguage.googleapis.com/v1beta`
- Gemini CLI OAuth requests use `cloudcode-pa.googleapis.com/v1internal` with Code Assist request envelope semantics

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

## MiniMax OAuth Setup (config.toml)

Set the MiniMax provider and OAuth placeholder in config:

```toml
default_provider = "minimax-oauth"
api_key = "minimax-oauth"
```

Then provide one of the following credentials via environment variables:

- `MINIMAX_OAUTH_TOKEN` (preferred, direct access token)
- `MINIMAX_API_KEY` (legacy/static token)
- `MINIMAX_OAUTH_REFRESH_TOKEN` (auto-refreshes access token at startup)

Optional:

- `MINIMAX_OAUTH_REGION=global` or `cn` (defaults by provider alias)
- `MINIMAX_OAUTH_CLIENT_ID` to override the default OAuth client id

Channel compatibility note:

- For MiniMax-backed channel conversations, runtime history is normalized to keep valid `user`/`assistant` turn order.
- Channel-specific delivery guidance (for example Telegram attachment markers) is merged into the leading system prompt instead of being appended as a trailing `system` turn.

## Qwen Code OAuth Setup (config.toml)

Set Qwen Code OAuth mode in config:

```toml
default_provider = "qwen-code"
api_key = "qwen-oauth"
```

Credential resolution for `qwen-code`:

1. Explicit `api_key` value (if not the placeholder `qwen-oauth`)
2. `QWEN_OAUTH_TOKEN`
3. `~/.qwen/oauth_creds.json` (reuses Qwen Code cached OAuth credentials)
4. Optional refresh via `QWEN_OAUTH_REFRESH_TOKEN` (or cached refresh token)
5. If no OAuth placeholder is used, `DASHSCOPE_API_KEY` can still be used as fallback

Optional endpoint override:

- `QWEN_OAUTH_RESOURCE_URL` (normalized to `https://.../v1` if needed)
- If unset, `resource_url` from cached OAuth credentials is used when available

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
