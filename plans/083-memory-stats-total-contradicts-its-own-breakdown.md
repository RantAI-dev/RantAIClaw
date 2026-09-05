# 083 — `memory stats` prints a total its own breakdown contradicts

Written against `7114f88`. Risk tier: **LOW** (`src/memory/cli.rs`, display only).
Affects every backend once the store passes 1000 entries.

`handle_stats` (`src/memory/cli.rs:296`) reads the total and the per-category
breakdown from two different sources:

```rust
let total = mem.count().await.unwrap_or(0);          // authoritative
...
let all = mem.list(None, None).await.unwrap_or_default();   // a page, not a total
for entry in &all { *counts.entry(entry.category.to_string()).or_default() += 1; }
```

`SqliteMemory::list` caps at `DEFAULT_LIST_LIMIT = 1000`
(`src/memory/sqlite.rs:986`, applied at `:1015` and `:1030`). So past 1000 rows
the `Total:` line and the `By category:` block disagree, with nothing on screen
saying the breakdown is partial.

This is the third instance of the same mistake in this codebase; the other two
were already fixed and both carry a comment warning about it:

- `src/memory/cli.rs` `handle_list` — "`list` is capped by the backend, so its
  length is a page size, not a total"
- `src/tui/commands/memory.rs:149-160` — same fix, same reasoning
- `src/memory/sqlite.rs:983-985` — the cap itself documents the contract:
  "Callers render `list().len()` as a total. It is not one past this cap"

`handle_stats` was missed.

## Evidence

```
---- memory::sqlite::tests::probe_list_len_matches_count_past_the_cap ----
assertion `left == right` failed: stats breakdown is built from list(), which returned 1000 of 1100
  left: 1000
 right: 1100
```

Control in the same probe passed: `count()` returned 1100.

## Second defect in the same function — `count()` failure renders as `Total: 0`

`mem.count().await.unwrap_or(0)` swallows the error. A store that cannot be read
is then indistinguishable from an empty one, and `Health:` does not cover the gap
because the two checks can disagree:

```
---- memory::markdown::tests::probe_count_failure_is_visible_next_to_health ----
count() failed but health_check() still says healthy, so stats prints
`Total: 0` + `Health: healthy` for an unreadable store
```

Control passed: an unreadable daily file does make `count()` return `Err`.
`MarkdownMemory::health_check` is `self.workspace_dir.exists()`
(`src/memory/markdown.rs:307`), so it stays `true`. The same shape exists on
`sqlite`, where `health_check` is `SELECT 1` and survives a damaged `memories`
table.

## Fix

In `handle_stats`:

1. Keep `Total:` from `count()`. When the breakdown is built from a capped
   `list()`, label it — mirror the wording `handle_list` already ships
   ("listing the most recent N") so the two commands read consistently. Only add
   the qualifier when `total > listed`, as `handle_list` does.
2. Surface a `count()` error instead of printing `0`. `handle_stats` returns
   `Result<()>`, so either propagate it, or print the error next to `Total:` and
   still show the rest of the block. Prefer the latter: `stats` is the command an
   operator runs *because* memory is misbehaving, so it should print what it can
   rather than bail.

Do not raise `DEFAULT_LIST_LIMIT`, and do not add a `count_by_category` trait
method for this. The cap exists on purpose and a new trait method would need an
implementation in all six backends plus every test mock — rule-of-three is not
met by one display caller.

## Optional, same file — TUI/API parity

`stats_memory` (`src/tui/commands/memory.rs:322`) and `memory_stats`
(`src/gateway/api_v1.rs:1709`) report backend/total/health with no breakdown.
Neither is wrong; they are just less informative than the CLI. Out of scope
unless the breakdown is wanted there — if it is, it must carry the same cap
qualifier, or it reintroduces this bug on two more surfaces.

## Validation

- Unit: `handle_stats` is `async fn(&Config)` and prints to stdout, so assert on
  the shared render instead — extract the breakdown+qualifier into a pure
  `fn(listed: &[MemoryEntry], total: usize) -> String` and test it directly with
  `listed.len() < total`. Keep the extraction minimal; do not restructure
  `handle_stats` beyond what the test needs.
- Unit: the same helper with `count()` unavailable must not render `0` as a total.
- `cargo test --lib -- memory::cli memory::sqlite`
- Manual: seed >1000 entries, run `rantaiclaw memory stats`, confirm the
  breakdown is labelled and its sum is explainable against `Total:`.

## Rollback

Single small commit, display-only. Revert directly; no data or config contract
changes.
