# Plan 109: Surface extraction failures instead of logging them away

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/intelligence/extract/llm.rs src/kb/axi/api.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: S–M
- **Risk**: LOW
- **Depends on**: 097 (soft — the capability block is where the signal lands)
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Document intelligence can fail completely and report success.

`CombinedLlmExtractor::extract` (`src/kb/intelligence/extract/llm.rs:134-218`)
loops over chunks and `continue`s past every failure mode — transport error
(`:154-157`), non-2xx (`:161-169`), undeserializable response (`:171-177`),
empty `choices` (`:179-185`), content that is not valid extraction JSON
(`:187-197`). Each logs a `warn` and moves on. The function returns
`Ok(Extracted::default())` even when **every** chunk failed.

An empty `Extracted` is indistinguishable from "this document genuinely has no
entities".

Two things make the silence complete:

- Ingest runs extraction fire-and-forget (`api.rs:1412-1437`), so even a real
  error never reaches the HTTP response.
- The console's empty state says extraction "may not have run yet … Try
  Re-extract" (`doc-intelligence-drawer.tsx:99-101`) — and Re-extract will fail
  the same way, forever, with the same message.

The most likely cause in practice is a bad or missing credential: `warn` lines
in a daemon log are not where an operator looks.

There is also a smaller leak. `llm.rs:161-168` logs the raw upstream body:

```rust
                tracing::warn!(status = status.as_u16(), body = %text, ...);
```

That is inconsistent with the deliberate care elsewhere — `api.rs:349-355` maps
an upstream failure to a status code and refuses to surface the body.

## Current state (verified at 2ca7e59)

- `Extracted` — `extract/mod.rs:8-12`; carries no failure information
- `extract_document_intelligence` returns `IntelligenceSummary` — `mod.rs:120`
- `ReExtractResponse { document_id, entities, relations }` — `api.rs:716-729`

## Scope

**In scope**: carry per-chunk failure counts out of the extractor; report them
on the re-extract response; stop logging upstream bodies.

**Out of scope**: making ingest's extraction synchronous. Fire-and-forget is
the right call for a slow LLM pass; the fix is reporting, not blocking.

## Git workflow

```bash
git switch -c fix/surface-extraction-failures
```

## Steps

### Step 1: Count failures in `Extracted`

```rust
pub struct Extracted {
    pub entities: Vec<(usize, String, EntityType, f32)>,   // shape per plan 093
    pub relations: Vec<(String, String, RelationType, f32)>,
    /// Chunks the extractor could not process at all. A non-zero value with
    /// zero entities means extraction FAILED — it does not mean the document
    /// has no entities. Callers must be able to tell those apart.
    pub failed_chunks: usize,
    /// First failure reason, for operator-facing display. Never contains the
    /// upstream body or any credential.
    pub first_error: Option<String>,
}
```

Increment at each `continue`; record a short reason (`"http 401"`,
`"invalid json"`, `"transport"`) the first time only.

### Step 2: Stop logging the upstream body

Replace `body = %text` at `llm.rs:164` with the status alone, matching
`api.rs:349-355`. Keep the body out of logs entirely.

**Verify**: `grep -n 'body = %' src/kb/intelligence/extract/llm.rs` is empty.

### Step 3: Carry it through the summary

Add `failed_chunks: usize` and `error: Option<String>` to
`IntelligenceSummary` (`intelligence/mod.rs:15-20`) and pass them through.

### Step 4: Report on re-extract

`ReExtractResponse` gains the two fields. Re-extract is a **synchronous**,
operator-initiated action (`api.rs:1019-1043`), so this is where the truth can
actually be delivered. When every chunk failed, return a `502` with the reason
instead of a `200` claiming zero entities — a total failure is not a successful
extraction of nothing.

### Step 5: Warn once per document on the ingest path

The fire-and-forget task at `api.rs:1417-1436` already logs. Make it log the
failure count and reason rather than only the error, so one line tells the
operator what happened.

### Step 6: Console

`doc-intelligence-drawer.tsx` — when the re-extract response reports failures,
show the reason instead of "may not have run yet". Combined with plan 097's
`credential_configured`, the empty state can finally name the cause.

### Step 7: Tests

In `intelligence_test.rs`, with a stubbed chat endpoint:

1. all chunks 401 → `failed_chunks == chunk count`, re-extract returns 502
2. control: all chunks succeed → `failed_chunks == 0`, 200
3. mixed → partial counts, still 200

Test 2 is what stops an over-eager fix from turning every extraction into an
error.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb intelligence_test
cargo test --features kb --test kb api_test
cd ../claw-ui && npx next build
```

## Done criteria

- A wholly-failed extraction is reported as a failure, not as zero entities.
- No upstream body reaches the logs.
- Both the success and total-failure paths are pinned.

## STOP conditions

- Plan 093 has changed `Extracted`'s shape differently — reconcile rather than
  reverting its change.
