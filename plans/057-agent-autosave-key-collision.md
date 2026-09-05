# 057 — Agent auto-save overwrites itself every turn

- **Finding:** #2 (memory deepscan, wave 0)
- **Written against:** `9781696`
- **Risk tier:** medium (`src/agent/**`)
- **Effort:** XS
- **Depends on:** nothing
- **Blocks:** nothing

## Problem

`Agent::turn_inner` auto-saves the user message under the literal key `"user_msg"`
(`src/agent/agent.rs:866-873`). The `memories` table declares `key TEXT NOT NULL UNIQUE`
and `store` upserts with `ON CONFLICT(key) DO UPDATE` (`src/memory/sqlite.rs:133`,
`:463-472`), so every turn overwrites the previous one. One row exists, ever.

`Agent` is what the TUI (`tui/app.rs:7387`, `tui/async_bridge.rs:75,188`) and the gateway
(`gateway/api_v1.rs:501,594`) run on — the two interactive surfaces. Their entire
auto-save history is a single row holding the most recent message.

The CLI path already fixed exactly this: `loop_.rs:120` derives a per-turn key with
`autosave_memory_key`, and `loop_.rs:3986` tests that consecutive turns both survive. The
fix never reached `Agent`.

Secondary effect: `store` also writes `session_id = excluded.session_id`, so the single
row's scope flips to whichever conversation wrote last.

## Change

### Files in scope

- `src/memory/mod.rs` — host the shared key builder
- `src/agent/loop_.rs` — use the shared builder instead of a local copy
- `src/agent/agent.rs` — use the shared builder
- `src/agent/tests.rs` — two assertions pin the literal key

### Files explicitly out of scope

- `src/channels/mod.rs` — already derives a unique key per message
  (`conversation_memory_key`); leave it
- Retention behaviour — accumulating rows are bounded by
  `conversation_retention_days` via existing hygiene; do not add new pruning here
- Conversation scoping — `conversation_id` is never set in production today; that is
  finding #4 and belongs to plan 065

### Steps

1. Move `autosave_memory_key` from `src/agent/loop_.rs:120-122` to `src/memory/mod.rs`,
   `pub`, next to `is_assistant_autosave_key`. Those two are the write and read halves of
   one convention and belong together.

2. In `loop_.rs`, delete the local definition and call the shared one at both call sites
   (`:2284`, `:2451`). Keep the existing unit tests; retarget them at the new path.

3. In `agent.rs:866-873`, replace the literal `"user_msg"` with
   `memory::autosave_memory_key("user_msg")`.

4. Update `src/agent/tests.rs:657` and `:1084` — both assert the literal key. They must
   assert the *prefix* instead, otherwise they encode the bug.

### Why centralise rather than duplicate

Normally rule-of-three says duplicate. This is not incidental formatting: the uniqueness
of an auto-save key is a correctness invariant, and `Agent` got it wrong precisely by not
having it. `is_assistant_autosave_key` — the matching read-side predicate — already lives
in `memory/mod.rs`. One definition, two halves, one place.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
cargo test --lib agent::
```

Not a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

In `src/agent/tests.rs`, add `agent_autosave_preserves_every_turn`: drive two turns
through a stub provider, then assert `memory.list(...)` holds two `user_msg_*` entries with
different keys and both message bodies present.

Verify it is not vacuous: with the literal key restored, it must fail with one entry
rather than two. A test that only checks "an entry exists" passes either way.

Retarget the two existing literal-key assertions to prefix matching in the same commit —
leaving them would fail the build and hide which assertion actually protects the fix.

## Escape hatch

If any production code looks up `"user_msg"` by exact key (not prefix), STOP and report —
that would make the key part of a contract rather than an internal detail. Current grep
shows only `agent.rs` writing it and two tests reading it.

## Maintenance note

Every auto-save write site must go through `memory::autosave_memory_key`. A future writer
that hardcodes a key reintroduces this. The read-side filter
`is_assistant_autosave_key` sits beside it as the reminder.

## Rollback

One commit across four files, no schema or config change. `git revert` is complete;
rows already written under unique keys stay valid and are pruned by existing retention.
