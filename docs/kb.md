# Knowledge Base

Document storage and retrieval for rantaiclaw. The KB is for organization-level documents (PDFs, markdown, office files, images, code) and is strictly separate from `memory/` (the agent's short-term/long-term conversation memory). The two stores have different lifecycles, different schemas, and different access paths — do not confuse them.

Last verified: **May 15, 2026** (Phase 14 — feature-gated, off by default).

## Architecture

- **Storage**: SQLite with the `sqlite-vec` virtual table for embeddings, plus FTS5 for BM25 lexical search. One `kb.db` per deployment (per workspace).
- **Embedding**: OpenRouter by default (`qwen/qwen3-embedding-8b` at 4096 dimensions). A Text Embeddings Inference (TEI) sidecar is supported via `KB_EMBEDDING_BASE_URL`.
- **Retrieval**: hybrid (vector + BM25 via Reciprocal Rank Fusion) with optional reranker (LLM via OpenRouter, Cohere, or vLLM sidecar). Optional query expansion and Anthropic-style contextual retrieval.
- **Extraction**: smart-router PDF pipeline. Defaults to unpdf; falls through to a vision LLM (or MinerU sidecar) when text-layer signals indicate the PDF needs OCR. Image OCR uses an OpenRouter vision LLM.
- **Surfaces**: three ways to consume the KB
  - In-process Rust API (used by rantaiclaw's own agent loop)
  - CLI: `rantaiclaw kb …` with TOON output (axi-cli style)
  - HTTP: `/api/v1/kb/*` on the gateway, JSON

## Quickstart

### Enable the feature

The KB is gated behind the `kb` Cargo feature. It is **off by default** to keep the base binary small.

```bash
cargo build --features kb
# or with office formats (.docx, .xlsx):
cargo build --features kb,kb-office
```

### Set the embedding API key

When using the default OpenRouter embedding endpoint:

```bash
export OPENROUTER_API_KEY=sk-or-...
```

When pointing at a TEI sidecar instead, `KB_EMBEDDING_API_KEY` (or the OpenRouter key as fallback) is sent as a bearer token. If neither is set the auth header is skipped — appropriate for an unauthenticated local sidecar.

### Ingest a document

```bash
rantaiclaw kb ingest /path/to/policy.pdf --category INSURANCE --group billing
```

Supported file types (subject to feature flags):
- PDF (always — text-layer first via `lopdf`, falls back to vision LLM or MinerU when configured)
- Markdown, plain text, source code (`.md`, `.txt`, `.rs`, `.py`, `.ts`, …)
- Images (`.png`, `.jpg`, `.jpeg`, `.webp`) — OCR via OpenRouter vision LLM
- Office (`.docx`, `.xlsx`) — requires the `kb-office` feature

### Search

```bash
rantaiclaw kb search "what is the coverage limit?" --top 5
```

TOON output (default):

```
chunks[5]{document,section,score,content_preview}:
  Insurance Policy,Coverage,0.91,The maximum coverage limit is...
  Insurance Policy,Coverage,0.84,Deductibles apply per claim...
  ...
```

JSON output for scripted callers:

```bash
rantaiclaw kb search "..." --json
```

### List, get, delete

```bash
rantaiclaw kb list
rantaiclaw kb get <document_id>
rantaiclaw kb delete <document_id>           # soft-delete (sets deleted_at, hides from search)
rantaiclaw kb delete <document_id> --hard    # permanent removal
```

### Maintenance

```bash
rantaiclaw kb drift                          # report chunks embedded with a non-current model
rantaiclaw kb re-embed --dry-run             # preview a re-embed run
rantaiclaw kb re-embed --include-current     # force re-embed every chunk
```

## Configuration

All KB settings are environment-driven. The full list of `KB_*` variables and their defaults is in [config-reference.md](config-reference.md#kb-knowledge-base). The most commonly tuned knobs:

| Env var | Default | Purpose |
|---|---|---|
| `KB_DB_PATH` | platform data dir (`~/.local/share/rantaiclaw/kb.db` on Linux) | Path to the SQLite file |
| `KB_EMBEDDING_MODEL` | `qwen/qwen3-embedding-8b` | Embedding model ID |
| `KB_EMBEDDING_DIM` | `4096` | Vector dimension; must match the model |
| `KB_HYBRID_BM25_ENABLED` | `true` | Hybrid vector + BM25 retrieval (set `false` to disable BM25) |
| `KB_RERANK_ENABLED` | `false` | Opt-in LLM/Cohere/vLLM reranker |
| `KB_EXTRACT_PRIMARY` | `smart` | PDF extraction: `smart`, `unpdf`, `vision`, `mineru` |

Storage path resolution:

1. `KB_DB_PATH` (when non-empty)
2. Platform data dir via `directories::ProjectDirs` (`~/.local/share/rantaiclaw/kb.db` on Linux, `~/Library/Application Support/rantaiclaw/kb.db` on macOS)
3. `./kb.db` in the current working directory (final fallback for containers without HOME)

## Sidecars (optional)

For deployments without OpenRouter access (air-gapped, regulated, on-prem):

### Text Embeddings Inference (TEI) for embeddings

```bash
export KB_EMBEDDING_BASE_URL=http://localhost:8080/embeddings
# optionally:
export KB_EMBEDDING_API_KEY=...   # sent as bearer; falls back to OPENROUTER_API_KEY if unset
```

### MinerU for PDF extraction

```bash
export KB_EXTRACT_MINERU_BASE_URL=http://localhost:8100
export KB_EXTRACT_PRIMARY=mineru
```

### vLLM reranker

```bash
export KB_RERANK_ENABLED=true
export KB_RERANK_PROVIDER=vllm
export KB_RERANK_MODEL=BAAI/bge-reranker-v2-m3
# vLLM endpoint is read from KB_OPENROUTER_CHAT_URL or provider-specific overrides
```

See [config-reference.md](config-reference.md#kb-knowledge-base) for the complete reranker provider matrix.

## CLI reference

The CLI is the "axi-cli" surface: idempotent, never interactive, defaults to TOON output, `--json` toggles JSON. Each subcommand is gated by `--features kb`.

| Subcommand | Purpose |
|---|---|
| `kb search <query>` | Hybrid retrieval. `--top`, `--group`, `--category`, `--json` |
| `kb ingest <path>` | Extract + chunk + embed + store a file. `--title`, `--category`, `--group`, `--json` |
| `kb list` | List documents. `--organization`, `--json` |
| `kb get <id>` | Show one document by ID. `--json` |
| `kb delete <id>` | Soft-delete a document (sets `deleted_at`). `--hard` for permanent |
| `kb drift` | Report chunks embedded with a stale model. `--json` |
| `kb re-embed` | Re-embed chunks. `--include-current`, `--dry-run`, `--batch-size`, `--json` |

Exit codes:

- `0` — success
- `1` — operational failure (document not found, empty extraction). A TOON `error[1]{code,message}:` block is printed to stdout.
- non-zero (other) — internal failure (DB unreachable, bad config). Rendered as a TOON error block.

Full flag list: [commands-reference.md](commands-reference.md#kb-knowledge-base).

## HTTP API

When the gateway runs with `--features kb`, the following routes are mounted under `/api/v1/kb/*`:

```
POST   /api/v1/kb/search             # JSON body: { "query", "top", "group_ids?", "category?" }
POST   /api/v1/kb/documents          # multipart file upload + metadata
GET    /api/v1/kb/documents          # list documents
GET    /api/v1/kb/documents/{id}     # get one document
DELETE /api/v1/kb/documents/{id}     # ?hard=true for permanent delete; default soft
GET    /api/v1/kb/drift              # staleness report
POST   /api/v1/kb/re-embed           # JSON body: { "include_current?", "dry_run?", "batch_size?" }
```

Authentication mirrors the rest of `/api/v1/*`: pairing/bearer-token rules from `[gateway]` apply unchanged. When `require_pairing = false`, requests pass through.

Upload size cap: **32 MiB per ingest request**. Larger files are rejected before any handler runs.

Init behavior: the KB context (config, store, embedder, optional reranker) is built lazily on first request and cached process-wide via `OnceCell`. Init failures cache as `Err` and surface as **503** on every subsequent call until the gateway restarts — this is intentional fail-fast behavior.

## Agent integration

When `--features kb` is enabled **and** a `kb.db` exists at the resolved path, rantaiclaw's agent loop auto-injects an axi-ambient context block into the system prompt that informs the model it can shell out via `rantaiclaw kb search "<query>"` to consult the KB.

No MCP server, tool registration, or schema declaration is required — the agent uses its existing `shell` capability with the standard policy + autonomy gates. If the autonomy preset doesn't permit `rantaiclaw` in the shell allowlist, the agent simply can't reach the KB. Operators can either add `rantaiclaw` to `[autonomy].allowed_commands` or implement a dedicated tool.

## Limitations

- **Office formats**: `.docx` and `.xlsx` are supported via the `kb-office` feature. Other formats (`.pptx`, `.rtf`, `.epub`, `.doc`, `.ppt`, `.odt`) are not implemented.
- **Image OCR via Ollama** (`use_ocr_pipeline: true` in the TS predecessor) is not ported — current builds use OpenRouter vision LLMs only.
- **LanceDB / HNSW backend**: not shipped. The current `sqlite-vec` backend performs a linear scan over the vector table; this is fast enough for ≤100k chunks. There is no in-tree `kb-lancedb` stub feature — adding one before a real implementation would itself be a YAGNI violation, and `lancedb` would pull Arrow + DataFusion (~50 MB compiled), conflicting with the binary-size goal.
- **Artifact indexer**: deferred. The TS-specific sandbox + pandoc artifact-indexing path is out of scope for the Rust port (non-goal documented in the integrating PR).

## Performance and tuning

- Latency benchmark methodology and current numbers: [kb-bench.md](kb-bench.md).
- Retrieval-quality knobs (rerank, expansion, contextual retrieval, smart chunker sizes): [kb-tuning.md](kb-tuning.md).

## See also

- [commands-reference.md](commands-reference.md#kb-knowledge-base) — CLI subcommand reference
- [config-reference.md](config-reference.md#kb-knowledge-base) — full `KB_*` env var list
- [kb-bench.md](kb-bench.md) — latency benchmarks
- [kb-tuning.md](kb-tuning.md) — retrieval quality tuning notes
