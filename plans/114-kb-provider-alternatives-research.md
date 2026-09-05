# Plan 114: Research alternative KB providers beyond OpenRouter — verified compatibility doc + recommendation

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/embed/mod.rs src/kb/config.rs docs/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M (research + doc; small code change optional)
- **Risk**: LOW
- **Depends on**: none — and it **resolves the batch-wide blocking question**
- **Category**: direction
- **Planned at**: commit `2ca7e59`, 2026-08-10

> **This is a research plan.** The deliverable is a verified document plus a
> recommendation, not a feature. Any code change beyond Step 4's optional
> dispatch fix belongs in a follow-up plan.

## Why this matters

This plan closes the original operator task **"Research alternative providers
besides OpenRouter"** — and it subsumes the open question blocking plans 090
and 103: whether OpenRouter's `/api/v1/embeddings` even serves the default
model. An unauthenticated probe returned `401`, identical to the control
against `/chat/completions`, so the result was inconclusive. If the default
endpoint/model is not viable, "alternative providers" stops being research and
becomes the fix.

What the audit already established (do not re-derive):

- **Embedding is provider-agnostic today.** `make_provider`
  (`embed/mod.rs:54-67`) dispatches on the base URL containing `openrouter.ai`;
  everything else goes to `TeiEmbedding`, which speaks the OpenAI-shaped
  `/embeddings` body and omits auth when no key resolves (`tei.rs:1-44`). So
  any OpenAI-compatible endpoint — OpenAI itself, vLLM, TEI, LocalAI, Azure —
  should already work via `KB_EMBEDDING_BASE_URL` + `KB_EMBEDDING_MODEL` +
  `KB_EMBEDDING_DIM`. **"Should" is untested — that is this plan's job.**
- **The chat side is the real OpenRouter pin.** Query expansion, contextual
  retrieval, intelligence extraction and the LLM reranker all POST to
  `cfg.openrouter_chat_url` (`config.rs:42,108`). The body is plain
  chat-completions, so an OpenAI-compatible chat endpoint should also work —
  same caveat.
- **Rerank already has non-OpenRouter backends** (`cohere`, `vllm`).
- **The URL-substring dispatch is fragile by its own admission**
  (`embed/mod.rs:43-48`).

## Deliverables

1. **A verified compatibility matrix** in `docs/reference/kb.md` (or a new
   `docs/reference/kb-providers.md` linked from it): for each candidate —
   OpenRouter (baseline), OpenAI, a self-hosted TEI, a self-hosted
   vLLM/Ollama-OpenAI endpoint — record: embeddings work? chat features work?
   rerank? which env settings, which models, which dims, **tested on which
   date with which result**. No untested row ships; mark untested candidates
   as such explicitly.
2. **The answer to the blocking question**, recorded in the doc and in
   `plans/README.md`: does `https://openrouter.ai/api/v1/embeddings` serve
   `qwen/qwen3-embedding-8b` with a real key? If **no**: name the working
   default (model and/or endpoint) and flag plans 090/103 for re-baselining
   before execution.
3. **A recommendation**: which provider(s) to document as first-class, and
   whether a named-provider registry (replacing the URL-substring dispatch) is
   worth building — as a proposal, not an implementation.

## What this needs from the operator

A real OpenRouter API key (and ideally an OpenAI key) for the probes. The
probes are one embeddings call and one chat call per provider — cents, not
dollars. **This plan cannot start without at least the OpenRouter key.**

## Git workflow

```bash
git switch -c docs/kb-provider-research
```

## Steps

### Step 1: Probe OpenRouter (the blocking question)

```bash
curl -s -w '\nHTTP=%{http_code}\n' -X POST https://openrouter.ai/api/v1/embeddings \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" -H 'content-type: application/json' \
  -d '{"model":"qwen/qwen3-embedding-8b","input":"probe"}'
```

Record status, and on 200 the vector length (must be 4096 to match the default
dim). Control probe: same key against `/chat/completions` with the default
intelligence model. If embeddings 404/400: enumerate what OpenRouter's model
list offers for embeddings, pick a working default, and raise the re-baselining
of 090/103 immediately — do not continue quietly.

### Step 2: Probe one hosted alternative (OpenAI) and one self-hosted (TEI or vLLM)

For each: run the real binary end-to-end, not just curl —

```bash
export KB_DB_PATH=/tmp/kb114-<provider>.db \
       KB_EMBEDDING_BASE_URL=... KB_EMBEDDING_MODEL=... KB_EMBEDDING_DIM=...
./target/release/rantaiclaw kb ingest ./README.md && \
./target/release/rantaiclaw kb search "what is this project" --top 3
```

The house rule applies: a probe needs a control. For each provider also run one
deliberately-wrong config (bad model name) and confirm the failure is legible.

### Step 3: Write the matrix + recommendation (Deliverables 1 and 3)

### Step 4 (optional, only if trivially safe): honest dispatch comment

If the research confirms the TEI path serves all OpenAI-compatible providers,
update the `make_provider` doc comment to say so in provider terms (today it
frames everything as "TEI sidecar"). Code change stays out unless the registry
proposal is accepted separately.

## Test plan

No Rust changes expected beyond Step 4's comment; run
`cargo build --features kb` if it lands. The deliverable check is the doc:
every matrix row carries a date and an observed result, and `markdownlint`
passes.

## Done criteria

- The blocking question has a recorded yes/no with evidence.
- At least two non-OpenRouter rows in the matrix are marked **verified**.
- Plans 090/103 are either unblocked or explicitly re-baselined.
- A written recommendation exists for the registry question.

## STOP conditions

- No API key available — the plan cannot produce verified rows; do not write
  an unverified matrix, that is the failure mode this plan exists to prevent.
- The default model turns out not to exist on OpenRouter: stop after recording
  it and re-baseline 090/103 with the maintainer before any further execution
  of this batch.
