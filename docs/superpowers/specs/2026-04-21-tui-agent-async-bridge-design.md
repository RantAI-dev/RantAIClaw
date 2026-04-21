# TUI ↔ Agent Async Bridge & Streaming — Design

**Date:** 2026-04-21
**Status:** Design (awaiting implementation plan)
**Scope:** Unblock the Hermes-parity gap where the TUI cannot reach the Agent. Land a bridge task, an event stream, cancellation, and input queueing behind a new `Agent::turn_streaming` API. Out of scope: markdown/tool-call rendering, `/retry` persistence, `-m` single-shot CLI, per-provider streaming fills.

---

## 1. Motivation

Today, `TuiApp::submit_input` is a stub that appends `"[provider not yet wired]"` (`src/tui/app.rs:105`). The TUI holds no Agent, Memory, or Config handle. Downstream features (real LLM responses, streaming UX, spinner, `/stop`, live tool-call rendering, token usage, `/compress`, `/personality`, `-m` mode) all block on a single missing piece: a way to drive `Agent` from the TUI and observe progress as events.

This spec introduces that piece — a bridge task plus a structured event stream — and a new `Agent::turn_streaming` method that existing `Agent::turn` delegates to, so the change is additive for non-TUI callers.

---

## 2. Architecture

```
┌─────────────────┐  TurnRequest    ┌──────────────────┐   awaits   ┌──────────────┐
│                 │ ───────────────▶│                  │───────────▶│              │
│   TuiApp        │                 │  TuiAgentActor   │            │  Agent       │
│   (render loop) │◀─────────────── │  (owns Agent)    │            │  (&mut self) │
│                 │   AgentEvent    │                  │            │              │
└─────────────────┘                 └──────────────────┘            └──────────────┘
        │ poll events in                    │ cancel_current()               │ emits events
        │ TuiApp::tick()                    │ ↓                              │ ↓
        │                                   └── CancellationToken ───────────┘
```

Three components:

- **`TuiAgentActor`** — single long-lived tokio task spawned in `run_tui()`. Owns the `Agent` exclusively. Pulls `TurnRequest`s off an mpsc and awaits each turn serially. Matches `Agent::turn(&mut self)` naturally — no locks.
- **Two mpsc channels**:
  - `req_tx: mpsc::Sender<TurnRequest>` — TUI → actor.
  - `events_tx: mpsc::Sender<AgentEvent>` — actor → TUI (receiver held in `TuiContext`).
- **CancellationToken** — owned by the actor for the currently-running turn. Reset before each turn; cancelled when `TurnRequest::Cancel` arrives.

The TUI does not spawn per-turn tokio tasks and does not hold a runtime handle. All async work lives in the actor.

---

## 3. New & changed types

### 3.1 `AgentEvent` (new, `src/agent/events.rs`)

```rust
use crate::cost::TokenUsage;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Streaming text fragment from the provider. Multiple emitted per turn.
    Chunk(String),

    /// A tool call has started. Emitted once per call, before execution.
    ToolCallStart {
        id: String,
        name: String,
        args: serde_json::Value,
    },

    /// A tool call has finished. `output_preview` is truncated to ~500 chars
    /// for UI display; full output stays in the conversation history.
    ToolCallEnd {
        id: String,
        ok: bool,
        output_preview: String,
    },

    /// Usage totals for the turn. Emitted once, immediately before `Done`.
    Usage(TokenUsage),

    /// Terminal event for a turn. `cancelled=true` when `CancellationToken`
    /// fired; `final_text` is the last assistant reply (possibly partial).
    Done { final_text: String, cancelled: bool },

    /// Non-recoverable error. Followed by `Done { cancelled: false, final_text: "" }`.
    Error(String),
}
```

### 3.2 `TurnRequest` (new, `src/tui/async_bridge.rs`)

```rust
pub enum TurnRequest {
    Submit(String),
    Cancel,
}
```

### 3.3 `TurnResult` (new, `src/agent/events.rs`)

```rust
pub struct TurnResult {
    pub text: String,
    pub usage: TokenUsage,
    pub cancelled: bool,
}
```

---

## 4. Agent API change

New method; existing `turn()` preserved as a zero-event delegate.

```rust
// src/agent/agent.rs

impl Agent {
    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        self.turn_streaming(user_message, None, None)
            .await
            .map(|r| r.text)
    }

    pub async fn turn_streaming(
        &mut self,
        user_message: &str,
        events: Option<tokio::sync::mpsc::Sender<AgentEvent>>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<TurnResult> { /* ... */ }
}
```

No other callers (Telegram, Discord, Slack, gateway) change. `turn()` keeps the same signature and behavior.

Internally, `turn_streaming` calls an extended `run_tool_call_loop` that accepts a new `events: Option<mpsc::Sender<AgentEvent>>` argument. The loop emits:

- `Chunk` whenever the existing `on_delta` path would have sent text (just reshaped).
- `ToolCallStart` immediately before executing a tool call (inside the parallel tool-call batch).
- `ToolCallEnd` immediately after each call finishes, with a truncated preview.
- `Usage` after the provider returns the final non-tool response.

`Done` and `Error` are emitted by `turn_streaming` itself (not the loop), so the loop stays focused on LLM + tool mechanics.

---

## 5. Actor loop

```rust
// src/tui/async_bridge.rs

pub struct TuiAgentActor {
    agent: Agent,
    req_rx: mpsc::Receiver<TurnRequest>,
    events_tx: mpsc::Sender<AgentEvent>,
    queue: VecDeque<String>,
    current: Option<CancellationToken>,
}

impl TuiAgentActor {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                Some(req) = self.req_rx.recv() => self.handle_request(req),
                _ = self.drain_next(), if self.current.is_none() && !self.queue.is_empty() => {}
                else => break,
            }
        }
    }
}
```

**Queue semantics** (β: queue mid-stream):

- While `current.is_some()`, new `Submit` requests append to `queue`.
- When `drain_next` sees the queue non-empty and `current.is_none()`, it pops the front, sets `current = Some(new_token)`, and awaits `agent.turn_streaming(...)`.
- On turn end (normal or cancelled), `current = None`; the next loop iteration picks up the queue.

**Cancel** (α: same turn; queued submits survive):

- `Cancel` only affects the currently-running turn. `queue` is untouched.
- If nothing is running, `Cancel` is a no-op.

---

## 6. TUI integration

### 6.1 `TuiContext` additions (`src/tui/context.rs`)

```rust
pub struct TuiContext {
    // ... existing fields
    pub req_tx: mpsc::Sender<TurnRequest>,
    pub events_rx: mpsc::Receiver<AgentEvent>,
    pub queued_turns: usize,
}
```

### 6.2 `AppState` (`src/tui/app.rs`, new enum)

```rust
pub enum AppState {
    Ready,
    Streaming {
        partial: String,
        tool_blocks: Vec<ToolBlockState>,
        cancelling: bool,
    },
    Quitting,
}

pub struct ToolBlockState {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub result: Option<(bool, String)>, // (ok, preview)
}
```

### 6.3 `submit_input`

1. If `state == Ready`: send `TurnRequest::Submit(text)`, transition to `Streaming { partial: "", tool_blocks: [], cancelling: false }`.
2. If `state == Streaming`: send `TurnRequest::Submit(text)`, increment `queued_turns`, stay in `Streaming`.
3. Clear `input_buffer`.

### 6.4 `tick` (drains events each frame)

```rust
while let Ok(ev) = context.events_rx.try_recv() {
    match (ev, &mut self.state) {
        (AgentEvent::Chunk(s), AppState::Streaming { partial, .. }) => partial.push_str(&s),
        (AgentEvent::ToolCallStart { id, name, args }, AppState::Streaming { tool_blocks, .. }) =>
            tool_blocks.push(ToolBlockState { id, name, args, result: None }),
        (AgentEvent::ToolCallEnd { id, ok, output_preview }, AppState::Streaming { tool_blocks, .. }) => {
            if let Some(b) = tool_blocks.iter_mut().find(|b| b.id == id) {
                b.result = Some((ok, output_preview));
            }
        }
        (AgentEvent::Usage(u), _) => context.token_usage.add(&u),
        (AgentEvent::Done { final_text, cancelled }, _) => self.finalize_turn(final_text, cancelled),
        (AgentEvent::Error(msg), _) => self.finalize_error(msg),
        _ => {} // events while Ready (spurious late) are ignored
    }
}
```

### 6.5 Ctrl+C binding

- In `Streaming`: send `TurnRequest::Cancel`, set `cancelling = true` (footer shows "cancelling…").
- In `Ready` / `Quitting`: existing behavior (quit confirm).
- The `cancelling` flag is cleared when `Done { cancelled: true }` arrives.

### 6.6 Finalization

On `AgentEvent::Done`:

- `cancelled == false`: push `Message::assistant(final_text)` with rendered tool blocks; commit to `SessionStore`; transition to `Ready`.
- `cancelled == true`: push `Message::assistant(format!("{final_text}\n\n[cancelled]"))`; commit; transition to `Ready`.

If `queued_turns > 0` after finalize, decrement and let the actor's `drain_next` auto-start the next turn; the TUI simply stays in `Streaming` for the next event wave.

---

## 7. Wiring in `run_tui()`

```rust
// src/tui/mod.rs (or src/main.rs::run_tui)

pub async fn run_tui(config: TuiConfig) -> Result<()> {
    let agent = Agent::from_config(&load_config()?).await?;  // new public ctor
    let (req_tx, req_rx) = mpsc::channel::<TurnRequest>(16);
    let (events_tx, events_rx) = mpsc::channel::<AgentEvent>(128);

    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    tokio::spawn(actor.run());

    let mut app = TuiApp::new(config, req_tx, events_rx)?;
    app.run().await
}
```

`Agent::from_config` is a thin new constructor on Agent that bundles what `main.rs` does today for the CLI agent entrypoint. No behavior change for existing call sites.

---

## 8. Cancellation semantics (α: keep partial + marker)

1. TUI sees Ctrl+C in `Streaming`: `req_tx.send(Cancel)`, set `cancelling = true`.
2. Actor receives `Cancel`: calls `current_token.cancel()` on the active token.
3. `run_tool_call_loop` checks the token at its iteration boundary (`src/agent/loop_.rs:1209`) and returns `ToolLoopCancelled`.
4. `turn_streaming` catches `ToolLoopCancelled`, assembles `TurnResult { text: partial_buffer, usage: accumulated, cancelled: true }`, emits `Usage(accumulated)` then `Done { final_text: partial_buffer, cancelled: true }`.
5. TUI's `tick` finalizes: pushes assistant message with `[cancelled]` appended, commits to session, returns to `Ready`.

Queued submits are preserved; the next one begins immediately.

---

## 9. Error handling

- Errors from `turn_streaming` (provider error, policy rejection, unrecoverable tool failure) produce `AgentEvent::Error(msg)` followed by `AgentEvent::Done { final_text: "", cancelled: false }`.
- TUI renders errors as a single red assistant message prefixed with `⚠ `; sets `context.last_error = Some(msg)` for `/debug` to inspect.
- Any partial text buffered before the error is discarded — we prefer a clear error over a half-message followed by an error.
- Errors do not crash the actor; after emitting `Done`, the actor returns to the event loop and drains the queue.

---

## 10. Invariants & ordering guarantees

- **Every turn ends with exactly one `Done` event.** Normal, cancelled, and error paths all produce `Done` last. TUI's state machine can rely on this.
- **`Usage` precedes `Done`** when usage is known. On cancellation, `Usage` reflects whatever was accumulated (may be zero).
- **`ToolCallStart` / `ToolCallEnd` are paired by `id`.** A `ToolCallEnd` always follows its matching `Start`. Parallel tool calls may interleave Start/End pairs.
- **`Chunk`s precede any `ToolCallStart` in a single LLM response**, but a turn may contain multiple LLM responses (one per tool-call iteration), so Chunk→ToolCallStart→ToolCallEnd→Chunk→… is valid.
- **Event channel capacity is 128.** Under sustained backpressure (TUI not polling fast enough), the actor's `events_tx.send` will await; this applies natural backpressure on the provider without dropping events.

---

## 11. Testing strategy

### 11.1 Unit tests

- `src/agent/events.rs` — `AgentEvent` serde round-trip for the variants that contain JSON (`ToolCallStart::args`).
- `src/agent/agent.rs` — `turn_streaming` with a scripted provider that emits deterministic chunks and tool calls. Assert event sequence.
- `src/tui/async_bridge.rs` — actor unit tests with an in-memory mpsc pair: submit + expect `Done`; cancel mid-chunk + expect `Done { cancelled: true }`; queue two turns + expect ordered processing.

### 11.2 Integration test

- `tests/tui_agent_bridge.rs` — wire a `TuiAgentActor` with a `ScriptedProvider` (already used in `src/agent/loop_.rs` tests), send TurnRequests via real mpsc, drain events, assert ordering and cancellation behaviour end-to-end without launching a terminal.

### 11.3 Non-goals for testing

- Real terminal I/O (ratatui rendering) — covered separately in the render-subsystem spec.
- Real provider streams (OpenAI, etc.) — provider tests already cover chunking.

---

## 12. Migration & rollout

- Additive-only for everything outside `src/tui/` and `src/agent/`. Telegram/Discord/Slack/gateway continue using `Agent::turn`.
- The new event channel types live under `src/agent/events.rs` and are re-exported at `crate::agent::events::*` so the `src/tui/` side can depend on them without pulling in Agent internals.
- No config schema changes.
- No runtime-contract reference doc changes (no new config keys, no CLI surface change in this spec).

---

## 13. Out of scope (explicit)

- **Markdown rendering** of chunks — separate spec under "Render subsystem".
- **Tool-call block rendering** beyond the raw `ToolBlockState` struct — this spec defines the data; visual treatment belongs in the render-subsystem spec.
- **`/retry` / `/undo` persistence** — requires `SessionStore::delete_message`; separate spec.
- **`-m` single-shot CLI mode** — will reuse `turn_streaming` but has its own CLI wiring; separate spec.
- **Per-provider streaming gaps** — providers without `stream_chat_*` impls still work via the buffering fallback in `run_tool_call_loop`.
- **`/compress` / `/personality` / `/memory` / `/cron` / `/skills` command wiring** — each depends on a distinct subsystem API; separate specs per gap-list category.

---

## 14. Risks

- **`Agent` construction from TUI**: `Agent::from_config` does not exist yet. Introducing it is a small refactor of `main.rs`'s agent setup path; needs care to preserve existing CLI behavior. Mitigation: land `Agent::from_config` first as a pure refactor (no behavior change), verify CLI still works, then wire TUI.
- **`run_tool_call_loop` signature growth**: adding an `events` param pushes the arg count to 15. Mitigation: introduce a `ToolLoopArgs` struct in the same change if count exceeds 16, or accept the wide signature for this spec and refactor separately.
- **Event channel overflow** under a very chatty tool loop: 128-slot buffer should be generous, but a `debug_assert!` log on `send` timeouts >100 ms will surface real problems. Not a hard failure.
- **Partial text on cancel**: the partial buffer lives in `turn_streaming`'s stack, not in history until `Done` fires. If the runtime panics mid-turn, the partial is lost. Acceptable — panics are not a normal cancel path.

---

## 15. Approval state

Approved for implementation. Next step: invoke `superpowers:writing-plans` to decompose this design into an implementation plan.
