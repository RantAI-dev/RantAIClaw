# Plan 096: Make graph node selection and reported degree the same metric

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/store/sqlite/intelligence.rs`
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
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`graph()` picks which nodes to return using one definition of degree, then
overwrites the value it reports with a different one.

Selection — `src/kb/store/sqlite/intelligence.rs:266-293`:

```sql
                 COUNT(DISTINCT r.id) AS degree
                 ...
                 ORDER BY degree DESC
                 LIMIT ?1
```

That counts relation **rows**. Then, after edges are deduplicated by
`(source, target, relation_type)` at `:321-345`, the reported degree is
recomputed from the deduplicated edge set — `:347-359`:

```rust
            // degree = incident DEDUPED edges (overwrites the SQL
            // COUNT(DISTINCT r.id) ordering key used for node selection above).
```

The comment records the overwrite but not the consequence: the top-N cut is
made on one metric and displayed on another.

Concretely: an entity extracted with the same `(target, type)` from 50
documents has 50 relation rows but one deduplicated edge. It wins a top-200
slot on `degree: 50` and then renders as `degree: 1`. An entity with 3 genuinely
distinct edges scores 3, misses the cut, and never appears — even though it is
the better-connected node by the metric the UI shows.

The graph view therefore over-represents repeatedly-extracted pairs and
under-represents genuinely connected entities, and the numbers next to the
nodes do not explain why those nodes were chosen.

## Current state (verified at 2ca7e59)

- Node SQL (both the grouped and ungrouped branch): `intelligence.rs:266-294`
- Edge dedup: `:319-345`
- Degree overwrite: `:347-359`
- `api_test.rs:723 graph_dedupes_edges_weights_and_recomputes_degree` pins the
  *reported* degree being the deduplicated one — that behaviour stays.

## Design decision

Select on the same metric that is reported: distinct
`(source, target, relation_type)` incident to the entity. Deduplicated degree is
what the UI shows and what a reader means by "connected", so it should also
decide who makes the cut.

Do not go the other way (report row counts) — `graph_dedupes_edges_weights_and_recomputes_degree`
pins the current display, `GraphEdge.weight` already carries the row multiplicity
(`store/mod.rs:164`), and row counts are the less meaningful number.

## Scope

**In scope**: the `ORDER BY` key in both branches of the node query.

**Out of scope**: the full-table scan at `:324-326` (plan 112), soft-delete
filtering (deliberate — see `intelligence_test.rs:531`).

## Git workflow

```bash
git switch -c fix/graph-degree-metric-consistency
```

## Steps

### Step 1: Order by distinct edges

Replace `COUNT(DISTINCT r.id)` with a count over the deduplicated triple in
both the grouped and ungrouped node SQL:

```sql
        COUNT(DISTINCT r.source_entity_id || char(31) || r.target_entity_id
              || char(31) || r.relation_type) AS degree
```

Use SQLite's `char(31)` (unit separator), not a literal `'|'` or a Rust `\x1f`
escape. Entity ids are UUIDs, but `relation_type` can be
`RelationType::Other(String)` straight from the model
(`intelligence/types.rs:108-109`), so an ordinary punctuation delimiter can
appear inside a value and merge two distinct triples into one.

Note the two branches differ: the ungrouped `node_sql` is a plain `&'static str`
literal, the grouped one is too — but the `total_entities` / `total_relations`
queries below use `format!` (`intelligence.rs:369,374-377`), where any literal
brace would need doubling. This edit touches only the two `node_sql` literals.
Keep `doc_count` as it is.

**Verify**: `cargo test --features kb --test kb api_test` — the three graph
tests (`:534`, `:579`, `:622`, `:723`) must still pass. If `:723` fails, the
reported degree changed, which this plan must not do — stop.

### Step 2: Update the comment at `:347-359`

It currently explains an overwrite that is no longer a change of metric. Say
instead that the SQL orders by the same deduplicated definition and the Rust
pass recomputes it exactly for the returned node set.

### Step 3: Pin the selection

Add a test: seed one entity with 5 duplicate relation rows for the same
`(target, type)`, and a second entity with 2 distinct edges. Request
`limit = 1`. Assert the **second** entity is returned.

That is the behaviour that was wrong, and no existing test covers node
selection under a cap with duplicate rows.

**Verify**: red before Step 1.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb api_test
cargo test --features kb --test kb intelligence_test
```

## Done criteria

- Node selection and reported degree use one definition.
- The new selection test is red without the fix.
- All four existing graph tests still pass unchanged.

## STOP conditions

- `graph_dedupes_edges_weights_and_recomputes_degree` goes red: the reported
  degree changed. Revert and re-derive — this plan changes only the ordering
  key.
