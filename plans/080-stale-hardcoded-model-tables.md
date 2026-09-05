# 080 — The hardcoded model tables have drifted, in several directions at once

Written against `93c7511`. Risk tier: medium (no behaviour change beyond the
data itself, but `default_model_for_provider` feeds fresh installs).

One concern — static model data that no longer matches reality — spread across
five tables that drifted independently. Evidence is a live `models_cache.json`
from OpenRouter (400 models) plus one from Venice (105 models) on a real profile.

Land these as separate commits inside one PR so any single table can be reverted.

## D1 — curated lists are a major generation behind

Two independent live catalogs agree that the Claude 5 family is shipping:
OpenRouter serves `anthropic/claude-fable-5`; Venice serves `claude-opus-5`,
`claude-sonnet-5`, `claude-fable-5`, `claude-opus-4-8-fast`.
`curated_models_for_provider` tops out at Sonnet 4.6 / Opus 4.7.

The OpenRouter block also lags the *direct-provider* blocks in the same function
by one generation across four families — an internal contradiction, provable
without any network:

| family | `"openrouter"` block | direct block | live catalog has |
|---|---|---|---|
| Anthropic Opus | `claude-opus-4.7` | `claude-opus-4-8` | `claude-opus-4.8`, `-4.8-fast` |
| DeepSeek | `deepseek-v3.2` | `deepseek-v4-pro` | `deepseek-v4-pro`, `-v4-flash` |
| GLM | `glm-5.1` | `glm-5.2` | `glm-5.2`, `glm-5-turbo` |
| Kimi | `kimi-k2.6` | `kimi-k2.7-code` | (k2.7 absent from OpenRouter — k2.6 correct here) |

`astrai` and `venice` carry the same lag.

Refresh all of them. While doing so, note that `x-ai/grok-4.1-fast` is not a
version behind — the live x-ai namespace is `grok-4.20` / `4.3` / `4.5` with no
`4.1-fast` at all.

This table will drift again. Plan 078 G1 (live-is-authoritative) is what stops
that from mattering; this commit only buys time.

## D2 — three providers default to a model absent from their own list

`default_model_for_provider` (`wizard.rs:817`) vs `curated_models_for_provider`
(`:846`):

| provider | default written by setup | in that provider's curated list? |
|---|---|---|
| `ollama` (`:836`) | `llama-spark` | no — list is `llama3.2`, `mistral`, `codellama`, `phi3`; also not an Ollama tag |
| `llamacpp` (`:837`) | `ggml-org/llama-spark-GGUF` | no — list is `gpt-oss-20b-GGUF`, `Llama-3.3-70B-Instruct-GGUF`, `Qwen2.5-Coder-7B-Instruct-GGUF` |
| `bedrock` (`:840`) | `anthropic.claude-sonnet-4-6-v1:0` | no — list has `anthropic.claude-sonnet-4-6`, no suffix |

Quick-setup therefore writes a `default_model` the picker never offers. The one
covering test, `provider_model_catalog_falls_back_to_curated_without_cache`
(`:6038`), only exercises `openai`, so the whole class is untested.

Add a test that asserts, for **every** provider, that
`default_model_for_provider(p)` appears in `curated_models_for_provider(p)`.
That single assertion prevents this class from recurring and is the highest-value
part of this plan.

## D3 — Bedrock IDs use three different formats in one four-item list

```
anthropic.claude-sonnet-4-6                 ← bare
anthropic.claude-opus-4-6-v1                ← -v1, no :0
anthropic.claude-haiku-4-5-20251001-v1:0    ← full
anthropic.claude-sonnet-4-5-20250929-v1:0   ← full
```

Bedrock model IDs require the `-v1:0` form; the first two will 400 at call time.
Normalize all four. Bedrock is not in `supports_live_model_fetch`
(`wizard.rs:1345`), so curated is its entire catalog forever — there is no live
list to correct this later.

## D4 — ollama / llamacpp lists are from 2024

`llama3.2`, `mistral`, `codellama`, `phi3`, `Qwen2.5-Coder-7B-Instruct-GGUF`,
`bartowski/Llama-3.3-70B-Instruct-GGUF`. Everything else in the file is 2026-era.
CodeLlama has been superseded upstream.

## D5 — `MINIMAX_ONBOARD_MODELS` is dead and contradicts the live table

`wizard.rs:809`. One occurrence repo-wide: its own declaration. It claims
`MiniMax-M2.5` is "latest, recommended" while `curated_models_for_provider`
says M3 and `default_model_for_provider` returns `MiniMax-M3`.

How it got there is visible in the source: it sits **between** the doc comment
`/// Pick a sensible default model for the given provider.` (`:808`) and the
function that comment describes (`default_model_for_provider`, `:817`) — pasted
into the doc-comment/function seam.

Delete it. Note that lints do not catch this: `cargo check --lib` emits no
warning for it, and a deliberately-added unused const drew no warning either
(verified with that control). So do not assume CI would have caught similar dead
tables elsewhere.

## D6 — cost pricing table has zero overlap with shipped defaults

`get_default_pricing()`, `src/config/schema.rs:768-830`:
`anthropic/claude-sonnet-4-20250514`, `anthropic/claude-opus-4-20250514`,
`anthropic/claude-3.5-sonnet`, `anthropic/claude-3-haiku`, `openai/gpt-4o`,
`openai/gpt-4o-mini`, `openai/o1-preview`, `google/gemini-2.0-flash`,
`google/gemini-1.5-pro`.

Not one is a current default. `CostConfig::default()` has `enabled: false`, so
nothing is broken today — but the moment an operator enables cost tracking,
every model they actually use is unpriced. Refresh, and add a test that each
`default_model_for_provider` value resolves to a price entry (or that the
miss path is explicitly handled and reported, if unpriced is meant to be legal).

## D7 — wizard tier labels contradict the lists one file over

`src/onboard/provision/provider.rs:101-106`: `"OpenAI — GPT-4o, o1, o3"`,
`"xAI — Grok 3"`, `"Google Gemini — Gemini 2.0 Flash & Pro"`, `"DeepSeek — V3 & R1"`,
`"Cohere — Command R+"`. The curated lists in the adjacent file say gpt-5.5,
grok-4.1, gemini-3-pro, deepseek-v4-pro, command-a. First thing a new operator
reads during setup.

## D8 — `VISION_MODEL` pinned to `gpt-4o-mini`

`src/kb/file/image.rs:35`, with a comment pinning it to a line number in a file
from another project (`file-processor.ts:11`). Re-point at a current vision
model and drop the stale cross-repo reference.

## Validation

- The two new invariant tests (D2, D6) are the deliverable that outlasts the data
  refresh — write them first, watch them fail, then fix the tables.
- `cargo test` for the wizard module; no network in tests.
- Manual: `rantaiclaw setup provider` for ollama, llamacpp and bedrock; the
  offered default must appear in the offered list.

## Rollback

Per-commit revert. D2/D3 affect fresh installs; D1/D4/D7 are data-only; D5 is a
deletion; D6/D8 touch other subsystems and can be split into their own PR if
review prefers.
