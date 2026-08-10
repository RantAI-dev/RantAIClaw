# KB provider compatibility (research record)

Research record for the task "Research alternative providers besides
OpenRouter" (plan 114). Every row states **how** it was verified and on what
date, or is explicitly marked untested. Untested rows are candidates, not
recommendations.

## How dispatch works today

Embedding dispatch is URL-based (`src/kb/embed/mod.rs`): a
`KB_EMBEDDING_BASE_URL` containing `openrouter.ai` routes to the OpenRouter
provider; **everything else** routes to the OpenAI-shaped `/embeddings`
client (historically called "TEI" in the code), which sends
`{"model", "input"}` and attaches `Authorization: Bearer` only when a key
resolves. The chat-side features (query expansion, contextual retrieval,
intelligence extraction, LLM rerank) POST plain chat-completions bodies to
`KB_OPENROUTER_CHAT_URL`. Rerank additionally has dedicated `cohere` and
`vllm` transports.

Practical consequence: any OpenAI-compatible endpoint is reachable **today**
with three env vars — `KB_EMBEDDING_BASE_URL`, `KB_EMBEDDING_MODEL`,
`KB_EMBEDDING_DIM` (the model/dim pairing rules are in
[kb.md → Model rules](kb.md#model-rules-embedding-extraction-vision)).

## Compatibility matrix

| Provider | Embeddings | Chat features | Rerank | Verified |
|---|---|---|---|---|
| **OpenRouter** (default) | ✅ `qwen/qwen3-embedding-8b` @ 4096 | ✅ chat-completions (default models) | ✅ via LLM reranker | **2026-08-10, live**: `rantaiclaw kb search` through the installed binary with the operator's stored key embedded the query against `https://openrouter.ai/api/v1/embeddings` with no upstream error — this also settles the batch-blocking question (see below) |
| **OpenAI-shaped self-hosted (TEI / vLLM / LocalAI serve the same wire shape)** | ✅ | ✅ (point `KB_OPENROUTER_CHAT_URL` at the endpoint's `/chat/completions`) | ✅ `vllm` transport exists | **2026-08-10, automated**: the e2e suite drives the **real binary** against an OpenAI-shaped `/embeddings` endpoint (no auth) for ingest + search on every CI run (`tests/kb/cli_test.rs` wiremock harness); the chat side is exercised the same way (contextual-prefix e2e). Wire-shape verified; not yet run against a live TEI/vLLM deployment |
| **OpenAI (hosted)** | untested | untested | n/a | **Untested** — same wire shape as the row above, and the `text-embedding-3-*` dims are documented in kb.md, but no probe has been run (needs an OpenAI key). Do not treat as verified |
| **Ollama (OpenAI-compat mode)** | untested | untested | n/a | **Untested** — expected to ride the OpenAI-shaped path; no probe run |
| **Cohere** | n/a (rerank only) | n/a | ✅ dedicated transport | Transport exists and is unit-tested against a stubbed endpoint; not verified against live Cohere |

## The batch-blocking question — answered

**Does `https://openrouter.ai/api/v1/embeddings` serve
`qwen/qwen3-embedding-8b`? Yes.** Verified 2026-08-10 by running the
installed binary (`rantaiclaw kb search`) with the operator's stored
(encrypted) key: the query-embedding call succeeded against the default
endpoint + model. An earlier unauthenticated probe 401'd identically to the
`/chat/completions` control, which is why this was inconclusive before.
Plans 090 and 103 executed against this confirmed baseline.

## Recommendation

1. **Document OpenRouter and OpenAI-shaped self-hosted as the two
   first-class paths** (done — kb.md Model rules + this page). They cover
   hosted-default and air-gapped deployments with zero code change.
2. **A named-provider registry is not worth building now.** The URL-substring
   dispatch has exactly two arms, its known failure mode (a self-hosted
   endpoint whose hostname contains `openrouter.ai`) is implausible, and a
   registry would add a config surface with one current consumer —
   CLAUDE.md §3.2 (YAGNI) applies. Revisit if a third wire shape (a
   non-OpenAI-compatible provider) actually lands.
3. **Before promoting hosted OpenAI to "verified"**: one paid probe
   (embeddings + chat) with an OpenAI key, then flip the row. The probe
   procedure is in plan 114 step 2.
