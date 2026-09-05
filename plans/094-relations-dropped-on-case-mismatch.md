# Plan 094: Wire relations by canonical key, and count what is dropped

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/intelligence/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: 093 (soft — same function, land in order to avoid conflicts)
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Extracted relations are matched to entities by **raw name**, while entities are
deduplicated by a **lowercased canonical key**. The two disagree, and the
mismatch silently deletes edges.

`src/kb/intelligence/mod.rs:64` and `:89`:

```rust
        entity_id_by_name.entry(name.clone()).or_insert(id);
```

`src/kb/intelligence/mod.rs:97-110`:

```rust
    for (src, tgt, rty, conf) in &llm.relations {
        if let (Some(s), Some(t)) = (entity_id_by_name.get(src), entity_id_by_name.get(tgt)) {
            relations.push(Relation { ... });
            n_rel += 1;
        }
    }
```

There is no `else`. A relation whose endpoint name does not match an entity
name **byte for byte** is dropped with no log, no counter, no error.

Meanwhile `canonical_key` (`intelligence/resolve.rs:7-15`) lowercases, trims,
collapses whitespace and strips surrounding ASCII punctuation. So "RantAI" and
"rantai" are the same entity but different relation keys.

LLMs routinely vary casing and punctuation between the `entities` array and the
`relations` array of the same response — the prompt (`extract/llm.rs:111-126`)
does not require consistency between them. Edge loss is the common case, not
the edge case.

`IntelligenceSummary.relations` reports `n_rel`, which counts only the
survivors, so the loss is invisible from every surface.

## Current state (verified at 2ca7e59)

- Entity dedup key: `canonical_key(name, type)` — lowercase
- Relation lookup key: raw `name` — case-sensitive
- `n_rel` incremented only inside the successful branch (`mod.rs:108`)
- No test covers a casing mismatch between entity and relation names

## Scope

**In scope**: key the relation lookup by the same normalization entities use,
and surface dropped relations.

**Out of scope**: changing `canonical_key` itself, or the `fuzzy` mode
(plan 097).

## Git workflow

```bash
git switch -c fix/relation-wiring-canonical-key
```

## Steps

### Step 1: Key the lookup the way entities are deduplicated

Relations carry endpoint names but not types, while `canonical_key` needs both.
Build a name-only normalized index that mirrors the name half of
`canonical_key`, and expose the normalizer so the two cannot drift:

In `resolve.rs`, extract the name normalization:

```rust
/// The name half of [`canonical_key`]. Kept public so relation wiring uses the
/// exact same normalization entity dedup uses — a divergence here silently
/// drops edges (see plan 094).
pub fn normalize_name(name: &str) -> String {
    name.trim()
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}
```

and make `canonical_key` call it, so there is one implementation.

**Verify**: `canonical_key` tests still pass unchanged.

### Step 2: Use it on both sides

In `intelligence/mod.rs`, key `entity_id_by_name` by `normalize_name(&name)`
and look up with `normalize_name(src)` / `normalize_name(tgt)`.

Note the collision this introduces: two entities with the same name but
different types now share a lookup slot. `or_insert` keeps the first, which is
the same tie-break `canonical_key` dedup uses — acceptable, and better than
dropping the edge. Record that in a comment.

### Step 3: Count and log what is still dropped

```rust
    let mut dropped_relations = 0usize;
    for (src, tgt, rty, conf) in &llm.relations {
        match (
            entity_id_by_name.get(&normalize_name(src)),
            entity_id_by_name.get(&normalize_name(tgt)),
        ) {
            (Some(s), Some(t)) => { /* push, n_rel += 1 */ }
            _ => dropped_relations += 1,
        }
    }
    if dropped_relations > 0 {
        tracing::warn!(
            target: "kb::intelligence",
            document_id,
            dropped = dropped_relations,
            "relations referenced entity names with no extracted entity"
        );
    }
```

Do **not** log the names — they are document content.

### Step 4: Regression test

Add a test driving `extract_document_intelligence` with a stub extractor that
emits entity `"TechCorp"` and a relation whose source is `"techcorp"`. Assert
the relation survives. It must fail before Step 2.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb intelligence_test
```

## Done criteria

- A casing/punctuation mismatch between entity and relation names no longer
  drops the edge.
- Genuinely unmatched relations are counted and logged once per document.
- The new test is red without the fix.

## STOP conditions

- Making `normalize_name` public changes `canonical_key` output for any input:
  it must not. Assert the existing `canonical_key_merges_same_entity_across_casing_and_whitespace`
  test (`intelligence_test.rs:107`) still passes unchanged; if it does not,
  stop.
