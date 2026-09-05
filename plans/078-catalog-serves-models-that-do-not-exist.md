# 078 — The catalog offers models the provider does not have

Written against `93c7511`. Risk tier: medium.

Every finding below was confirmed by diffing the curated tables against a live
`models_cache.json` fetched from OpenRouter (400 models, ~44 min old) on a real
profile — not by reading code.

## G1 — curated IDs are appended to live results without checking they exist

`provider_model_catalog` (`wizard.rs:1886-1890`) takes the live list and appends
every curated ID not already in it:

```rust
let mut models = cached.models;
for id in curated {
    if !models.contains(&id) { models.push(id); }
}
```

The intent is sound when curated is a subset of live. It is not. Five of the
twelve curated OpenRouter entries are absent from OpenRouter's live catalog, so
the union *manufactures* options that cannot be called:

| curated ID (ghost) | what OpenRouter actually serves |
|---|---|
| `openai/gpt-5.5-codex` | tops out at `openai/gpt-5.3-codex` |
| `google/gemini-3-pro` | `google/gemini-3.1-pro-preview`, `gemini-3-pro-image` |
| `google/gemini-3-flash` | `google/gemini-3-flash-preview` (suffix required) |
| `x-ai/grok-4.1-fast` | `x-ai/grok-4.20`, `grok-4.3`, `grok-4.5` |
| `meta-llama/llama-spark` | no such model; tops out at `llama-4-maverick`/`-scout` |

Note `meta-llama/llama-spark` and `openai/gpt-5.5-codex` are not "one version
behind" — they do not exist in any form. `llama-spark` is also the hardcoded
default for groq, ollama, fireworks, together-ai and nvidia
(`default_model_for_provider`, `wizard.rs:823-837`).

Decide the policy explicitly rather than leaving the union unqualified:

- **Preferred:** when a live list is present, it is authoritative — curated is a
  *seed* used only when the cache is empty. Simple, and it makes the freshness
  problem self-correcting.
- **If the union must stay** (e.g. so a known-good ID survives a partial API
  response), mark appended entries as unverified in the returned struct so the
  TUI/web console can render them differently, and never make one the default.

Either way, `ProviderCatalog.source` currently reports a single `"cache"` for a
list that is part live and part curated. That is inaccurate; report the split.

## G2 — providers with no curated entry offer a model named `default`

`doctor models` probes 33 providers (`providers::list_providers()`).
`curated_models_for_provider` covers ~25. The rest fall through:

```rust
_ => vec![("default".to_string(), "Default model".to_string())],
```

Observed for `synthetic`, `opencode`, `doubao`, `copilot`, `lmstudio`,
`ovhcloud`. Each shows exactly one picker entry called `default`, which is not a
model ID on any of them.

Return an empty list instead and let the caller render "no catalog — run
`models refresh`, or type the ID directly". An empty list is honest; a fake
entry is not. Check `ListPicker`'s empty-state message renders sensibly
(`model.rs:73` already supplies one).

## G3 — the ollama cloud list is hardcoded but cached as if fetched

`fetch_live_models_for_provider` (`wizard.rs:1676-1699`) returns a hand-written
10-entry list for remote ollama endpoints:

```rust
// Keep this curated list aligned with current Ollama cloud catalog.
vec!["glm-5:cloud".to_string(), "glm-4.7:cloud".to_string(), ...]
```

`run_models_refresh` then writes it through `cache_live_models_for_provider` and
prints `✅ model catalog check passed`. Observed on a profile with **no ollama
running at all** — the probe "passed" without touching a network.

`provider_model_catalog` subsequently reports `source: "cache"` with a fresh
`age_secs`, so `models list` claims live provenance for a compile-time constant.
That is a false green: a probe that cannot fail is not a check.

Either fetch the remote endpoint's `/api/tags` for real, or keep the constant
but route it through the curated path so it is labelled `"curated"` and the
doctor line reads skipped/curated rather than passed. Do not cache it as live.

## Validation

- Unit: `provider_model_catalog` with a cache lacking a curated ID must not
  silently promote that ID (assert against the chosen policy from G1).
- Unit: `curated_models_for_provider("opencode")` returns empty, and the picker
  renders its empty state rather than one row named `default`.
- Unit: a remote-ollama refresh must not produce `source: "cache"` for the
  constant list.
- Manual: `models list --provider openrouter` count must equal the live fetch
  count, with no ghost entries.

## Rollback

G1 changes user-visible list contents — the one to revert first if a regression
shows up. G2 and G3 are independent and low-blast.
