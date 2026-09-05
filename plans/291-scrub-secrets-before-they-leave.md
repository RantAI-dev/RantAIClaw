# Plan 291: Scrub credentials on the two paths that carry them out of the process

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/agent/loop_.rs src/agent/agent.rs src/cron/scheduler.rs src/cron/store.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-3, part b)
- **Effort**: S–M
- **Risk**: LOW
- **Category**: security
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

Two paths move text out of the process without the redaction their siblings apply.

**Autosave.** Four call sites sanitize before storing a memory — the channel dispatcher, the
memory tool, the config API and the CLI. The agent loop's autosave does not. A credential
typed into the TUI lands verbatim in `brain.db`, is returned by `memory_recall`, is served by
`GET /api/v1/memory`, and travels back to the provider on the next recall.

**Cron announcements.** Run history is scrubbed and truncated before storage — the code calls
that store "a world-readable payload sink". The same output is posted to the announcement
channel **raw**, which may be a group chat. The protection is on the quieter path.

## Current state (verified at `4b8f61e`)

```rust
// src/agent/loop_.rs:2374 and :2546 — raw message stored
let user_key = autosave_memory_key("user_msg");
// src/agent/agent.rs:1068-1070 — same shape
&crate::memory::autosave_memory_key("user_msg"),
... MemoryCategory::Conversation,
```

The sanitizer those sites skip is `crate::memory::sanitize_memory_content`
(`rg -n 'sanitize_memory_content' src/` shows the four sites that do use it).

```rust
// src/cron/store.rs:477 — history path: scrubbed AND truncated
output.map(|o| truncate_cron_output(&crate::agent::loop_::scrub_credentials(o)));
// src/cron/scheduler.rs:472 — announcement path: raw
if let Err(e) = deliver_if_configured(config, job, output).await {
```

`scrub_credentials` lives at `src/agent/loop_::scrub_credentials` and keeps a 4-character
prefix. Note the README claims tool output is scrubbed before reaching the conversation;
today `scrub_credentials` has exactly one production caller — `cron/store.rs:477`.

## Steps

1. **Route the three autosave sites through `sanitize_memory_content`,** matching how the
   channel dispatcher does it (including what it does when the sanitizer returns `Err` —
   skip the store rather than store raw).
   **Verify**: `rg -n 'autosave_memory_key' src/` — every site is preceded by sanitisation.

2. **Scrub and bound the announcement once, before both sinks.** Move the
   `scrub_credentials` + `truncate_cron_output` pair so the announcement and the history read
   from the same prepared string, rather than the announcement reading the raw one.
   **Verify**: `src/cron/scheduler.rs:472` no longer passes the raw `output`.

3. **Fix the README claim, or fix the code.** The README states tool output is credential-
   scrubbed before it reaches the conversation. Either wire `scrub_credentials` into the tool-
   result path in the agent loop, or narrow the README sentence to what is true. Pick one and
   say which in the PR body — a stale security claim is worse than no claim.
   **Verify**: whichever you pick, `rg -n 'scrub_credentials' src/` and the README agree.

4. **Tests that bite.** Store a `sk-`-shaped message through the loop's autosave and assert
   the persisted row is redacted; run a cron job whose stdout contains a credential shape and
   assert the delivered announcement is redacted.
   **Verify**: `cargo test --lib agent` and `cargo test --lib cron` pass; both tests fail if
   their fix is reverted.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib agent`, `cargo test --lib cron` pass with both new tests.
- README and code agree on what is scrubbed.

## STOP conditions

- Sanitising autosave turns out to drop legitimate content in a way that breaks recall tests →
  STOP and report; the sanitiser's behaviour is then the thing to discuss, not the wiring.

## Test plan

Two tests, each asserting the redacted marker appears and the raw secret does not. Use
placeholder credential shapes, never a real key.

## Maintenance note

Any new path that persists or transmits user or tool text needs the same treatment. The rule
is: scrub at the boundary the text crosses, not at the sink that happens to be noticed.

## Rollback

One commit across four files plus tests. No schema change.
