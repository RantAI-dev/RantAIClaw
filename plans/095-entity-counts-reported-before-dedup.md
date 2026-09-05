# Plan 095: Report entity counts after global deduplication

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/intelligence/mod.rs src/kb/axi/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: 093, 094 (same function — land in order)
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`IntelligenceSummary.entities` counts raw extractions, not stored entities, so
every surface that shows it contradicts the graph.

`src/kb/intelligence/mod.rs:42,65,90`:

```rust
    let mut n_ent = 0usize;
    // ... per LLM entity:
        n_ent += 1;
    // ... per pattern entity, per chunk:
        n_ent += 1;
```

Entities are then deduplicated globally by `canonical_key` inside
`store_intelligence` (`store/sqlite/intelligence.rs:152-170`). Pattern entities
are deduplicated only *within a chunk* (`extract/pattern.rs:26`), so the same
email in ten chunks increments `n_ent` ten times and stores one row. Plan 093
multiplies LLM mentions per chunk, which makes the gap wider still.

User-visible result: the web console toast says
`Extracted 40 entities · 12 relations` (`claw-ui doc-intelligence-drawer.tsx:61`)
while the Entities tab beside it lists 12, and the graph view's `total_entities`
disagrees with both.

This is the same defect family this repo has shipped before — a headline number
computed from a different set than the breakdown under it.

## Current state (verified at 2ca7e59)

- `IntelligenceSummary { entities: n_ent, relations: n_rel }` — `mod.rs:120-123`
- Consumed by `ReExtractResponse` (`api.rs:716-729`) and the CLI
- `store_intelligence` returns `KbResult<()>` — it does not report how many rows
  actually landed (`store/mod.rs:205-211`)

## Scope

**In scope**: make the reported counts equal what is stored.

**Out of scope**: the graph view's own counting (plan 096).

## Git workflow

```bash
git switch -c fix/intelligence-summary-counts
```

## Steps

### Step 1: Count distinct canonical keys before the store call

The orchestrator already holds the full entity vec. Deduplicate by the same key
the store uses:

```rust
    let unique_entities = entities
        .iter()
        .map(|e| e.canonical_key.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
```

This is the count *this document contributed*. It still differs from
"rows added to the global table" when another document already had the entity —
which is correct and worth saying in the doc comment: the number means "entities
found in this document", and the graph's `total_entities` means "entities in the
corpus". Two different questions, two honest answers.

### Step 2: Report it

```rust
    Ok(IntelligenceSummary {
        entities: unique_entities,
        relations: relations.len(),
    })
```

Use `relations.len()` rather than the running `n_rel` so the same "what was
actually stored" rule applies to both. Drop the now-unused counters.

Update the doc comment on `IntelligenceSummary` (`mod.rs:15-20`) to state the
meaning precisely.

### Step 3: Test it

Drive `extract_document_intelligence` with a stub extractor that emits the same
entity name three times across chunks. Assert `summary.entities == 1`.

**Verify**: red before Step 2.

### Step 4: Align the console wording

In `claw-ui`, `doc-intelligence-drawer.tsx:60-63`, reword the toast so it says
what the number is — e.g. `Found 12 entities · 8 relations in this document`.
This is a one-line change and belongs with the backend fix so the two ship
together.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb intelligence_test
```

claw-ui:

```bash
cd ../claw-ui && npx next build
```

## Done criteria

- Re-extract reports a count that matches the Entities tab beside it.
- The doc comment says which question the number answers.

## STOP conditions

- `store_intelligence`'s dedup key stops being `canonical_key` — then Step 1's
  count is wrong again; re-derive from the store.
