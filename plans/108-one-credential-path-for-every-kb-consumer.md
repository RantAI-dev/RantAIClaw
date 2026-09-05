# Plan 108: One credential path for every KB consumer

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/retrieve/ src/kb/rerank/ src/kb/config.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: 091, 092 (their *decision*, not their code — see the note below)
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

A key entered in the web console or the TUI reaches some KB consumers and not
others. There is no signal about which.

Config-set keys flow through `KbConfig::from_env_with_keys`
(`src/kb/config.rs:130-139`) into `cfg.embedding_api_key` /
`cfg.extract_vision_api_key`. Consumers that read from `cfg` get them.
Consumers that call `std::env::var` directly do not.

| Consumer | Source | Console key reaches it |
|---|---|---|
| `embed/openrouter.rs:54` | `resolve_key(cfg.embedding_api_key)` | yes |
| `embed/tei.rs:36` | same | yes |
| `extract/vision_llm.rs:159` | `resolve_key_with_fallback` | yes |
| `axi/api.rs:737` intelligence | `resolve_key(cfg.embedding_api_key)` | yes |
| `retrieve/query_expansion.rs:49` | `env::var("OPENROUTER_API_KEY")` | **no** |
| `retrieve/contextual.rs:45` | `env::var("OPENROUTER_API_KEY")` | **no** |
| `retrieve/standalone_query.rs:76` | `env::var("OPENROUTER_API_KEY")` | **no** |
| `rerank/llm.rs:59` | `env::var("OPENROUTER_API_KEY")` | **no** |
| `rerank/cohere.rs:54` | `env::var("KB_RERANK_API_KEY")` | **no** |

The failure modes differ, and both are quiet:

- `query_expansion` returns the original query when the key is empty
  (`query_expansion.rs:49-52`) — the feature silently does nothing.
- `LlmReranker` returns a hard `KbError::Config` (`rerank/llm.rs:59-62`), and it
  is the **default** reranker for any provider string that is not `vllm` or
  `cohere` (`rerank/mod.rs:90-94`), including the empty default. `apply_rerank`
  catches it and falls back to the fused order with a `warn`
  (`retrieve/mod.rs:463-471`), so rerank is off and nothing says so.

An operator who configures the KB entirely through the console gets embeddings,
OCR and intelligence working, while query expansion and reranking are dead.

## Note on the surviving dead paths

Plans 091 and 092 may delete `contextual.rs` and `standalone_query.rs`. Land
those first if they are going to be deleted — do not fix credentials in a file
that is about to be removed. Check their status before starting.

## Current state (verified at 2ca7e59)

- `KbConfig::resolve_key(override)` — `config.rs:143-148`: returns the override,
  else `OPENROUTER_API_KEY`
- `resolve_key_with_fallback(primary, secondary)` — `config.rs:154-159`
- `KbConfig` has no dedicated chat/rerank credential field

## Scope

**In scope**: route every consumer through `KbConfig`, and make an
unusable-credential state observable.

**Out of scope**: adding a config surface for these keys (plan 102 owns config
shape) — env remains the input, but it must be read **once**, into `KbConfig`,
at construction.

## Git workflow

```bash
git switch -c fix/kb-credential-unification
```

## Steps

### Step 1: Give `KbConfig` the missing credentials

```rust
    /// Credential for the chat endpoint shared by query expansion, contextual
    /// retrieval and the LLM reranker. Reads `OPENROUTER_API_KEY`; falls back
    /// to the embedding key so a single console-entered credential works
    /// everywhere. Resolved ONCE here — consumers must not call `env::var`.
    pub chat_api_key: String,
    /// Credential for a managed rerank provider. Reads `KB_RERANK_API_KEY`
    /// then `COHERE_API_KEY`.
    pub rerank_api_key: String,
```

Populate in `from_env`. In `from_env_with_keys`, when `chat_api_key` is empty
and an embedding key was supplied, fall back to it — that is what makes a single
console key work across the board.

**Verify**: extend `config_test.rs:106
resolve_key_with_fallback_prefers_override_then_secondary_then_env` with the new
fields.

### Step 2: Convert the consumers

Replace each `env::var` with the config field:

- `query_expansion.rs:49` → `cfg.chat_api_key`
- `contextual.rs:45` → `cfg.chat_api_key` (skip if 091 deletes it)
- `standalone_query.rs:76` → `cfg.chat_api_key` (skip if 092 deletes it)
- `rerank/llm.rs:59` → the reranker needs the value at construction;
  `make_reranker` already has `cfg` (`rerank/mod.rs:73`), so pass it into
  `LlmReranker::new` rather than reading env inside `rerank`
- `rerank/cohere.rs:54` → `cfg.rerank_api_key`, falling back to
  `cfg.chat_api_key`

**Verify**: `grep -rn 'env::var("OPENROUTER_API_KEY")' src/kb/` returns only
`config.rs:147`.

### Step 3: Make a dead reranker visible

`make_reranker` currently hands back an `LlmReranker` that will fail on every
call when no credential resolves. Decide at construction instead:

```rust
        _ => {
            if cfg.chat_api_key.is_empty() {
                tracing::warn!(
                    target: "kb::rerank",
                    "rerank is enabled but no chat credential resolves; skipping the rerank stage",
                );
                return None;
            }
            Some(Box::new(LlmReranker::new(...)))
        }
```

`None` is the honest representation of "no rerank stage", and it matches how
the vLLM branch already handles an init failure (`rerank/mod.rs:78-88`).

### Step 4: Tests

- `config_test.rs`: `chat_api_key` falls back to the embedding key when
  `OPENROUTER_API_KEY` is unset.
- `rerank_test.rs`: `make_reranker` returns `None` when rerank is enabled and no
  credential resolves. Note `rerank_test.rs:158
  llm_rerank_returns_error_on_missing_api_key` asserts the *old* behaviour —
  update it to reflect construction-time refusal, and say why in the test name.
- One test proving a config-supplied key reaches query expansion (stub the chat
  endpoint, assert it is called).

That last test is the regression guard for the whole plan.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb config_test
cargo test --features kb --test kb rerank_test
cargo test --features kb --test kb retrieve_test
```

Manual, with `OPENROUTER_API_KEY` **unset** and a key set only in
`config.toml`:

```bash
KB_QUERY_EXPANSION_ENABLED=true KB_RERANK_ENABLED=true \
  cargo run --features kb -- kb search "test query" --top 3
# expect no "no chat credential resolves" warning
```

## Done criteria

- No KB consumer reads a credential from env directly.
- A single console-entered key drives embedding, OCR, intelligence, expansion
  and rerank.
- An unusable rerank config produces one clear warning, not a per-query error.

## STOP conditions

- 091/092 are still undecided — fixing credentials in a module slated for
  deletion wastes the work. Resolve their status first.
