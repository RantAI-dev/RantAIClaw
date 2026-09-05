# Plan 010: Stop the streaming fallback from re-emitting the full response after partial tokens

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/providers/reliable.rs`
> If the file changed since this plan was written, compare the "Current state"
> excerpt against the live code; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

`ReliableProvider::chat_stream` delegates to the primary provider's
`chat_stream` (which sends SSE deltas token-by-token via `text_tx`). If that call
returns `Err` **after** already sending some deltas (a mid-stream network/provider
error), the code falls back to non-streaming `chat()` and sends the **entire**
response text as one more chunk. The consumer (agent loop → `AgentEvent::Chunk`)
has already displayed the partial text, then receives the full text again →
duplicated/garbled live output in the TUI/channel draft. History persistence uses
the final response, so only the live display is corrupted — but it's visibly
wrong to the user on any mid-stream failure.

## Current state

- `src/providers/reliable.rs:656-702` — `chat_stream` (verified):
  ```rust
  /// ... "if the streaming call fails, we fall back to the non-streaming
  /// `chat()` ... and emit its result as a single chunk." ...
  async fn chat_stream(&self, request: ChatRequest<'_>, model: &str, temperature: f64,
      text_tx: tokio::sync::mpsc::Sender<String>) -> anyhow::Result<ChatResponse> {
      if let Some((_, provider)) = self.providers.first() {
          if provider.supports_streaming() {
              let req = ChatRequest { messages: request.messages, tools: request.tools };
              match provider.chat_stream(req, model, temperature, text_tx.clone()).await {
                  Ok(resp) => return Ok(resp),
                  Err(e) => { tracing::warn!(error = %e, "chat_stream failed, falling back to non-streaming chat"); }
              }
          }
      }
      // Fallback: non-streaming chat with full retry/fallback machinery.
      let req = ChatRequest { messages: request.messages, tools: request.tools };
      let response = self.chat(req, model, temperature).await?;
      if let Some(text) = response.text.as_deref() {
          if !text.is_empty() {
              let _ = text_tx.send(text.to_string()).await;   // line 698: re-emits full text
          }
      }
      Ok(response)
  }
  ```
  The comment (656-662) assumes nothing was previously emitted — which the
  partial-then-fail case violates.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Provider tests | `cargo test reliable` | all pass, incl. new |

## Scope

**In scope**:
- `src/providers/reliable.rs` — `chat_stream` only.
- New test in the same file's `#[cfg(test)]` module.

**Out of scope** (do NOT touch):
- `chat()` and its retry/fallback machinery.
- The individual providers' `chat_stream`.
- `stream_chat_with_system` (a separate, currently-unused path).

## Git workflow

- Branch: `advisor/010-stream-fallback-double-emit`
- One commit; message e.g.
  `fix(providers): don't re-emit full text on stream fallback after partial deltas`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Track whether the primary streamed anything

Wrap `text_tx` so the fallback can tell whether any delta was forwarded. The
simplest KISS approach: an `Arc<AtomicBool>` (or `Arc<AtomicUsize>` counting
bytes) that a small forwarding task sets when the primary sends its first chunk.

Because `text_tx` is an mpsc `Sender<String>` handed to the primary, insert a
relay: create an intermediate channel, spawn a task that forwards from the
intermediate to the real `text_tx` and flips an `AtomicBool emitted = true` on
the first message, and pass the intermediate sender to the primary's
`chat_stream`. On fallback, read `emitted`.

Target shape:
```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

let emitted = Arc::new(AtomicBool::new(false));
if let Some((_, provider)) = self.providers.first() {
    if provider.supports_streaming() {
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::channel::<String>(64);
        let real_tx = text_tx.clone();
        let emitted_c = emitted.clone();
        let pump = tokio::spawn(async move {
            while let Some(s) = relay_rx.recv().await {
                emitted_c.store(true, Ordering::SeqCst);
                if real_tx.send(s).await.is_err() { break; }
            }
        });
        let req = ChatRequest { messages: request.messages, tools: request.tools };
        let result = provider.chat_stream(req, model, temperature, relay_tx).await;
        // dropping relay_tx (moved into chat_stream) ends the pump
        let _ = pump.await;
        match result {
            Ok(resp) => return Ok(resp),
            Err(e) => { tracing::warn!(error = %e, "chat_stream failed, falling back to non-streaming chat"); }
        }
    }
}
```
(Adjust ownership so `relay_tx` is fully dropped after the primary returns, so
`pump` terminates. If `chat_stream` takes the sender by value this is automatic.)

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 2: Suppress the re-emit when partial output already went out

In the fallback block, only send the full text if nothing was already emitted:
```rust
let response = self.chat(req, model, temperature).await?;
if !emitted.load(Ordering::SeqCst) {
    if let Some(text) = response.text.as_deref() {
        if !text.is_empty() {
            let _ = text_tx.send(text.to_string()).await;
        }
    }
}
Ok(response)
```
The final `ChatResponse` is still returned in full (history/consumer uses it);
only the *live re-emit* is suppressed when it would duplicate already-shown text.

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

## Test plan

- New unit test in `src/providers/reliable.rs` `#[cfg(test)]`. Use mock providers
  (there are almost certainly existing mock `Provider` impls in the test module —
  `grep -n "impl Provider for\|struct Mock\|#\[cfg(test)\]" src/providers/reliable.rs`):
  1. `fallback_after_partial_does_not_reemit`: a mock primary that sends one
     delta ("Hello") then returns `Err`; a fallback `chat()` returning
     "Hello world". Collect everything received on `text_tx`; assert the receiver
     did NOT get the full "Hello world" appended after "Hello" (i.e. no
     duplication), while the returned `ChatResponse.text` is still complete.
  2. `fallback_with_no_partial_still_emits`: a mock primary that returns `Err`
     immediately (no deltas); assert the fallback full text IS emitted once.
  3. `happy_path_streams_through`: primary succeeds; deltas pass through, no
     fallback emit.
- Verification: `cargo test reliable` → all pass including the three tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test reliable` passes; the partial-then-fail no-reemit test exists
- [ ] Only `src/providers/reliable.rs` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `chat_stream` does not match the excerpt (drift).
- There are no mock `Provider` impls in the test module and building one is
  disproportionate — report so the test strategy can be decided (do not add a
  network-dependent test).
- The relay-channel change risks reordering or dropping deltas on the happy path
  — if so, prefer a simpler design (e.g. a wrapper `Sender` type) and report.

## Maintenance notes

- The `AtomicUsize` byte-count variant would additionally let a future
  improvement emit only the *un-emitted suffix* on fallback instead of
  suppressing entirely — noted, not built here (suppression is correct and
  simpler).
- Reviewer should confirm the pump task always terminates (no leaked task) and
  that the happy path has no added latency from the relay hop.
