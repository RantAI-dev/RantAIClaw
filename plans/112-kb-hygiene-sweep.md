# Plan 112: KB hygiene sweep: byte-vs-char, section pollution, perf, stale comments, dead config

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/ src/persona/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `2ca7e59`, 2026-08-10

> **Split freely.** This is a bundle of independent small items found during
> the sweep. None is worth its own plan; each is worth doing. If any turns out
> larger than described, lift it into its own plan rather than growing this one.
> Prefer several small commits over one.

## Why this matters

None of these breaks the Knowledge Base on its own, which is exactly why they
accumulated. Three of them are correctness bugs that only show on non-Latin
text or large documents — the cases least likely to appear in a dev loop. Three
are comments that describe behaviour the code does not have — the same thing
that let the bigger findings in this effort go unnoticed. And two are config
surfaces served over HTTP that nothing consumes.

Taken together they are the difference between a subsystem that is merely
working and one a stranger can trust.

## Scope

**In scope**: the nine items below, each independently.

**Out of scope**: anything that grows past its description. Items 3, 4 and 6 are
performance items — if a measurement shows the current behaviour is fine,
closing the item with that number recorded is a valid outcome and better than a
speculative rewrite. Item 8 (`always_on_kbs`) may only be *removed* here; wiring
it to the agent is a feature and needs its own plan.

## Items

### 1. `is_unpdf_sufficient` compares bytes against a character threshold

`src/kb/extract/text_layer_signals.rs:128-140`:

```rust
    if text.is_empty() || text.len() < opts.min_chars_per_page * pages {
```

`text.len()` is **bytes**; `min_chars_per_page` is documented as characters
(`:10-12`). For non-Latin scripts — Chinese, Japanese, Arabic, Cyrillic, all
2-3 bytes per character — a thin extraction clears the threshold and OCR
fallback never fires. `has_columnar_lines:44` has the same `line.len() < 10`.

Fix: use `text.chars().count()`. Keep `min_text_filesize_ratio`
(`:161-165`) on bytes — bytes over bytes is consistent there.

Test: a CJK string whose character count is below the threshold but whose byte
length is above must be judged insufficient. That test fails today.

### 2. `update_hierarchy` receives a whole block, not a heading line

`src/kb/chunk/smart.rs:146-150` passes the entire block to `update_hierarchy`,
which strips leading `#` and keeps the rest (`:428-436`). Blocks are split on
blank lines (`:257-265`), so `"# Title\nbody text"` — a heading with no blank
line after it — yields a hierarchy entry of `"Title\nbody text"`. That leaks
into `section` via `hierarchy_path.join(" > ")` (`:503-509`) for every
non-heading block that inherits the path.

Fix: pass only the first line, or reuse the captured group `detect_structure`
already extracts cleanly (`:284-292`).

Test: heading immediately followed by prose yields `section == "Title"`.

### 3. `pdf_splitter` clones the whole document per segment

`src/kb/extract/pdf_splitter.rs:56`: `let mut segment = source.clone();` inside
the loop. A 500-page PDF split into 5-page segments performs 100 full clones of
the parsed document. It is inside `spawn_blocking` so the executor is safe, but
memory is not.

Fix: build each segment from a single shared parse, or cap `pages_per_segment`
work by streaming. Measure before and after on a large PDF and put both numbers
in the PR — if the clone is cheap in practice, say so and close the item.

### 4. `graph()` scans the whole relation table per request

`src/kb/store/sqlite/intelligence.rs:324-336` selects every row of
`entity_relation` and filters in Rust against the node set. Push the filter into
SQL with an `IN (…)` over the selected node ids — the set is already bounded by
`limit` (default 200, hard cap 5000 at `api.rs:69`).

Careful: `IN` with 5000 parameters can exceed SQLite's variable limit. Chunk the
ids or use a temp table. Add a test at the hard cap so the limit is exercised.

### 5. `try_substitute` searches from position 0 for every prose block

`src/kb/extract/hybrid.rs:298` calls `find_normalized(…, 0)` per block, so the
merge is O(blocks × layer length) and a repeated phrase can substitute
out of order. Thread a running offset through `merge_structural_with_text_layer`
(`:336-359`) so each block searches forward from the previous match.

Test: a document with a repeated sentence merges in document order.

### 6. LLM extraction is sequential with no cap

`src/kb/intelligence/extract/llm.rs:137` — one POST per chunk, serially, with
no concurrency and no ceiling. A 200-chunk document makes 200 sequential calls.
`embed_many_via_http` (`embed/openrouter.rs:214-290`) already has the worker
pattern to copy, and `cfg.embed_concurrency` is a reasonable bound to reuse.

This is fire-and-forget on ingest so it blocks nothing, but it is slow and
costly. If plan 093 has landed, note that per-chunk mentions make throughput
matter more.

### 7. Stale comments

- `src/kb/axi/api.rs:12-17` describes `KB_CTX` as a `OnceCell` whose failures
  persist "until the process restarts". It is a `Mutex<Option<CachedCtx>>`
  (`:157`) with `clear_kb_ctx` (`:199`). (Plan 105 may already fix this —
  check first.)
- `src/kb/store/sqlite/chunks.rs:4-6` claims the dimension contract is enforced
  before any INSERT. It compares against the configured dimension, not the
  table's. (Plan 098 fixes the behaviour; make sure the comment followed.)
- `src/kb/store/sqlite/schema.rs:138-140` points at a `bulk_re_embed` migration
  path for dimension changes that does not exist. (Plan 098.)

Verify each is already handled before editing; do not re-fix.

### 8. `persona.always_on_kbs` is dead config

`src/persona/mod.rs:123` stores it, `src/gateway/api_v1.rs:1618,1699-1710`
reads and writes it over HTTP, and **nothing consumes it** —
`grep -rn 'always_on_kbs' src/agent/ src/tools/` is empty.

Either wire it (the agent should scope KB search to the persona's knowledge
bases) or remove it from the API and the struct. Wiring is a real feature and
belongs in its own plan; removing is this plan's job unless someone claims it.
Decide, do not leave it.

### 9. `DocumentTypeHint` is threaded and never read

`src/kb/file/mod.rs:41-49`. Plan 110 may cover this — check first.

## Git workflow

```bash
git switch -c chore/kb-hygiene-sweep
```

One commit per item, each with its own test where a test is possible.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb
```

Per-item verification is described inline above. Items 3 and 4 need a
before/after measurement in the PR body, not just a green suite.

## Done criteria

- Items 1, 2, 5 have a regression test each that is red at `2ca7e59`.
- Items 3, 4, 6 have measured numbers or a written decision to close as
  not-worth-it.
- Items 7-9 leave no comment or field that describes behaviour the code does
  not have.

## STOP conditions

- Item 4's `IN` clause exceeds SQLite's parameter limit at the 5000 hard cap:
  chunk it or stop and report; do not silently lower the cap.
- Item 8: someone is depending on `always_on_kbs` through the HTTP API. Removing
  it would be a breaking API change — stop and report.
