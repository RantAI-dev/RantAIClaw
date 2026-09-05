# 084 — `markdown` backend: `store` never upserts, so `forget` reports a lie

Written against `7114f88`. Risk tier: **MEDIUM** (`src/memory/markdown.rs`; the
tool that calls it advertises deleting sensitive data). Only reached when
`[memory] backend = "markdown"` — the default is `sqlite`
(`src/config/schema.rs:2066`).

Two defects, one root cause.

## Defect A — `store` appends unconditionally

`MarkdownMemory::store` (`src/memory/markdown.rs:186-199`) picks a file by
category and calls `append_to_file`. There is no read-modify-write and no key
lookup, so storing the same key twice writes two lines.

Every other real backend upserts: `SqliteMemory::store` uses
`ON CONFLICT(key) DO UPDATE` (`src/memory/sqlite.rs:674-681`), `PostgresMemory`
matches it. `Memory` is one trait; this is a contract divergence, not a backend
flavour.

```
---- memory::markdown::tests::probe_store_same_key_twice_is_upsert ----
assertion `left == right` failed: one key must count once
  left: 2
 right: 1
```

Consequences: `count()` (which `memory stats` renders as `Total:`) inflates,
`list()` shows the same key twice, and `get()` returns whichever copy sorts first
— which is decided by `timestamp`, and `timestamp` is the **filename**
(`markdown.rs:113`), not a time. `MEMORY.md` → `"MEMORY"`, a daily log →
`"2026-08-08"`; `read_all_entries` sorts descending by that string
(`markdown.rs:175`), so `"MEMORY"` wins on byte order alone.

## Defect B — `forget` stops at the first file that matched

`MarkdownMemory::forget` (`markdown.rs:267-300`) iterates `all_memory_files()`,
filters matching lines out of a file, writes it, and `break`s. A key present in
more than one file loses one copy and keeps the rest — while returning `true`.

`all_memory_files` (`markdown.rs:122-142`) yields `MEMORY.md` first, then the
daily logs, so `store(k, Core)` followed by `store(k, Daily)` leaves the daily
copy behind:

```
---- memory::markdown::tests::probe_forget_across_two_files ----
forget returned true but entry survives: Some(MemoryEntry {
  id: "2026-08-08:0", key: "dupe", content: "daily copy",
  category: Daily, timestamp: "2026-08-08", session_id: None, score: None })
```

Control: the identical two-write-then-forget sequence on `sqlite` passes — one
row, deleted cleanly.

This matters because of who calls it. `MemoryForgetTool`'s description is
"Use to delete outdated facts or sensitive data" (`src/tools/memory_forget.rs:28`)
and it renders `Ok(true)` as `"Forgot memory: {key}"`. The doc comment on
`forget` (`markdown.rs:260-266`) already argues that answering wrongly about a
deletion is unacceptable — the `break` reintroduces the same class of wrong
answer it was written to remove.

## Fix

Order matters: B is a correct fix on its own, A is what stops the duplicate state
from being created.

**B first** — drop the early `break`. Sweep every file, track `removed` per file
so an untouched file is not rewritten, and return whether anything was removed
across all of them. Note the current `removed` flag is shared across the loop and
set inside the filter closure; once the `break` is gone that flag will make every
later file get rewritten. Scope it per file.

**A** — make `store` replace. Read the target file, drop any line whose
`split_stored_entry` key equals the incoming key, then append the new line.
`split_stored_entry` (`markdown.rs:12-20`) is already the inverse of the write
format, so reuse it rather than writing a second parser.

Decide and document one thing explicitly: whether a re-store under a *different*
category moves the entry (delete from the old file, write to the new) or is
rejected. `sqlite` moves it — `ON CONFLICT(key) DO UPDATE SET category = excluded.category`
— so **moving** is the behaviour that keeps the trait consistent. Implement that:
`store` must clear the key from every file it owns before writing, not just from
the file it is about to write.

Preserve what the format is for: lines an operator hand-wrote that
`split_stored_entry` returns `None` for must not be touched, matching how
`forget` already leaves them alone. `MEMORY.md` is a file humans edit.

## Non-goals

- Making `markdown` the default, or giving it real timestamps. The
  filename-as-timestamp shape is ugly and is what makes `get()`'s tie-break
  arbitrary, but changing it touches `parse_entries_from_file`, `recall` ordering
  and `list` ordering. Separate concern; leave a note, do not bundle it.
- `get()`'s `|| e.content.contains(key)` fallback (`markdown.rs`), which lets a
  content substring resolve as a key. Real, but a different defect with a
  different blast radius.

## Validation

- Unit: store the same key twice in `Core` → `count() == 1`, and `get()` returns
  the second value.
- Unit: store a key in `Core`, re-store it in `Daily` → `count() == 1`, `list()`
  shows it once, category is `Daily`, and `MEMORY.md` no longer holds the line.
- Unit: the cross-file `forget` probe above must pass — `forget` returns `true`
  **and** `get()` returns `None`.
- Unit: a hand-written line (no `**key**: ` shape) survives both a `store` of an
  unrelated key and a `forget` of an unrelated key.
- Unit: `forget` of an absent key returns `false` and rewrites no file (assert on
  file mtime *and* contents — contents alone is the weaker check, mtime alone has
  bitten this repo before).
- `cargo test --lib -- memory::markdown`

## Rollback

Two commits (B then A), both confined to `src/memory/markdown.rs`. Revert either
independently. No config or schema change; no migration — existing duplicate
lines in a live workspace are collapsed on the next `store` of that key and are
otherwise harmless.
