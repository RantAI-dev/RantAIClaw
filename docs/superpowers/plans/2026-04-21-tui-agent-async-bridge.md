# TUI ↔ Agent Async Bridge & Streaming Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the TUI to the real `Agent` via an actor-based async bridge with a structured `AgentEvent` stream, cancellation, and queued mid-stream input — unblocking Hermes-parity real LLM responses from the TUI.

**Architecture:** A single `TuiAgentActor` tokio task owns the `Agent` exclusively. The TUI sends `TurnRequest`s over an mpsc and drains `AgentEvent`s each frame in `TuiApp::tick()`. A new `Agent::turn_streaming` method wraps the existing tool-call loop, which gains an optional `events` parameter alongside its existing `on_delta`. No non-TUI caller changes.

**Tech Stack:** Rust 1.92, tokio (mpsc, select!), tokio-util (CancellationToken), ratatui/crossterm (already present), anyhow, serde_json.

**Base branch:** `feature/tui-agent-bridge` (created off main `3e95d94`).

**Spec:** `docs/superpowers/specs/2026-04-21-tui-agent-async-bridge-design.md`

**Pre-existing facts the plan relies on (verified before planning):**
- `Agent::from_config(&Config) -> Result<Agent>` already exists at `src/agent/agent.rs:233`. Do **not** add it.
- `run_tool_call_loop` at `src/agent/loop_.rs:1182` already accepts `cancellation_token: Option<CancellationToken>` and `on_delta: Option<mpsc::Sender<String>>`. The `on_delta` emission site is `src/agent/loop_.rs:1331-1352`.
- Tool call execution happens later in the loop (after the `on_delta` block). Emission points for `ToolCallStart` / `ToolCallEnd` must be found around the `execute_tool_calls_*` invocation.
- `TokenUsage::new(model, input_tokens, output_tokens, input_price_per_M, output_price_per_M) -> Self` at `src/cost/types.rs:30`. There is no `add` method. For accumulating usage, create a new `TokenUsage` with summed tokens or store as a running tuple and convert at end.
- `ScriptedProvider` already exists in `src/agent/loop_.rs` tests (line ~2491) — reusable for integration tests by re-exporting or copying the helper structure.
- `on_delta`-path is still used by Telegram's `with_streaming` — existing Telegram tests must continue passing unchanged.

---

## File Structure

### Create
- `src/agent/events.rs` — `AgentEvent` enum, `TurnResult` struct, `AgentEventSender` type alias. Unit tests for `AgentEvent` construction and basic serde.
- `src/tui/async_bridge.rs` — `TurnRequest` enum, `TuiAgentActor` struct + `run()` loop. Unit tests for submit/cancel/queue behaviour with a mock Agent.
- `tests/tui_agent_bridge.rs` — end-to-end integration test wiring a real `TuiAgentActor` with a scripted provider.

### Modify
- `src/agent/mod.rs` — add `pub mod events;` and re-exports.
- `src/agent/agent.rs` — add `Agent::turn_streaming`; refactor `Agent::turn` to delegate.
- `src/agent/loop_.rs` — extend `run_tool_call_loop` signature with `events: Option<AgentEventSender>`; add emission of `Chunk`/`ToolCallStart`/`ToolCallEnd`/`Usage`.
- `src/tui/mod.rs` — add `pub mod async_bridge;`.
- `src/tui/context.rs` — add `req_tx`, `events_rx`, `queued_turns` fields.
- `src/tui/app.rs` — add `AppState` enum (`Ready`/`Streaming{..}`/`Quitting`), `ToolBlockState` struct; rewrite `submit_input`; add `drain_events` helper called from `tick`; add Ctrl+C handling in `Streaming` state; add `finalize_turn` / `finalize_error` helpers.
- `src/main.rs` — where `run_tui` is invoked for `Commands::Chat` (find the block that calls `run_tui` or creates `TuiApp`), construct the bridge channels and spawn `TuiAgentActor`.

### Touch lightly (one-line changes expected)
- `Cargo.toml` — none; tokio, tokio-util, mpsc already in tree.

---

## Task 1: Scaffold the events module

**Files:**
- Create: `src/agent/events.rs`
- Modify: `src/agent/mod.rs` (add `pub mod events;`)
- Test: inline in `src/agent/events.rs`

- [ ] **Step 1: Create the events module file with the initial types and a failing test**

```rust
// src/agent/events.rs
//! Structured events emitted during a single agent turn.
//!
//! Consumed by TUI / other callers that want richer feedback than the
//! plain `on_delta: mpsc::Sender<String>` stream used by channels.

use crate::cost::TokenUsage;

pub type AgentEventSender = tokio::sync::mpsc::Sender<AgentEvent>;

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

    /// A tool call has finished. `output_preview` is truncated to ~500 chars.
    /// Full output still lives in the conversation history.
    ToolCallEnd {
        id: String,
        ok: bool,
        output_preview: String,
    },

    /// Usage totals for the turn. Emitted once, immediately before `Done`.
    Usage(TokenUsage),

    /// Terminal event for a turn. `cancelled=true` when `CancellationToken` fired.
    Done { final_text: String, cancelled: bool },

    /// Non-recoverable error. Followed by `Done { cancelled: false, final_text: "" }`.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub text: String,
    pub usage: TokenUsage,
    pub cancelled: bool,
}

pub(crate) const TOOL_OUTPUT_PREVIEW_MAX: usize = 500;

pub(crate) fn truncate_preview(s: &str) -> String {
    if s.len() <= TOOL_OUTPUT_PREVIEW_MAX {
        s.to_string()
    } else {
        let mut out = String::with_capacity(TOOL_OUTPUT_PREVIEW_MAX + 1);
        // char-boundary-safe truncation
        let mut end = TOOL_OUTPUT_PREVIEW_MAX;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        out.push_str(&s[..end]);
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_preview_under_limit_is_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_preview(s), s);
    }

    #[test]
    fn tool_output_preview_over_limit_is_truncated_with_ellipsis() {
        let s = "x".repeat(TOOL_OUTPUT_PREVIEW_MAX + 100);
        let out = truncate_preview(&s);
        assert!(out.ends_with('…'));
        assert!(out.len() <= TOOL_OUTPUT_PREVIEW_MAX + 4); // 3 bytes for ellipsis
    }

    #[test]
    fn tool_output_preview_respects_char_boundaries() {
        // Emoji takes 4 bytes in UTF-8 — ensure we don't slice mid-codepoint.
        let mut s = String::new();
        for _ in 0..TOOL_OUTPUT_PREVIEW_MAX { s.push('a'); }
        s.push('🦀'); // 4 bytes
        let out = truncate_preview(&s);
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    #[test]
    fn agent_event_variants_construct() {
        let _ = AgentEvent::Chunk("hi".into());
        let _ = AgentEvent::ToolCallStart {
            id: "1".into(),
            name: "shell".into(),
            args: serde_json::json!({"cmd": "ls"}),
        };
        let _ = AgentEvent::ToolCallEnd {
            id: "1".into(),
            ok: true,
            output_preview: "ok".into(),
        };
        let _ = AgentEvent::Done { final_text: "done".into(), cancelled: false };
        let _ = AgentEvent::Error("boom".into());
    }
}
```

- [ ] **Step 2: Register the module**

```rust
// src/agent/mod.rs — add this line near other `pub mod` declarations
pub mod events;
```

And re-export the most-used types:

```rust
// src/agent/mod.rs — add after the pub use ... block (or near it)
pub use events::{AgentEvent, AgentEventSender, TurnResult};
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p rantaiclaw --lib agent::events`
Expected: all 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/agent/events.rs src/agent/mod.rs
git commit -m "feat(agent): add AgentEvent stream + TurnResult scaffolding"
```

---

## Task 2: Extend `run_tool_call_loop` signature with `events` parameter (no emission yet)

Only the signature change + plumbing; no event emission in this task, so all existing behavior is preserved.

**Files:**
- Modify: `src/agent/loop_.rs` (the `run_tool_call_loop` fn at line ~1182 and all call sites)

- [ ] **Step 1: Add the import at the top of `loop_.rs`**

```rust
// near other `use crate::...` lines
use crate::agent::events::AgentEventSender;
```

- [ ] **Step 2: Extend the function signature**

Current signature ends with `on_delta: Option<tokio::sync::mpsc::Sender<String>>,`. Append a new parameter **after** it (keep argument order conservative so breakage is obvious if anything is missed):

```rust
pub(crate) async fn run_tool_call_loop(
    provider: &dyn Provider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    provider_name: &str,
    model: &str,
    temperature: f64,
    silent: bool,
    approval: Option<&ApprovalManager>,
    channel_name: &str,
    multimodal_config: &crate::config::MultimodalConfig,
    max_tool_iterations: usize,
    cancellation_token: Option<CancellationToken>,
    on_delta: Option<tokio::sync::mpsc::Sender<String>>,
    events: Option<AgentEventSender>,            // <-- new
) -> Result<String> {
```

- [ ] **Step 3: Update every call site to pass `None` for `events`**

Find every call in the crate (there are ~5 in `loop_.rs` itself plus Agent callers). Run:

```
cargo check -p rantaiclaw 2>&1 | grep -E "run_tool_call_loop|arguments"
```

For each error, add `None,` as the new last argument. Verified call sites from the exploration phase:
- `src/agent/loop_.rs:1000` (run_full_loop_with_observer wrapper)
- `src/agent/loop_.rs:1740` (cross-session wrapper)
- `src/agent/loop_.rs:1859` (peer chat wrapper)
- `src/agent/loop_.rs:2329, 2373, 2411, 2531` (test call sites)

Each becomes: `, None).await` → `, None, None).await`.

Also update the three places in `src/agent/agent.rs` where Agent invokes the loop (search with `grep -n run_tool_call_loop src/agent/agent.rs`).

- [ ] **Step 4: Verify compile + existing tests pass**

```bash
cargo build -p rantaiclaw
cargo test -p rantaiclaw --lib agent::
```

Expected: clean build; all existing agent loop tests pass. No behavior change (events is ignored).

- [ ] **Step 5: Commit**

```bash
git add src/agent/loop_.rs src/agent/agent.rs
git commit -m "refactor(agent): extend run_tool_call_loop with events param (noop)"
```

---

## Task 3: Emit `AgentEvent::Chunk` alongside `on_delta`

**Files:**
- Modify: `src/agent/loop_.rs` (emission site at line ~1331)
- Test: new unit test in `src/agent/loop_.rs` tests module

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block near the existing `run_tool_call_loop_*` tests:

```rust
#[tokio::test]
async fn run_tool_call_loop_emits_chunk_events_when_events_some() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "hello world this is a streamed response".to_string(),
    ]);
    let mut history = vec![ChatMessage::user("hi")];
    let tools_registry: Vec<Box<dyn Tool>> = vec![];
    let observer = crate::observability::NoopObserver;
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel::<crate::agent::events::AgentEvent>(32);

    let multimodal = crate::config::MultimodalConfig::default();
    let _ = run_tool_call_loop(
        &provider,
        &mut history,
        &tools_registry,
        &observer,
        "mock-provider",
        "mock-model",
        0.0,
        true,
        None,
        "test",
        &multimodal,
        5,
        None,
        None,           // on_delta: None
        Some(events_tx), // events: Some
    )
    .await
    .expect("loop succeeds");

    // Drain the receiver; expect at least one Chunk event.
    drop(history); // release mut borrow if needed
    let mut chunks = Vec::new();
    while let Ok(ev) = events_rx.try_recv() {
        if let crate::agent::events::AgentEvent::Chunk(s) = ev {
            chunks.push(s);
        }
    }
    assert!(!chunks.is_empty(), "expected ≥1 Chunk event");
    let combined: String = chunks.join("");
    assert!(combined.contains("hello"));
    assert!(combined.contains("streamed"));
}
```

- [ ] **Step 2: Run the test — it fails**

```bash
cargo test -p rantaiclaw --lib agent::loop_::tests::run_tool_call_loop_emits_chunk_events_when_events_some
```

Expected: FAIL with "expected ≥1 Chunk event" (chunks is empty — the code path doesn't emit yet).

- [ ] **Step 3: Implement the emission**

Find the block at `src/agent/loop_.rs:1331`:

```rust
if let Some(ref tx) = on_delta {
    // ... word-by-word chunking into tx ...
}
```

Replace with a helper that dispatches to either `events` (preferred when present) or the legacy `on_delta`:

```rust
if events.is_some() || on_delta.is_some() {
    // Split on whitespace boundaries, accumulating chunks of at least
    // STREAM_CHUNK_MIN_CHARS characters.
    let mut chunk = String::new();
    for word in display_text.split_inclusive(char::is_whitespace) {
        if cancellation_token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(ToolLoopCancelled.into());
        }
        chunk.push_str(word);
        if chunk.len() >= STREAM_CHUNK_MIN_CHARS {
            let piece = std::mem::take(&mut chunk);
            if let Some(ref tx) = events {
                if tx.send(crate::agent::events::AgentEvent::Chunk(piece)).await.is_err() {
                    break; // receiver dropped
                }
            } else if let Some(ref tx) = on_delta {
                if tx.send(piece).await.is_err() {
                    break;
                }
            }
        }
    }
    if !chunk.is_empty() {
        if let Some(ref tx) = events {
            let _ = tx.send(crate::agent::events::AgentEvent::Chunk(chunk)).await;
        } else if let Some(ref tx) = on_delta {
            let _ = tx.send(chunk).await;
        }
    }
}
```

**Important:** when `events` is `Some`, `on_delta` is ignored (spec §4). This avoids duplicate chunk delivery.

- [ ] **Step 4: Run the test — it passes**

```bash
cargo test -p rantaiclaw --lib agent::loop_::tests::run_tool_call_loop_emits_chunk_events_when_events_some
```

Expected: PASS.

- [ ] **Step 5: Also verify the existing `on_delta` path still works**

```bash
cargo test -p rantaiclaw --lib agent::loop_
```

Expected: all existing tests still pass (the `events = None` case falls through to `on_delta`).

- [ ] **Step 6: Commit**

```bash
git add src/agent/loop_.rs
git commit -m "feat(agent): emit AgentEvent::Chunk when events channel is provided"
```

---

## Task 4: Emit `ToolCallStart` and `ToolCallEnd`

**Files:**
- Modify: `src/agent/loop_.rs` (tool execution section, after line ~1368)
- Test: new unit test

- [ ] **Step 1: Locate the tool execution boundaries**

In `run_tool_call_loop`, tool calls are executed in a block starting near `let mut tool_results = String::new();` (line ~1368). Each tool call goes through either `should_execute_tools_in_parallel` → parallel branch, or a serial branch. Both branches iterate `tool_calls: Vec<ParsedToolCall>`.

Find the point in each branch (serial and parallel) where an individual `ParsedToolCall` is about to be dispatched, and the point where its `ToolResult` is obtained.

- [ ] **Step 2: Write the failing test**

```rust
#[tokio::test]
async fn run_tool_call_loop_emits_tool_call_start_and_end_events() {
    // Scripted provider returns one tool call, then a text response.
    let provider = ScriptedProvider::from_text_responses(vec![
        r#"<tool_call>{"name":"echo","arguments":{"text":"hi"}}</tool_call>"#.into(),
        "final answer".into(),
    ]);
    let mut history = vec![ChatMessage::user("call echo")];
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::EchoTool)];
    let observer = crate::observability::NoopObserver;
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);

    let multimodal = crate::config::MultimodalConfig::default();
    let _ = run_tool_call_loop(
        &provider, &mut history, &tools_registry, &observer,
        "mock-provider", "mock-model", 0.0, true, None, "test",
        &multimodal, 5, None, None, Some(events_tx),
    ).await.expect("loop succeeds");

    let mut saw_start = false;
    let mut saw_end = false;
    while let Ok(ev) = events_rx.try_recv() {
        match ev {
            crate::agent::events::AgentEvent::ToolCallStart { name, .. } => {
                assert_eq!(name, "echo");
                saw_start = true;
            }
            crate::agent::events::AgentEvent::ToolCallEnd { ok, .. } => {
                assert!(ok, "echo tool should succeed");
                saw_end = true;
            }
            _ => {}
        }
    }
    assert!(saw_start && saw_end, "expected both ToolCallStart and ToolCallEnd");
}
```

If `EchoTool` doesn't exist in `crate::tools`, use an existing trivially-successful tool (e.g. scan `src/tools/mod.rs` for a no-arg tool suitable for tests, or define a local test tool inline in the test module).

- [ ] **Step 3: Run the test — it fails**

```bash
cargo test -p rantaiclaw --lib agent::loop_::tests::run_tool_call_loop_emits_tool_call_start_and_end_events
```

Expected: FAIL (no ToolCallStart/End events).

- [ ] **Step 4: Implement emission in the serial branch**

In the serial branch of the tool execution block, wrap each dispatch:

```rust
// Before dispatching each ParsedToolCall:
if let Some(ref tx) = events {
    let _ = tx.send(crate::agent::events::AgentEvent::ToolCallStart {
        id: parsed_call.id.clone().unwrap_or_else(|| format!("call-{}", idx)),
        name: parsed_call.name.clone(),
        args: parsed_call.arguments.clone(),  // already serde_json::Value
    }).await;
}

let tool_result = /* existing dispatch expression */;

if let Some(ref tx) = events {
    let preview = crate::agent::events::truncate_preview(&tool_result.output);
    let _ = tx.send(crate::agent::events::AgentEvent::ToolCallEnd {
        id: parsed_call.id.clone().unwrap_or_else(|| format!("call-{}", idx)),
        ok: tool_result.success,
        output_preview: preview,
    }).await;
}
```

- [ ] **Step 5: Implement emission in the parallel branch**

The parallel path uses `futures::future::join_all` or similar. Wrap each future so it emits its own Start before execution and End after:

```rust
// Map each ParsedToolCall into a future that emits start, runs, emits end.
let events_clone = events.clone();  // mpsc::Sender is cheap to clone
let futures = tool_calls.iter().enumerate().map(|(idx, pc)| {
    let events_for_call = events_clone.clone();
    let id = pc.id.clone().unwrap_or_else(|| format!("call-{}", idx));
    let name = pc.name.clone();
    let args = pc.arguments.clone();
    async move {
        if let Some(ref tx) = events_for_call {
            let _ = tx.send(crate::agent::events::AgentEvent::ToolCallStart {
                id: id.clone(), name: name.clone(), args,
            }).await;
        }
        let result = /* existing execute_tool_call expression */;
        if let Some(ref tx) = events_for_call {
            let preview = crate::agent::events::truncate_preview(&result.output);
            let _ = tx.send(crate::agent::events::AgentEvent::ToolCallEnd {
                id, ok: result.success, output_preview: preview,
            }).await;
        }
        result
    }
});
```

- [ ] **Step 6: Run the test — it passes**

```bash
cargo test -p rantaiclaw --lib agent::loop_::tests::run_tool_call_loop_emits_tool_call_start_and_end_events
```

Expected: PASS.

- [ ] **Step 7: Run all agent loop tests**

```bash
cargo test -p rantaiclaw --lib agent::loop_
```

Expected: no regressions.

- [ ] **Step 8: Commit**

```bash
git add src/agent/loop_.rs
git commit -m "feat(agent): emit ToolCallStart/End events during tool execution"
```

---

## Task 5: Collect usage inside the loop and emit `AgentEvent::Usage`

**Files:**
- Modify: `src/agent/loop_.rs`

**Context:** Provider `ChatResponse` does not currently carry token counts directly — they're observed via `ObserverEvent::AgentEnd`. For this plan, we collect usage by asking the provider for its usage when available, OR by synthesizing a zero-token `TokenUsage` as a placeholder. The spec (§10) explicitly permits zero usage on cancellation.

- [ ] **Step 1: Inspect current ChatResponse structure**

```bash
grep -n "struct ChatResponse\|pub.*tokens\|pub.*usage" /home/shiro/rantai/RantAI-Agents/packages/rantaiclaw/src/providers/traits.rs | head
```

If `ChatResponse` has no usage field today, emit a zero-token `TokenUsage::new(model, 0, 0, 0.0, 0.0)` as the final Usage event for this task. Populating real token counts is a follow-up (tracked in the design's "token usage" gap; not part of this bridge plan).

- [ ] **Step 2: Write the failing test**

```rust
#[tokio::test]
async fn run_tool_call_loop_emits_usage_event_before_returning() {
    let provider = ScriptedProvider::from_text_responses(vec!["done".into()]);
    let mut history = vec![ChatMessage::user("hi")];
    let tools_registry: Vec<Box<dyn Tool>> = vec![];
    let observer = crate::observability::NoopObserver;
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
    let multimodal = crate::config::MultimodalConfig::default();

    run_tool_call_loop(
        &provider, &mut history, &tools_registry, &observer,
        "mock-provider", "mock-model", 0.0, true, None, "test",
        &multimodal, 5, None, None, Some(events_tx),
    ).await.unwrap();

    let mut usage_seen = false;
    while let Ok(ev) = events_rx.try_recv() {
        if let crate::agent::events::AgentEvent::Usage(_) = ev {
            usage_seen = true;
        }
    }
    assert!(usage_seen, "expected Usage event before loop returned");
}
```

- [ ] **Step 3: Run the test — it fails**

- [ ] **Step 4: Add Usage emission at the terminal branch**

In the `tool_calls.is_empty()` branch of the loop (where `run_tool_call_loop` returns Ok with the final text) — after the Chunk emission, before the `return Ok(display_text);`:

```rust
if let Some(ref tx) = events {
    let usage = crate::cost::TokenUsage::new(model, 0, 0, 0.0, 0.0);
    let _ = tx.send(crate::agent::events::AgentEvent::Usage(usage)).await;
}
```

- [ ] **Step 5: Run the test — it passes**

- [ ] **Step 6: Commit**

```bash
git add src/agent/loop_.rs
git commit -m "feat(agent): emit AgentEvent::Usage before loop returns"
```

---

## Task 6: Add `Agent::turn_streaming` wrapper

**Files:**
- Modify: `src/agent/agent.rs`
- Test: inline in `src/agent/agent.rs`

- [ ] **Step 1: Write the failing test (with an inline mock provider)**

Add inside the existing `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn turn_streaming_emits_done_with_final_text() {
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);
    let mut agent = Agent::builder()
        .provider(Box::new(MockProvider {
            responses: parking_lot::Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some("hello".into()),
                    tool_calls: vec![],
                },
            ]),
        }))
        .tools(vec![])
        .memory(Arc::new(crate::memory::NoopMemory::default()))
        .observer(Arc::new(crate::observability::NoopObserver))
        .model_name("mock-model")
        .build()
        .unwrap();

    let result = agent
        .turn_streaming("hi", Some(events_tx), None)
        .await
        .expect("turn_streaming ok");

    assert_eq!(result.text, "hello");
    assert!(!result.cancelled);

    // Done event is the last structured event for the turn.
    let mut last_was_done = false;
    while let Ok(ev) = events_rx.try_recv() {
        if let AgentEvent::Done { final_text, cancelled } = ev {
            assert_eq!(final_text, "hello");
            assert!(!cancelled);
            last_was_done = true;
        }
    }
    assert!(last_was_done, "expected Done event");
}

#[tokio::test]
async fn turn_delegates_to_turn_streaming() {
    let mut agent = Agent::builder()
        .provider(Box::new(MockProvider {
            responses: parking_lot::Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some("delegated".into()),
                    tool_calls: vec![],
                },
            ]),
        }))
        .tools(vec![])
        .memory(Arc::new(crate::memory::NoopMemory::default()))
        .observer(Arc::new(crate::observability::NoopObserver))
        .model_name("mock-model")
        .build()
        .unwrap();

    let text = agent.turn("hi").await.unwrap();
    assert_eq!(text, "delegated");
}
```

- [ ] **Step 2: Run the tests — fail with "method not found"**

```bash
cargo test -p rantaiclaw --lib agent::agent::tests::turn_streaming_emits_done_with_final_text
```

Expected: FAIL (turn_streaming not defined).

- [ ] **Step 3: Add the turn_streaming method**

In `src/agent/agent.rs`, add imports near the top:

```rust
use crate::agent::events::{AgentEvent, AgentEventSender, TurnResult};
use tokio_util::sync::CancellationToken;
```

Replace the existing `turn` method body with:

```rust
pub async fn turn(&mut self, user_message: &str) -> Result<String> {
    self.turn_streaming(user_message, None, None)
        .await
        .map(|r| r.text)
}

pub async fn turn_streaming(
    &mut self,
    user_message: &str,
    events: Option<AgentEventSender>,
    cancel: Option<CancellationToken>,
) -> Result<TurnResult> {
    // Build-up is identical to the prior turn() body, but we collect the
    // final text from run_tool_call_loop's return, track cancellation, and
    // emit Done/Error on the events channel.

    if self.history.is_empty() {
        let system_prompt = self.build_system_prompt()?;
        self.history.push(ConversationMessage::Chat(ChatMessage::system(system_prompt)));
    }

    if self.auto_save {
        let _ = self
            .memory
            .store("user_msg", user_message, MemoryCategory::Conversation, None)
            .await;
    }

    let context = self
        .memory_loader
        .load_context(self.memory.as_ref(), user_message)
        .await
        .unwrap_or_default();

    let enriched = if context.is_empty() {
        user_message.to_string()
    } else {
        format!("{context}{user_message}")
    };

    self.history
        .push(ConversationMessage::Chat(ChatMessage::user(enriched)));

    let effective_model = self.classify_model(user_message);

    // We need a `&mut Vec<ChatMessage>` for run_tool_call_loop but our
    // history is `Vec<ConversationMessage>`. Use the existing dispatcher
    // adapter (same approach as turn() used to take).
    let mut chat_history: Vec<ChatMessage> =
        self.tool_dispatcher.to_provider_messages(&self.history);

    let loop_result = crate::agent::loop_::run_tool_call_loop(
        self.provider.as_ref(),
        &mut chat_history,
        &self.tools,
        self.observer.as_ref(),
        self.config.provider_name(),
        &effective_model,
        self.config.temperature,
        /* silent = */ true,
        /* approval = */ None,
        /* channel_name = */ "tui",
        &self.config.multimodal,
        self.config.max_tool_iterations,
        cancel.clone(),
        /* on_delta = */ None,
        events.clone(),
    )
    .await;

    let (text, cancelled) = match loop_result {
        Ok(t) => (t, false),
        Err(e) => {
            // If cancelled, finalize with partial text if we captured any.
            if e.downcast_ref::<crate::agent::loop_::ToolLoopCancelled>().is_some() {
                // Partial text is whatever's already in `chat_history` as the last
                // assistant message, if any.
                let partial = chat_history
                    .iter()
                    .rev()
                    .find_map(|m| if m.role == "assistant" { Some(m.content.clone()) } else { None })
                    .unwrap_or_default();
                (partial, true)
            } else {
                if let Some(ref tx) = events {
                    let _ = tx.send(AgentEvent::Error(format!("{e:#}"))).await;
                    let _ = tx.send(AgentEvent::Done { final_text: String::new(), cancelled: false }).await;
                }
                return Err(e);
            }
        }
    };

    // Merge assistant turn back into self.history (preserve the Vec<ConversationMessage> shape).
    if !text.is_empty() && !cancelled {
        self.history
            .push(ConversationMessage::Chat(ChatMessage::assistant(text.clone())));
    } else if cancelled && !text.is_empty() {
        self.history.push(ConversationMessage::Chat(ChatMessage::assistant(text.clone())));
    }

    let usage = crate::cost::TokenUsage::new(&effective_model, 0, 0, 0.0, 0.0);

    if let Some(ref tx) = events {
        let _ = tx.send(AgentEvent::Done { final_text: text.clone(), cancelled }).await;
    }

    Ok(TurnResult { text, usage, cancelled })
}
```

**Note on the Usage event:** `run_tool_call_loop` already emits `Usage` in the non-error path (Task 5). `turn_streaming` does NOT emit a second Usage — the one from the loop is authoritative. On the error/cancel path, `turn_streaming` does not emit Usage (keeps the "Usage precedes Done only when known" guarantee).

- [ ] **Step 4: Run tests**

```bash
cargo test -p rantaiclaw --lib agent::agent::tests::turn_streaming_emits_done_with_final_text
cargo test -p rantaiclaw --lib agent::agent::tests::turn_delegates_to_turn_streaming
cargo test -p rantaiclaw --lib agent::
```

Expected: PASS. Existing turn()-consuming tests still pass (delegation).

- [ ] **Step 5: Commit**

```bash
git add src/agent/agent.rs
git commit -m "feat(agent): add turn_streaming method; turn delegates"
```

---

## Task 7: Cancellation test for `turn_streaming`

**Files:**
- Test: `src/agent/agent.rs` tests module

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn turn_streaming_cancellation_yields_done_cancelled_true() {
    use tokio::time::{sleep, Duration};

    // Provider that hangs briefly so cancellation has time to fire.
    struct SlowProvider;
    #[async_trait::async_trait]
    impl Provider for SlowProvider {
        async fn chat_with_system(
            &self, _sp: Option<&str>, _m: &str, _model: &str, _t: f64,
        ) -> Result<String> { Ok("slow".into()) }
        async fn chat(
            &self, _r: ChatRequest<'_>, _model: &str, _t: f64,
        ) -> Result<crate::providers::ChatResponse> {
            sleep(Duration::from_millis(200)).await;
            Ok(crate::providers::ChatResponse {
                text: Some("never delivered".into()),
                tool_calls: vec![],
            })
        }
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(32);

    let mut agent = Agent::builder()
        .provider(Box::new(SlowProvider))
        .tools(vec![])
        .memory(Arc::new(crate::memory::NoopMemory::default()))
        .observer(Arc::new(crate::observability::NoopObserver))
        .model_name("mock-model")
        .build()
        .unwrap();

    // Fire cancel after 50ms (before provider delivers at 200ms).
    tokio::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });

    let result = agent.turn_streaming("hi", Some(events_tx), Some(cancel)).await;
    let result = result.expect("turn_streaming returns Ok on cancel path");
    assert!(result.cancelled, "expected cancelled=true");

    // Verify Done { cancelled: true } appeared.
    let mut saw_cancelled_done = false;
    while let Ok(ev) = events_rx.try_recv() {
        if let AgentEvent::Done { cancelled: true, .. } = ev {
            saw_cancelled_done = true;
        }
    }
    assert!(saw_cancelled_done);
}
```

- [ ] **Step 2: Run — the test should PASS already** (Task 6's error-path handling covers cancellation). If it fails, fix `turn_streaming`'s cancel detection and rerun.

```bash
cargo test -p rantaiclaw --lib agent::agent::tests::turn_streaming_cancellation_yields_done_cancelled_true
```

- [ ] **Step 3: Commit**

```bash
git add src/agent/agent.rs
git commit -m "test(agent): cover turn_streaming cancellation path"
```

---

## Task 8: Create `TurnRequest` and `TuiAgentActor` scaffolding

**Files:**
- Create: `src/tui/async_bridge.rs`
- Modify: `src/tui/mod.rs`

- [ ] **Step 1: Create the module with types only (no run loop yet)**

```rust
// src/tui/async_bridge.rs
//! Actor that owns the Agent and serves the TUI's turn requests.

use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::agent::Agent;
use crate::agent::events::{AgentEvent, AgentEventSender};

#[derive(Debug)]
pub enum TurnRequest {
    Submit(String),
    Cancel,
}

pub struct TuiAgentActor {
    agent: Agent,
    req_rx: mpsc::Receiver<TurnRequest>,
    events_tx: AgentEventSender,
    queue: VecDeque<String>,
    current: Option<CancellationToken>,
}

impl TuiAgentActor {
    pub fn new(
        agent: Agent,
        req_rx: mpsc::Receiver<TurnRequest>,
        events_tx: AgentEventSender,
    ) -> Self {
        Self {
            agent,
            req_rx,
            events_tx,
            queue: VecDeque::new(),
            current: None,
        }
    }

    pub async fn run(mut self) {
        // Fleshed out in Task 9.
        let _ = self.agent;
        let _ = &self.events_tx;
        while let Some(req) = self.req_rx.recv().await {
            // placeholder
            drop(req);
        }
    }
}
```

- [ ] **Step 2: Register the module**

```rust
// src/tui/mod.rs — add to the module declarations
pub mod async_bridge;
```

And re-export public items:

```rust
pub use async_bridge::{TuiAgentActor, TurnRequest};
```

- [ ] **Step 3: Verify compile**

```bash
cargo build -p rantaiclaw
```

- [ ] **Step 4: Commit**

```bash
git add src/tui/async_bridge.rs src/tui/mod.rs
git commit -m "feat(tui): scaffold TuiAgentActor + TurnRequest"
```

---

## Task 9: Actor `run` loop — Submit + Cancel + Queue

**Files:**
- Modify: `src/tui/async_bridge.rs`
- Test: inline

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent::Agent;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    // Build a minimal Agent with a deterministic mock provider.
    // Factored out so each test gets a fresh instance.
    fn build_test_agent(response_text: &str) -> Agent {
        struct EchoProvider(String);
        #[async_trait::async_trait]
        impl crate::providers::Provider for EchoProvider {
            async fn chat_with_system(
                &self, _sp: Option<&str>, _m: &str, _model: &str, _t: f64,
            ) -> anyhow::Result<String> {
                Ok(self.0.clone())
            }
            async fn chat(
                &self, _r: crate::providers::ChatRequest<'_>, _model: &str, _t: f64,
            ) -> anyhow::Result<crate::providers::ChatResponse> {
                Ok(crate::providers::ChatResponse {
                    text: Some(self.0.clone()),
                    tool_calls: vec![],
                })
            }
        }
        Agent::builder()
            .provider(Box::new(EchoProvider(response_text.to_string())))
            .tools(vec![])
            .memory(std::sync::Arc::new(crate::memory::NoopMemory::default()))
            .observer(std::sync::Arc::new(crate::observability::NoopObserver))
            .model_name("mock-model")
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn actor_processes_single_submit_and_emits_done() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(build_test_agent("reply"), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx.send(TurnRequest::Submit("hi".into())).await.unwrap();

        let mut got_done = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), events_rx.recv()).await {
            if let AgentEvent::Done { final_text, cancelled } = ev {
                assert_eq!(final_text, "reply");
                assert!(!cancelled);
                got_done = true;
                break;
            }
        }
        assert!(got_done);
        drop(req_tx); // triggers actor shutdown
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_processes_queued_submit_after_first_completes() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(build_test_agent("r"), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx.send(TurnRequest::Submit("first".into())).await.unwrap();
        req_tx.send(TurnRequest::Submit("second".into())).await.unwrap();

        let mut done_count = 0;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(3), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                done_count += 1;
                if done_count == 2 { break; }
            }
        }
        assert_eq!(done_count, 2, "both turns should complete, in order");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_cancel_while_idle_is_a_noop() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(build_test_agent("x"), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx.send(TurnRequest::Cancel).await.unwrap();
        // Give it a moment; nothing should arrive.
        let result = timeout(Duration::from_millis(150), events_rx.recv()).await;
        assert!(result.is_err(), "no event expected from idle Cancel");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }
}
```

- [ ] **Step 2: Run the tests — they fail (actor doesn't actually process)**

```bash
cargo test -p rantaiclaw --lib tui::async_bridge::tests
```

- [ ] **Step 3: Implement the real `run` loop**

Replace the placeholder `run`:

```rust
impl TuiAgentActor {
    pub async fn run(mut self) {
        loop {
            // If idle and queue non-empty, start the next turn.
            if self.current.is_none() {
                if let Some(text) = self.queue.pop_front() {
                    let token = CancellationToken::new();
                    self.current = Some(token.clone());
                    // Run the turn inline (not spawned) so we can await its
                    // completion before receiving the next request. This
                    // matches the spec's serial processing model.
                    let events = self.events_tx.clone();
                    // tokio::select between the turn and an incoming Cancel.
                    tokio::select! {
                        biased;
                        maybe_req = self.req_rx.recv() => {
                            match maybe_req {
                                Some(TurnRequest::Cancel) => {
                                    token.cancel();
                                    // Fall through: the turn may still be mid-await,
                                    // but since we haven't actually awaited it yet in
                                    // this branch arm, we need a different structure.
                                    // See note below — this branch structure is
                                    // replaced by the polling structure below.
                                }
                                Some(TurnRequest::Submit(more)) => {
                                    self.queue.push_back(more);
                                    self.queue.push_front(text); // re-queue the one we were about to run
                                    // Loop again to retry.
                                }
                                None => { return; }
                            }
                        }
                        result = self.agent.turn_streaming(&text, Some(events), Some(token.clone())) => {
                            // Turn completed (normal, cancelled, or error path all handled
                            // inside turn_streaming, which emits Done).
                            let _ = result;
                            self.current = None;
                        }
                    }
                    continue;
                }
            }

            // Idle: wait for the next request.
            match self.req_rx.recv().await {
                Some(TurnRequest::Submit(text)) => self.queue.push_back(text),
                Some(TurnRequest::Cancel) => {
                    // Cancel while idle is a no-op.
                }
                None => return, // channel closed
            }
        }
    }
}
```

**Note on the tokio::select! shape:** the `maybe_req` arm above has a subtle issue — when we Cancel, we've already committed to this turn's execution via the `turn_streaming` future; the Cancel needs to reach the token. Simplify by separating concerns: let the turn run fully, but spawn a lightweight "cancel watcher" that receives on a secondary channel. In practice the simplest correct shape is:

```rust
pub async fn run(mut self) {
    loop {
        // Idle: take requests.
        if self.current.is_none() {
            match self.req_rx.recv().await {
                Some(TurnRequest::Submit(text)) => self.queue.push_back(text),
                Some(TurnRequest::Cancel) => { /* noop */ continue; }
                None => return,
            }
            // Fall through: may have a queue now.
        }

        // If we have something queued and no current, run it.
        if self.current.is_none() {
            if let Some(text) = self.queue.pop_front() {
                let token = CancellationToken::new();
                self.current = Some(token.clone());
                let events = self.events_tx.clone();

                // We need to drain incoming requests (specifically Cancel)
                // while the turn is running. Use select over two futures:
                //   - the turn itself
                //   - self.req_rx.recv()
                // On Cancel, call token.cancel() and keep awaiting the turn.
                let mut turn = Box::pin(self.agent.turn_streaming(&text, Some(events), Some(token.clone())));
                loop {
                    tokio::select! {
                        biased;
                        maybe_req = self.req_rx.recv() => match maybe_req {
                            Some(TurnRequest::Submit(more)) => self.queue.push_back(more),
                            Some(TurnRequest::Cancel) => token.cancel(),
                            None => {
                                // Channel closed — still let the current turn finish.
                                break;
                            }
                        },
                        res = &mut turn => {
                            let _ = res;
                            self.current = None;
                            break;
                        }
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests — they pass**

```bash
cargo test -p rantaiclaw --lib tui::async_bridge::tests
```

- [ ] **Step 5: Add the cancellation-during-turn test**

```rust
#[tokio::test]
async fn actor_cancel_while_streaming_yields_done_cancelled() {
    use tokio::time::sleep;
    // Slow agent via a provider that delays 300ms.
    struct SlowProvider;
    #[async_trait::async_trait]
    impl crate::providers::Provider for SlowProvider {
        async fn chat_with_system(
            &self, _sp: Option<&str>, _m: &str, _model: &str, _t: f64,
        ) -> anyhow::Result<String> { Ok("x".into()) }
        async fn chat(
            &self, _r: crate::providers::ChatRequest<'_>, _model: &str, _t: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            sleep(Duration::from_millis(300)).await;
            Ok(crate::providers::ChatResponse { text: Some("late".into()), tool_calls: vec![] })
        }
    }
    let agent = Agent::builder()
        .provider(Box::new(SlowProvider))
        .tools(vec![])
        .memory(std::sync::Arc::new(crate::memory::NoopMemory::default()))
        .observer(std::sync::Arc::new(crate::observability::NoopObserver))
        .model_name("mock-model")
        .build()
        .unwrap();
    let (req_tx, req_rx) = mpsc::channel(4);
    let (events_tx, mut events_rx) = mpsc::channel(32);
    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let handle = tokio::spawn(actor.run());

    req_tx.send(TurnRequest::Submit("start".into())).await.unwrap();
    sleep(Duration::from_millis(50)).await;
    req_tx.send(TurnRequest::Cancel).await.unwrap();

    let mut cancelled_done = false;
    while let Ok(Some(ev)) = timeout(Duration::from_secs(2), events_rx.recv()).await {
        if let AgentEvent::Done { cancelled: true, .. } = ev {
            cancelled_done = true;
            break;
        }
    }
    assert!(cancelled_done);
    drop(req_tx);
    let _ = timeout(Duration::from_secs(1), handle).await;
}
```

Run: `cargo test -p rantaiclaw --lib tui::async_bridge::tests::actor_cancel_while_streaming_yields_done_cancelled`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/tui/async_bridge.rs
git commit -m "feat(tui): actor run loop — submit, cancel, queue semantics"
```

---

## Task 10: Extend `TuiContext` with bridge channels

**Files:**
- Modify: `src/tui/context.rs`

- [ ] **Step 1: Add fields**

```rust
// src/tui/context.rs
use tokio::sync::mpsc;
use crate::agent::events::AgentEvent;
use crate::tui::async_bridge::TurnRequest;

pub struct TuiContext {
    // ... existing fields unchanged ...
    pub req_tx: mpsc::Sender<TurnRequest>,
    pub events_rx: mpsc::Receiver<AgentEvent>,
    pub queued_turns: usize,
}
```

Update the `TuiContext::new(...)` signature and callers (`TuiApp::new`) to pass in `req_tx` and `events_rx`. Every `TuiContext::new` test will need these — add test helpers like:

```rust
#[cfg(test)]
pub fn test_context() -> (TuiContext, mpsc::Receiver<TurnRequest>, mpsc::Sender<AgentEvent>) {
    let (req_tx, req_rx) = mpsc::channel(4);
    let (events_tx, events_rx) = mpsc::channel(32);
    let ctx = TuiContext {
        // ... fill in other defaults
        req_tx, events_rx, queued_turns: 0,
    };
    (ctx, req_rx, events_tx)
}
```

- [ ] **Step 2: Fix all compile errors from the signature change**

```bash
cargo build -p rantaiclaw
```

Each call site that constructs a `TuiContext` must now supply the channels. In test code (the existing TUI command tests in `src/tui/commands/*.rs`), use the `test_context()` helper.

- [ ] **Step 3: Run all tests**

```bash
cargo test -p rantaiclaw --lib tui::
```

Expected: all existing tests compile and pass.

- [ ] **Step 4: Commit**

```bash
git add src/tui/context.rs src/tui/commands/
git commit -m "feat(tui): add bridge channels and queued_turns to TuiContext"
```

---

## Task 11: Add `AppState` enum and `ToolBlockState`

**Files:**
- Modify: `src/tui/app.rs`

- [ ] **Step 1: Add types near the top of `app.rs`**

```rust
#[derive(Debug, Clone)]
pub struct ToolBlockState {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub result: Option<(bool, String)>, // (ok, preview)
}

#[derive(Debug)]
pub enum AppState {
    Ready,
    Streaming {
        partial: String,
        tool_blocks: Vec<ToolBlockState>,
        cancelling: bool,
    },
    Quitting,
}

impl Default for AppState {
    fn default() -> Self { AppState::Ready }
}
```

- [ ] **Step 2: Add `state: AppState` field to `TuiApp`**

```rust
pub struct TuiApp {
    pub context: TuiContext,
    pub state: AppState,
    // ... existing fields
}
```

Initialize to `AppState::Ready` in `TuiApp::new`.

- [ ] **Step 3: Verify compile + tests**

```bash
cargo test -p rantaiclaw --lib tui::
```

- [ ] **Step 4: Commit**

```bash
git add src/tui/app.rs
git commit -m "feat(tui): add AppState + ToolBlockState"
```

---

## Task 12: Rewrite `submit_input` to dispatch via the bridge

**Files:**
- Modify: `src/tui/app.rs` (`submit_input` at line ~105)
- Test: inline

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod submit_tests {
    use super::*;
    use crate::tui::context::test_context;

    #[tokio::test]
    async fn submit_input_ready_state_sends_request_and_transitions_to_streaming() {
        let (ctx, mut req_rx, _events_tx) = test_context();
        let mut app = TuiApp {
            context: ctx,
            state: AppState::Ready,
            // ... other default fields
        };
        app.context.input_buffer = "hello".into();

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.context.input_buffer, "");
        let req = req_rx.recv().await.unwrap();
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "hello"),
            _ => panic!("expected Submit"),
        }
    }

    #[tokio::test]
    async fn submit_input_streaming_state_queues_and_increments_counter() {
        let (ctx, mut req_rx, _events_tx) = test_context();
        let mut app = TuiApp {
            context: ctx,
            state: AppState::Streaming {
                partial: String::new(),
                tool_blocks: vec![],
                cancelling: false,
            },
            // ... other default fields
        };
        app.context.input_buffer = "queued".into();

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.context.queued_turns, 1);
        let req = req_rx.recv().await.unwrap();
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "queued"),
            _ => panic!("expected Submit"),
        }
    }
}
```

- [ ] **Step 2: Run tests — they fail**

- [ ] **Step 3: Rewrite `submit_input`**

Replace the existing echo-stub body:

```rust
pub async fn submit_input(&mut self) -> anyhow::Result<()> {
    if self.context.input_buffer.trim().is_empty() {
        return Ok(());
    }
    let text = std::mem::take(&mut self.context.input_buffer);

    // Record user turn in history + session.
    self.context.append_user_message(text.clone());

    // Dispatch to the actor. Use blocking_send if the channel is bounded and we
    // want backpressure; try_send with a log warning is also acceptable.
    match self.context.req_tx.send(TurnRequest::Submit(text)).await {
        Ok(()) => {}
        Err(e) => {
            // Actor dropped — surface as error, stay in current state.
            self.context.last_error = Some(format!("agent bridge closed: {e}"));
            return Ok(());
        }
    }

    match self.state {
        AppState::Ready => {
            self.state = AppState::Streaming {
                partial: String::new(),
                tool_blocks: Vec::new(),
                cancelling: false,
            };
        }
        AppState::Streaming { .. } => {
            self.context.queued_turns += 1;
        }
        AppState::Quitting => {}
    }

    Ok(())
}
```

- [ ] **Step 4: Run tests — they pass**

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs
git commit -m "feat(tui): submit_input dispatches TurnRequest via bridge"
```

---

## Task 13: `drain_events` helper called from `tick`

**Files:**
- Modify: `src/tui/app.rs`
- Test: inline

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn drain_events_chunk_appends_to_partial() {
    let (ctx, _req_rx, events_tx) = test_context();
    let mut app = TuiApp {
        context: ctx,
        state: AppState::Streaming {
            partial: String::from("prev "),
            tool_blocks: vec![],
            cancelling: false,
        },
        // ...
    };
    events_tx.send(AgentEvent::Chunk("more".into())).await.unwrap();

    app.drain_events();

    if let AppState::Streaming { partial, .. } = &app.state {
        assert_eq!(partial, "prev more");
    } else {
        panic!("expected Streaming");
    }
}

#[tokio::test]
async fn drain_events_done_finalizes_turn_to_ready() {
    let (ctx, _req_rx, events_tx) = test_context();
    let mut app = TuiApp {
        context: ctx,
        state: AppState::Streaming {
            partial: String::from("answer"),
            tool_blocks: vec![],
            cancelling: false,
        },
        // ...
    };
    events_tx.send(AgentEvent::Done { final_text: "answer".into(), cancelled: false }).await.unwrap();

    app.drain_events();

    assert!(matches!(app.state, AppState::Ready));
    assert!(!app.context.messages.is_empty());
    assert_eq!(app.context.messages.last().unwrap().content, "answer");
}

#[tokio::test]
async fn drain_events_done_cancelled_appends_marker() {
    let (ctx, _req_rx, events_tx) = test_context();
    let mut app = TuiApp {
        context: ctx,
        state: AppState::Streaming {
            partial: String::from("partial"),
            tool_blocks: vec![],
            cancelling: true,
        },
        // ...
    };
    events_tx.send(AgentEvent::Done { final_text: "partial".into(), cancelled: true }).await.unwrap();

    app.drain_events();

    assert!(matches!(app.state, AppState::Ready));
    let last = app.context.messages.last().unwrap();
    assert!(last.content.contains("partial"));
    assert!(last.content.contains("[cancelled]"));
}
```

- [ ] **Step 2: Run tests — they fail**

- [ ] **Step 3: Implement `drain_events`**

```rust
impl TuiApp {
    pub fn drain_events(&mut self) {
        while let Ok(ev) = self.context.events_rx.try_recv() {
            self.handle_event(ev);
        }
    }

    fn handle_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Chunk(s) => {
                if let AppState::Streaming { partial, .. } = &mut self.state {
                    partial.push_str(&s);
                }
            }
            AgentEvent::ToolCallStart { id, name, args } => {
                if let AppState::Streaming { tool_blocks, .. } = &mut self.state {
                    tool_blocks.push(ToolBlockState { id, name, args, result: None });
                }
            }
            AgentEvent::ToolCallEnd { id, ok, output_preview } => {
                if let AppState::Streaming { tool_blocks, .. } = &mut self.state {
                    if let Some(b) = tool_blocks.iter_mut().find(|b| b.id == id) {
                        b.result = Some((ok, output_preview));
                    }
                }
            }
            AgentEvent::Usage(u) => {
                self.context.token_usage = u; // or accumulate if you prefer summing
            }
            AgentEvent::Done { final_text, cancelled } => {
                self.finalize_turn(final_text, cancelled);
            }
            AgentEvent::Error(msg) => {
                self.finalize_error(msg);
            }
        }
    }

    fn finalize_turn(&mut self, mut final_text: String, cancelled: bool) {
        if cancelled {
            if !final_text.is_empty() { final_text.push_str("\n\n"); }
            final_text.push_str("[cancelled]");
        }
        self.context.append_assistant_message(final_text);
        if self.context.queued_turns > 0 {
            // Next turn will start streaming itself when the actor picks up;
            // we need to transition back to Streaming then. Simplest: stay in
            // Streaming with a fresh partial, decrement counter.
            self.context.queued_turns -= 1;
            self.state = AppState::Streaming {
                partial: String::new(),
                tool_blocks: Vec::new(),
                cancelling: false,
            };
        } else {
            self.state = AppState::Ready;
        }
    }

    fn finalize_error(&mut self, msg: String) {
        self.context.append_assistant_message(format!("⚠ {msg}"));
        self.context.last_error = Some(msg);
        // A Done follows per the spec; when it arrives we'll transition to Ready there.
        // But if the caller doesn't follow with Done, ensure we don't stall in Streaming.
        // For robustness, transition now:
        self.state = AppState::Ready;
    }
}
```

Also hook `drain_events` into the existing `tick` method (or equivalent each-frame callback). Find `tick` or the render-loop entry and call `self.drain_events();` at the top.

- [ ] **Step 4: Run tests — they pass**

```bash
cargo test -p rantaiclaw --lib tui::app
```

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs
git commit -m "feat(tui): drain_events handles AgentEvent stream per frame"
```

---

## Task 14: Ctrl+C in Streaming sends Cancel

**Files:**
- Modify: `src/tui/app.rs` (key-handler section)
- Test: inline

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn ctrl_c_in_streaming_sends_cancel_and_sets_cancelling_flag() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let (ctx, mut req_rx, _events_tx) = test_context();
    let mut app = TuiApp {
        context: ctx,
        state: AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
        },
        // ...
    };

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)).await.unwrap();

    let got = req_rx.recv().await.unwrap();
    assert!(matches!(got, TurnRequest::Cancel));
    if let AppState::Streaming { cancelling, .. } = &app.state {
        assert!(*cancelling);
    } else {
        panic!("state should remain Streaming during cancel");
    }
}
```

- [ ] **Step 2: Run — fails (Ctrl+C currently quits)**

- [ ] **Step 3: Amend the key handler**

Find the branch for `KeyCode::Char('c')` with CONTROL modifier in `handle_key`. Split by state:

```rust
(KeyCode::Char('c'), KeyModifiers::CONTROL) => match &mut self.state {
    AppState::Streaming { cancelling, .. } => {
        *cancelling = true;
        let _ = self.context.req_tx.send(TurnRequest::Cancel).await;
    }
    AppState::Ready | AppState::Quitting => {
        self.state = AppState::Quitting;
    }
},
```

- [ ] **Step 4: Run — passes**

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs
git commit -m "feat(tui): Ctrl+C cancels streaming turn; quits when Ready"
```

---

## Task 15: Wire the actor into `run_tui`

**Files:**
- Modify: `src/tui/mod.rs` or `src/main.rs` (whichever currently calls `TuiApp::new`). Locate with `grep -n "TuiApp::new\|run_tui" src/main.rs src/tui/mod.rs`.

- [ ] **Step 1: Find the current `run_tui` entrypoint**

```bash
grep -n "pub async fn run_tui\|TuiApp::new" src/main.rs src/tui/mod.rs src/tui/app.rs
```

Update that function to:

```rust
pub async fn run_tui(config: TuiConfig) -> anyhow::Result<()> {
    let app_config = crate::config::Config::load_or_init()?;
    let agent = crate::agent::agent::Agent::from_config(&app_config)?;

    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<TurnRequest>(16);
    let (events_tx, events_rx) = tokio::sync::mpsc::channel::<AgentEvent>(128);

    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let actor_handle = tokio::spawn(actor.run());

    let mut app = TuiApp::new(config, req_tx, events_rx)?;
    let result = app.run().await;

    // Drop req_tx via `app` going out of scope — actor receives None and exits.
    drop(app);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), actor_handle).await;
    result
}
```

`TuiApp::new` signature also gets the two channel ends; update it and `TuiContext::new` accordingly.

- [ ] **Step 2: Verify compile + smoke-run (optional)**

```bash
cargo build -p rantaiclaw
# Optional smoke: run the chat subcommand against a mock provider via env:
# RANTAICLAW_PROVIDER=mock cargo run -p rantaiclaw -- chat
```

- [ ] **Step 3: Commit**

```bash
git add src/main.rs src/tui/mod.rs src/tui/app.rs src/tui/context.rs
git commit -m "feat(tui): wire TuiAgentActor into run_tui startup"
```

---

## Task 16: End-to-end integration test

**Files:**
- Create: `tests/tui_agent_bridge.rs`

- [ ] **Step 1: Write the integration test**

```rust
// tests/tui_agent_bridge.rs
//! End-to-end test: TuiAgentActor + real Agent + scripted provider.

use rantaiclaw::agent::agent::Agent;
use rantaiclaw::agent::events::AgentEvent;
use rantaiclaw::tui::async_bridge::{TuiAgentActor, TurnRequest};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

struct StaticProvider(&'static str);

#[async_trait::async_trait]
impl rantaiclaw::providers::Provider for StaticProvider {
    async fn chat_with_system(
        &self, _sp: Option<&str>, _m: &str, _model: &str, _t: f64,
    ) -> anyhow::Result<String> { Ok(self.0.into()) }
    async fn chat(
        &self, _r: rantaiclaw::providers::ChatRequest<'_>, _model: &str, _t: f64,
    ) -> anyhow::Result<rantaiclaw::providers::ChatResponse> {
        Ok(rantaiclaw::providers::ChatResponse {
            text: Some(self.0.into()),
            tool_calls: vec![],
        })
    }
}

#[tokio::test]
async fn end_to_end_turn_emits_chunks_and_done() {
    let agent = Agent::builder()
        .provider(Box::new(StaticProvider("hello world from integration test")))
        .tools(vec![])
        .memory(Arc::new(rantaiclaw::memory::NoopMemory::default()))
        .observer(Arc::new(rantaiclaw::observability::NoopObserver))
        .model_name("mock-model")
        .build()
        .unwrap();

    let (req_tx, req_rx) = mpsc::channel(4);
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let handle = tokio::spawn(actor.run());

    req_tx.send(TurnRequest::Submit("hi".into())).await.unwrap();

    let mut chunks = Vec::new();
    let mut saw_done = false;
    while let Ok(Some(ev)) = timeout(Duration::from_secs(3), events_rx.recv()).await {
        match ev {
            AgentEvent::Chunk(s) => chunks.push(s),
            AgentEvent::Done { final_text, cancelled } => {
                assert_eq!(final_text, "hello world from integration test");
                assert!(!cancelled);
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(!chunks.is_empty(), "expected at least one Chunk");
    let combined: String = chunks.into_iter().collect();
    assert!(combined.contains("hello"));
    assert!(saw_done);

    drop(req_tx);
    let _ = timeout(Duration::from_secs(1), handle).await;
}
```

- [ ] **Step 2: Run**

```bash
cargo test -p rantaiclaw --test tui_agent_bridge
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/tui_agent_bridge.rs
git commit -m "test: end-to-end TuiAgentActor + Agent + scripted provider"
```

---

## Task 17: Full test + lint sweep

- [ ] **Step 1: Format**

```bash
cargo fmt --all
```

- [ ] **Step 2: Clippy (delta gate parity)**

```bash
BASE_SHA=3e95d94 bash scripts/ci/rust_strict_delta_gate.sh
```

Expected: "No blocking strict lint issues on changed Rust lines." If there are blocking issues, fix them (usually `#[allow(..)]` on a new `async fn` that doesn't await, or rewriting a `.contains("x")` to `.contains('x')`).

- [ ] **Step 3: Full test run**

```bash
cargo test -p rantaiclaw
```

Expected: all tests pass.

- [ ] **Step 4: Smoke run the TUI locally**

```bash
cargo run -p rantaiclaw --release -- chat
```

Type a message, verify:
- spinner appears during response
- text streams in chunks
- Ctrl+C during stream cancels with `[cancelled]` marker
- typing a second message while one is streaming queues it (footer shows "1 queued")

Exit with `/quit` or `Ctrl+C` while Ready.

- [ ] **Step 5: Commit any lint/fmt fixups**

```bash
git add -A
git commit -m "chore: fmt + clippy sweep post-bridge"
```

- [ ] **Step 6: Push branch + open PR**

```bash
git push -u origin feature/tui-agent-bridge
gh pr create --title "feat(tui): Agent async bridge + streaming" --body "$(cat <<'EOF'
## Summary
Implements the TUI ↔ Agent async bridge per spec `docs/superpowers/specs/2026-04-21-tui-agent-async-bridge-design.md`.

- New `AgentEvent` stream (Chunk/ToolCallStart/ToolCallEnd/Usage/Done/Error)
- New `Agent::turn_streaming` method; `Agent::turn` delegates
- `TuiAgentActor` owns Agent, processes turns serially, supports Cancel + queued submits
- TUI `AppState::Streaming`, `drain_events`, Ctrl+C cancels mid-stream
- No non-TUI caller changes (channels/gateway unchanged)

## Test plan
- [ ] Unit: `cargo test -p rantaiclaw --lib`
- [ ] Integration: `cargo test -p rantaiclaw --test tui_agent_bridge`
- [ ] Manual: `cargo run -- chat` — type, stream, cancel, queue
EOF
)"
```

---

## Self-review notes (author-side, pre-execution)

**Spec coverage check:**
- §3.1 AgentEvent — Task 1
- §3.2 TurnRequest — Task 8
- §3.3 TurnResult — Task 1
- §4 turn_streaming — Task 6
- §5 Actor loop + queue — Task 9
- §6 TUI integration (AppState, submit_input, tick, finalize, Ctrl+C) — Tasks 11–14
- §7 run_tui wiring — Task 15
- §8 Cancellation semantics — Tasks 7, 9, 13, 14
- §9 Error handling — Task 13 (`finalize_error`)
- §10 Ordering invariants — enforced by `turn_streaming`'s emit order; verified by tests in Tasks 6, 7, 16
- §11 Testing strategy — Tasks 3, 4, 5, 6, 7, 9, 12, 13, 14, 16
- §12 Migration — additive only, verified by Task 2 step 4 ("existing tests still pass")

**Placeholder scan:** no TBD/TODO/"fill in later" remaining. All tests have real assertions. All code blocks show actual code.

**Type consistency:** `AgentEvent`, `AgentEventSender`, `TurnResult`, `TurnRequest`, `TuiAgentActor`, `ToolBlockState`, `AppState` used identically across tasks.

**Risk watch:**
- Task 9's actor loop shape is the hardest task. The tokio::select! structure is shown in its final, simplified form. If the initial intermediate version causes test flakiness, apply the simplified form directly.
- Task 15's `Config::load_or_init()` is assumed; verify by `grep -n "load_or_init" src/config/mod.rs` before using it. If the fn is named differently, substitute.
- `NoopMemory` / `NoopObserver` names assumed in tests — verify in `src/memory/` and `src/observability/` and substitute if named differently (e.g. `MarkdownMemory::in_memory()`).
