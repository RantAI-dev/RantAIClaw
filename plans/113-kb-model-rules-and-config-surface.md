# Plan 113: Define the KB model rules: one documented model/dim registry and a config surface

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/config.rs src/kb/file/image.rs docs/reference/kb.md`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: 090 (recipe tag), 098 (dim guard) — both change what the rules must say
- **Category**: direction
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

This plan closes the original operator task **"Define OCR & embedding model
rules"**. Plans 090/098/110 fix individual defects; none of them answers the
operator's actual question: *which models can I use, what dimension goes with
each, and how do I change one safely?*

Today there are no rules — there are scattered facts:

- **Every model knob is env-only.** `KbConfig::from_env` (`config.rs:66-123`)
  reads 8+ model-related vars (`KB_EMBEDDING_MODEL`, `KB_EMBEDDING_DIM`,
  `KB_EXTRACT_PRIMARY`, `KB_EXTRACT_FALLBACK`, `KB_EXTRACT_SMART_FALLBACK`,
  `KB_RERANK_MODEL`, `KB_INTELLIGENCE_MODEL`,
  `KB_CONTEXTUAL_RETRIEVAL_MODEL`, `KB_QUERY_EXPANSION_MODEL`). None appears
  in `config.toml`; `KnowledgeConfig` carries only credentials (+ `enabled`
  after plan 102).
- **One model is not configurable at all.** Image ingestion is pinned to
  `pub const VISION_MODEL: &str = "openai/gpt-5-mini"` (`file/image.rs:31-37`),
  whose own comment says a knob "should land in `KbConfig`".
- **The model/dim pairing is a trap with no table.** The default is
  `qwen/qwen3-embedding-8b` @ 4096. Change the model without the dim and
  plan 098's guard now *refuses* — but nothing tells the operator which dim
  the new model needs.
- **Extractor sentinels fail silently on typos.** `build_extractor`
  (`extract/mod.rs:75-121`) treats any unknown sentinel as an OpenRouter model
  id, so `KB_EXTRACT_PRIMARY=unpfd` becomes a runtime API error, not a config
  error. Same catch-all in `make_reranker` (`rerank/mod.rs:90`).

## Deliverables

1. **A model registry section in `docs/reference/kb.md`** — the rules, written
   down: a table of known-good embedding models with their dimensions
   (`qwen/qwen3-embedding-8b` → 4096 at minimum, plus the OpenAI
   `text-embedding-3-*` dims for TEI/OpenAI-shaped endpoints), the safe
   model-change procedure (set model+dim → new DB or `kb re-embed
   --include-current` → `kb drift` shows in-sync), the OCR/vision model rules
   (PDF: sentinel or vision model id; image: `kb.vision_model`; local OCR:
   `kb-ocr` feature), and every model env var in one table.
2. **`vision_model` promoted to `KbConfig`** — new field + `KB_VISION_MODEL`
   env, default `openai/gpt-5-mini`; `file/image.rs` reads it instead of the
   const. The const's comment already prescribes this.
3. **Sentinel validation** — `build_extractor` rejects an unknown sentinel
   that does not look like a model id (`contains('/')` is the discriminator
   OpenRouter ids all satisfy; `graph_exposes_capability` at
   `api_test.rs:684-706` already leans on that shape). A bare-word typo like
   `unpfd` becomes `KbError::Config` naming the valid sentinels.

## Scope

**In scope**: the three deliverables. All are additive; no default changes.

**Out of scope**: moving model config into `config.toml`/schema — that is a
schema-version event and belongs with a future config-surface effort; the
docs table and env vars are the contract for now. Also out: auto-detecting
dims from a provider API (nice, speculative — CLAUDE.md §3.2).

## Git workflow

```bash
git switch -c feat/kb-model-rules
```

## Steps

### Step 1: Promote `vision_model` to `KbConfig`

Add the field, read `KB_VISION_MODEL` in `from_env` with the current const as
default, replace the use in `file/image.rs:108`, delete the const (its doc
comment moves to the config field).

**Verify**: `tests/kb/file_test.rs:231 process_image_makes_openrouter_vision_call`
still passes (it stubs the endpoint, not the model); add an assertion that an
env override reaches the request body.

### Step 2: Validate extractor sentinels

In `build_extractor`'s catch-all arm: if the sentinel does not contain `/`,
return `KbError::Config` listing `unpdf | mineru | hybrid | smart | <provider/model>`.
Mirror the same check in `make_reranker`'s catch-all only if a bare word would
otherwise construct an `LlmReranker` with a nonsense model — read plan 108's
Step 3 first; land after it to avoid conflicts.

**Verify**: new test — `KB_EXTRACT_PRIMARY=unpfd` errors at build time with the
sentinel list; control: a real model id still routes to `VisionLlmExtractor`.

### Step 3: Write the registry docs

The section in `docs/reference/kb.md` described in Deliverable 1. Cross-link
`kb drift` / `kb re-embed`, plan 098's mismatch error, and plan 090's `+meta1`
recipe suffix so the three read as one procedure.

**Verify**: `markdownlint docs/reference/kb.md`; every env var named in
`config.rs:66-123` appears exactly once in the table (script the check, do not
eyeball 20 rows).

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb file_test
cargo test --features kb --test kb extract_test
```

## Done criteria

- An operator can answer "which model, which dim, how do I switch" from one
  docs section.
- A typo'd sentinel is a config error naming the options.
- No hardcoded model constant remains in the ingest path.

## STOP conditions

- Plan 108 unlanded and Step 2 touches `make_reranker` — defer that half.
- The `contains('/')` discriminator misclassifies a real sentinel someone
  added since `2ca7e59` — re-derive the discriminator, do not special-case.
