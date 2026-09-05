# Plan 288: Trim and compact history at turn boundaries so tool pairs never break

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/agent/agent.rs src/agent/loop_.rs src/agent/compaction.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-1) — the only defect found that bricks a live session
- **Effort**: M
- **Risk**: LOW–MED (changes which messages survive a trim)
- **Category**: bug
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

`AssistantToolCalls` and `ToolResults` are two separate entries in the history vector, but
every trim path cuts by **count**. A cut that lands between them leaves a `tool` result whose
originating `tool_calls` is gone. OpenAI and Anthropic both reject that shape with a 400, so
once the cut happens every later turn in that session fails until the user clears it.

The default `max_history_messages` is 50 and each tool iteration appends two entries, so a
tool-heavy session reaches the boundary in a handful of turns. This is not a rare edge.

A pairing-safe splitter already exists — `compaction::compute_split_index` cuts before a user
turn — but the trim paths do not use it.

## Current state (verified at `4b8f61e`)

```rust
// src/providers/traits.rs:94-99 — the two halves are separate variants
AssistantToolCalls { text: Option<String>, tool_calls: Vec<ToolCall> },
ToolResults(Vec<ToolResultMessage>),
```

```rust
// src/agent/agent.rs:911-913 — count-based
fn trim_history(&mut self) {
    let max = self.config.max_history_messages;
    if self.history.len() <= max { return; }
```

Also count-based: `src/agent/loop_.rs:139` `trim_history`, and
`src/agent/loop_.rs:184` `auto_compact_history` (compacts an arbitrary index range).

The safe primitive: `src/agent/compaction.rs:105` `compute_split_index`.

The existing test at `src/agent/loop_.rs:4146`
(`trim_history_preserves_system_prompt`) seeds text-only turns, which is why this has
never failed in CI.

## Steps

1. **Read `compute_split_index` and decide whether it can serve all three call sites**, or
   whether a smaller shared helper ("given a history and a target length, return a cut index
   that never separates a tool-call pair") is the better fit. Prefer reusing what exists.
   **Verify**: write the chosen rule down in the PR description before coding.

2. **Route `agent.rs:911` through it.** Keep the system-message preservation the current code
   already does.

3. **Route `loop_.rs:139` and the compaction range at `loop_.rs:184` through it.**
   **Verify**: `rg -n 'drain\(|\[start\.\.' src/agent/` shows no remaining raw index cut on
   history.

4. **Test the shape that production actually produces.** Seed a history that alternates
   user → `AssistantToolCalls` → `ToolResults` until it exceeds `max_history_messages`, then
   trim, then assert: every surviving `ToolResults` has a preceding `AssistantToolCalls` in
   the same history. Assert the invariant, not a fixed length.
   **Verify**: `cargo test --lib agent` passes; the new test **fails** if you revert step 2
   (prove it bites).

5. **Check the fallback.** If a pairing-safe cut cannot reach the target length (e.g. one
   enormous tool exchange), decide and document what happens — drop the orphaned
   `ToolResults` too, rather than emit an invalid history.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib agent` passes with the new invariant test.
- Reverting any one of the three call sites makes the new test fail.

## STOP conditions

- `compute_split_index` turns out to be unusable for these call sites and a new helper would
  duplicate its logic → STOP and report; that is a design call.
- The fix would change the *number* of messages sent to providers in the common no-tool case
  → STOP; this plan must not alter ordinary chat behaviour.

## Test plan

One invariant test per call site, in the module that owns it. Follow the naming already used
(`<subject>_<expected_behavior>`).

## Maintenance note

Any future code that shortens history must go through the pairing-safe cut. The two-variant
representation in `traits.rs` is what makes a count-based cut unsafe.

## Rollback

Single commit across three files plus tests. No schema or storage change.
