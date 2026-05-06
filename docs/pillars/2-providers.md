# Pillar 2 — Provider and Model Runtime

> **ClickUp:** [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) → Feature: Per-Provider SSE Streaming + Setup: Provider · **Maturity:** Mostly stable, streaming partial · **Modules:** `src/providers/`

Talk to any model from one binary. Native adapters for the leaders, OpenRouter as a fallback to 200+ models, and a single resilient wrapper that handles retries, timeouts, and quota errors uniformly.

## What this pillar covers

- Direct provider adapters (OpenAI, Anthropic, Gemini, Bedrock, GLM/Z.AI, Moonshot/Kimi, MiniMax, Qwen, Meta/Llama family, DeepSeek, Mistral, Groq, xAI, OpenRouter, Venice, Astrai)
- Resilient request wrapper (retry, fallback, timeout)
- Per-provider streaming (SSE) — OpenRouter shipped in v0.4.0; Anthropic / OpenAI / Gemini / Groq / Mistral / xAI landing in v0.6.0
- Curated default-model registry (refreshed per release)
- Tool-call schema across providers

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Native providers | ~15 | TBD | TBD |
| Aggregator passthrough | OpenRouter / Venice / Astrai | TBD | TBD |
| Streaming model coverage | OpenRouter shipped; Anthropic / OpenAI / Gemini / Groq / Mistral / xAI in v0.6 | TBD | TBD |
| Tool-call schema unified | ✅ | TBD | TBD |
| Resilient wrapper (retry/fallback) | ✅ `src/providers/reliable.rs` | TBD | TBD |
| Default-model freshness cadence | Per-release refresh (v0.5.3 = 2026-04-30) | TBD | TBD |

## Current state by maturity

| Surface | Maturity |
|---|---|
| OpenAI adapter | Stable |
| Anthropic adapter | Stable |
| Gemini adapter | Stable |
| Bedrock adapter | Stable |
| OpenRouter aggregator | Stable; streaming shipped v0.4.0 |
| GLM / Z.AI | Stable |
| Moonshot / Kimi | Stable |
| MiniMax | Stable |
| Qwen | Stable |
| Llama family (groq / fireworks / together / nvidia / llamacpp) | Stable |
| DeepSeek | Stable; needs model-id refresh |
| Per-provider SSE streaming | OpenRouter only — Anthropic / OpenAI / Gemini / Groq / Mistral / xAI in v0.6.0 |

## Architecture

```
src/providers/
├── traits.rs        ← Provider trait
├── mod.rs           ← factory + alias resolution
├── reliable.rs      ← retry/fallback wrapper
├── compatible.rs    ← OpenAI-shape compatibility layer
├── openai.rs / anthropic.rs / gemini.rs / bedrock.rs / ...
└── tests
```

Resolution: `provider:model` string → factory key → `Provider` impl → resilient wrapper → request.

## Trait extension point

- `Provider` — `src/providers/traits.rs`
- Register in `src/providers/mod.rs` factory
- Tests: factory wiring + error path
- Avoid leaking provider-specific behavior into orchestration

## CLI / config

```bash
rantaiclaw model                       # show current
rantaiclaw model set openai gpt-5.5    # switch
rantaiclaw model list                  # list known
```

```toml
[provider]
name = "anthropic"
model = "claude-sonnet-4-6"
# api_key from env: ANTHROPIC_API_KEY
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Feature: Per-Provider SSE Streaming completes the v0.4.0 promise (Anthropic / OpenAI / Gemini / Groq / Mistral / xAI). Setup: Provider validates the picker against every supported provider.
