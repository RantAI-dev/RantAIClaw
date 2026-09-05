# Plan 091: Contextual retrieval: wire it or remove the knob

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/retrieve/contextual.rs src/kb/config.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M (wire) / S (remove)
- **Risk**: MED
- **Depends on**: 090 (the prefix must reach the embedder first, or wiring this
  changes nothing)
- **Category**: direction
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`generate_contextual_prefixes` (`src/kb/retrieve/contextual.rs:32`) asks an LLM
to write a one-line situating sentence per chunk, so a chunk that says "it
increased by 12%" carries "this discusses Q3 revenue for ACME" into its
embedding. It is gated by `KB_CONTEXTUAL_RETRIEVAL_ENABLED`
(`src/kb/config.rs:83`).

**It has zero production callers** — only `tests/kb/retrieve_test.rs:894`.

The knock-on is larger than one function. `contextual_prefix` is therefore
**never non-`None` anywhere in production**: every producer hard-codes `None`
(`chunk/smart.rs:518`, `chunk/recursive.rs:176`). So the following are all
plumbing for data that does not exist:

- the `contextual_prefix` column (`store/sqlite/schema.rs:130`)
- the field on `ChunkMetadata` and `SearchResult` (`kb/types.rs:41,60`)
- the `Context:` line in `prepare_chunk_for_embedding` (`chunk/prepare.rs:32`)
- the prefix render in `retrieve/mod.rs:291-294`
- both read paths (`chunks.rs:392,416`, `intelligence.rs:532,545`)

An operator who sets `KB_CONTEXTUAL_RETRIEVAL_ENABLED=true` today gets nothing
and no warning.

## Current state (verified at 2ca7e59)

```bash
grep -rn 'generate_contextual_prefixes' src/ tests/
# src/kb/retrieve/contextual.rs:32   (definition)
# tests/kb/retrieve_test.rs:894      (tests only)

grep -rn 'contextual_prefix' src/ | grep -v None
# only column names, struct fields, and read paths — no producer
```

## The decision

**Option A — wire it.** Call `generate_contextual_prefixes` in both ingest
paths between chunking and embedding, write the returned prefixes onto
`chunk.metadata.contextual_prefix`, then embed.

- Pro: the feature works; `prepare_chunk_for_embedding` gains real signal;
  the column and struct fields stop being dead weight.
- Con: one extra LLM call per ingest batch (cost + latency); needs the same
  credential unification as plan 108, since `contextual.rs:45` reads
  `OPENROUTER_API_KEY` from env only and will silently no-op for a
  console-configured key.
- Off by default, so the cost is opt-in.

**Option B — remove it.** Delete `contextual.rs`, the config field
(`config.rs:22,83`), the tests, and the now-unreachable `contextual_prefix`
plumbing listed above.

- Pro: honest; removes a knob that does nothing (CLAUDE.md §3.2).
- Con: throws away a real retrieval-quality technique and a working port.

Recommendation: **A**, but only after 090 lands — wiring it before the prefix
reaches the embedder puts the text in the DB and still not in the vector.

## Scope

**In scope**: one option, fully, including the dependent plumbing.

**Out of scope**: credential unification — plan 108. If you take Option A,
depend on it and say so, rather than duplicating a fix here.

## Git workflow

```bash
git switch -c feat/wire-contextual-retrieval   # or chore/… for option B
```

## Steps (Option A)

### Step 1: Call it during ingest

In `api.rs` ingest and `cli.rs` `cmd_ingest`, after `smart_chunk_document` and
before the embed call:

```rust
    // Opt-in; returns a vec of empty strings when disabled or unavailable, so
    // the zip below is always length-correct.
    let bodies: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let prefixes = generate_contextual_prefixes(&ctx.cfg, &doc_text, &bodies).await;
    for (chunk, prefix) in chunks.iter_mut().zip(prefixes) {
        if !prefix.trim().is_empty() {
            chunk.metadata.contextual_prefix = Some(prefix);
        }
    }
```

Read `contextual.rs:32-46` for the exact signature and the empty-vec contract
before writing this — the fail-soft shape matters.

**Verify**: with the flag off, the stored `contextual_prefix` column stays
NULL and ingest timing is unchanged.

### Step 2: Prove it reaches the vector

Extend the plan-090 regression test: with the flag on and a stubbed chat
endpoint returning a known prefix, assert the text sent to the embedder
contains it. Without this the feature can be "wired" and still inert.

### Step 3: Document it

`docs/reference/kb.md` — what the flag does, that it costs one chat call per
ingest, and that it requires a chat credential.

## Steps (Option B)

1. Delete `src/kb/retrieve/contextual.rs` and its `pub mod` line.
2. Remove `contextual_retrieval_enabled` + `contextual_retrieval_model` from
   `KbConfig` (`config.rs:22-23,83-86`).
3. Remove `contextual_prefix` from `ChunkMetadata`/`SearchResult`, the DB
   column, `prepare.rs:32-37`, `retrieve/mod.rs:291-294`, and the four read
   paths. This is the larger half of the work — do it, or the dead plumbing
   survives the deletion of the feature.
4. Delete `retrieve_test.rs:894-1004` and `format_includes_contextual_prefix_when_present`
   (`:628`).
5. Note the removal in `docs/reference/kb.md` and the CHANGELOG.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb retrieve_test
cargo test --features kb --test kb
```

## Done criteria

- Either the flag produces a prefix that demonstrably reaches the embedding
  input, or the flag and all its plumbing are gone.
- No third state.

## STOP conditions

- Plan 090 has not landed. Wiring this first stores prefixes that never reach a
  vector — the exact defect 090 fixes. Stop.
