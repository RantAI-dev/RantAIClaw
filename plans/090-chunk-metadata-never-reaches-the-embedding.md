# Plan 090: Embed chunks with their metadata prefix, and make the change detectable

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/chunk/ src/kb/store/ src/kb/maintenance/ src/kb/axi/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P1
- **Effort**: M–L
- **Risk**: HIGH
- **Depends on**: none (but see "Release impact" — this forces a full re-embed)
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`prepare_chunk_for_embedding` (`src/kb/chunk/prepare.rs:23`) exists to prepend
semantic context to a chunk before it is embedded:

```text
Category: {category}
Topic: {subcategory}
Section: {section}
Context: {contextual_prefix}

{chunk content}
```

**It has zero production callers.** All three embedding paths embed the raw
chunk body:

| Path | Line | What is embedded |
|---|---|---|
| HTTP ingest | `src/kb/axi/api.rs:1336` | `c.content.clone()` |
| CLI ingest | `src/kb/axi/cli.rs:317` | `c.content.clone()` |
| Bulk re-embed | `src/kb/maintenance/bulk_re_embed.rs:113-116` | `content` column, raw |

The only caller is `tests/kb/parity_helpers.rs:165`, which the parity gate
uses. So the harness that exists to prove the Rust port matches the TypeScript
reference **embeds differently from the shipped binary** — which is why nothing
ever flagged this.

Every vector in every existing corpus is missing the category, topic and
section context the design intends. Retrieval quality is systematically below
what the pipeline was built for, and no test or metric would show it.

## Current state (verified at 2ca7e59)

`src/kb/axi/api.rs:1336-1337`:

```rust
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let embeddings = ctx.embedder.embed_many(&texts).await.map_err(|e| { ... })?;
```

`src/kb/maintenance/bulk_re_embed.rs:113-116`:

```rust
        let (ids, texts): (Vec<_>, Vec<_>) = page
            .into_iter()
            .map(|(id, content, _)| (id, content))
            .unzip();
```

`list_chunks_for_re_embed` returns `(ChunkId, String, Option<String>)` —
`src/kb/store/mod.rs:121-126`. **It does not return chunk metadata**, so the
re-embed path cannot rebuild the prefix today. That is the main structural
obstacle.

`contextual_prefix` is always `None` in production — every producer writes
`None` (`chunk/smart.rs:518`, `chunk/recursive.rs:176`) because
`generate_contextual_prefixes` is itself dead (plan 091). Treat the `Context:`
line as inert for now; it will start working if 091 wires it.

## The detection problem — read before writing code

`kb drift` compares the `embedding_model` column against `cfg.embedding_model`
(`maintenance/drift.rs:37-41`). This change alters the **input text**, not the
model name. So after the fix:

- freshly ingested chunks carry metadata-prefixed vectors,
- pre-existing chunks carry raw-content vectors,
- both are tagged with the same model string,
- `kb drift` reports `in_sync: true`,
- and the two live in different regions of the embedding space.

A silently mixed corpus is worse than the original bug. The fix therefore MUST
ship with a recipe marker so drift can see it.

## Scope

**In scope**: wire the prefix into all three paths; widen
`list_chunks_for_re_embed` to carry the metadata needed to rebuild it; add a
recipe marker to the stored model tag so drift detects stale vectors.

**Out of scope**: contextual retrieval (plan 091), embedding dimension
migration (plan 098).

## Git workflow

```bash
git switch -c fix/embed-chunk-metadata
```

## Steps

### Step 1: Introduce the recipe marker

Add to `src/kb/chunk/prepare.rs`:

```rust
/// Version tag for the text sent to the embedding provider. Bump whenever the
/// composed embedding input changes shape — the stored `embedding_model` tag
/// carries it so `kb drift` can tell a stale corpus from a current one. The
/// model name alone cannot: the model does not change, the input does.
pub const EMBEDDING_RECIPE: &str = "meta1";

/// The value written to `chunk.embedding_model`, e.g.
/// `qwen/qwen3-embedding-8b+meta1`.
pub fn tagged_model(model: &str) -> String {
    format!("{model}+{EMBEDDING_RECIPE}")
}
```

**Verify**: unit test that `tagged_model("m")` returns `"m+meta1"`.

### Step 2: Make drift compare against the tagged model

In `maintenance/drift.rs`, compare stored tags against
`tagged_model(&cfg.embedding_model)` instead of the bare model. Pre-existing
rows tagged with the bare name become "stale", which is correct — they are.

**Verify**: `cargo test --features kb --test kb maintenance_test` — the four
drift tests will need their expected model strings updated. Update them; do not
weaken the assertions.

### Step 3: Wire the prefix into both ingest paths

In `api.rs` and `cli.rs`, replace the `c.content.clone()` map with
`prepare_chunk_for_embedding`, and store the tagged model:

```rust
    let texts: Vec<String> = chunks.iter().map(prepare_chunk_for_embedding).collect();
    let embeddings = ctx.embedder.embed_many(&texts).await...;
    // stored tag carries the recipe so drift can spot a pre-metadata corpus
    let model_tag = tagged_model(ctx.embedder.model());
```

and pass `&model_tag` to `store_document_with_chunks`.

**Verify**: ingest a document, then
`sqlite3 kb.db 'select distinct embedding_model from chunk'` shows the `+meta1`
suffix.

### Step 4: Widen `list_chunks_for_re_embed` so re-embed can rebuild the prefix

Change the trait method (`store/mod.rs:121`) to return the chunk's metadata
alongside its content. The `chunk` table already stores it as
`metadata_json` (`schema.rs:129`), so this is a column addition to the existing
`SELECT` at `chunks.rs:218`, not a schema change.

Return `(ChunkId, Chunk, Option<String>)` — a full `Chunk` carries both
`content` and `metadata`, which is exactly what `prepare_chunk_for_embedding`
takes, and avoids inventing a tuple shape.

Update:
- `store/mod.rs` trait signature + doc comment
- `store/sqlite/chunks.rs:196-251` implementation (add `c.metadata_json`, parse
  with the same `unwrap_or_else` fallback used at `chunks.rs:386`)
- `store/sqlite/trait_impl.rs:130-138` delegation
- the mock in `tests/kb/retrieve_test.rs:308`

**Verify**: `cargo build --features kb` and the workspace compiles.

### Step 5: Use the prefix in bulk re-embed

```rust
        let (ids, texts): (Vec<_>, Vec<_>) = page
            .into_iter()
            .map(|(id, chunk, _)| (id, prepare_chunk_for_embedding(&chunk)))
            .unzip();
```

and write `tagged_model(embedder.model())` via `update_chunk_embedding`.

**Verify**: `cargo test --features kb --test kb maintenance_test` passes with
updated expectations.

### Step 6: Regression test that would have caught the original bug

Add to `tests/kb/embed_test.rs` (or a new module): ingest a document whose
`category` is a distinctive token that does NOT appear in the chunk body, then
assert the text handed to the embedder contains that token. Use the existing
wiremock harness so the request body is inspectable.

This is the missing test — the previous suite only ever called
`prepare_chunk_for_embedding` directly, never asserted that ingest used it.

**Verify**: the test fails when Step 3 is reverted.

### Step 6b: Note the user-visible tag change

`chunk.embedding_model` values now read `qwen/qwen3-embedding-8b+meta1`, and
that string is surfaced verbatim by `kb drift` (`DriftResponse.by_model`,
`api.rs:465-482`) and the CLI's TOON output. Nothing parses it — `bulk_re_embed`
compares it whole via `skip_model` — but an operator will see the suffix.
Mention it in the docs section below so it does not read as corruption.

### Step 7: Document the migration

Add a section to `docs/reference/kb.md` under drift/re-embed:

- what changed and why,
- that existing corpora must run
  `rantaiclaw kb re-embed --include-current`,
- that `kb drift` now reports pre-metadata chunks as stale by design,
- that re-embedding calls the embedding provider once per chunk and costs money.

## Release impact — state this in the PR body

Every existing Knowledge Base must be re-embedded. This is a one-way step: once
a corpus is re-embedded, reverting the binary leaves metadata-prefixed vectors
being queried by code that no longer builds the prefix. Roll back the binary
AND restore the `kb.db` backup together, or not at all.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb
```

End-to-end against the real binary:

```bash
cargo build --release --features kb
export KB_DB_PATH=/tmp/kbplan90.db
./target/release/rantaiclaw kb ingest ./README.md
sqlite3 $KB_DB_PATH 'select distinct embedding_model from chunk'   # expect +meta1
./target/release/rantaiclaw kb drift                                # expect in_sync
# now simulate a legacy corpus:
sqlite3 $KB_DB_PATH "update chunk set embedding_model='qwen/qwen3-embedding-8b'"
./target/release/rantaiclaw kb drift          # MUST report stale
./target/release/rantaiclaw kb re-embed --include-current
./target/release/rantaiclaw kb drift          # back in sync
```

That legacy-corpus simulation is the control. Without it, "drift is in sync"
proves nothing.

## Done criteria

- All three paths embed via `prepare_chunk_for_embedding`.
- Stored tags carry the recipe; `kb drift` flags a pre-metadata corpus.
- A test fails if ingest stops using the prefix.
- The migration is documented.

## STOP conditions

- The parity harness (`parity_helpers.rs:165`) and production now disagree in
  the *other* direction — align the harness to production, not the reverse.
- Widening `list_chunks_for_re_embed` cascades into more than the four call
  sites listed in Step 4: stop and report the extra consumers before editing.
- Any test asserts a bare model string in `chunk.embedding_model` that you
  cannot update honestly: stop; that would mean something outside the KB
  depends on the tag format.
