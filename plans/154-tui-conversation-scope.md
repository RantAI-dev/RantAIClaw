# Plan 154: give the TUI a conversation scope for memory reads and writes

> **Executor instructions**: One PR, one concern: the TUI's conversation
> identity for layered memory. Read the whole plan before editing. Step 0 is
> an investigation step — do it before writing code. Run every verification
> command. If anything under "STOP conditions" occurs, stop and report. When
> done, add this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat d0089a4..HEAD -- src/agent/agent.rs src/memory/mod.rs src/tui/app.rs src/tui/async_bridge.rs src/tui/mod.rs src/gateway/api_v1.rs`
> All line numbers below are from `d0089a4`. If this diff is non-empty,
> re-verify each cited line before editing.

## Status

- **DONE 2026-08-17** — PR #556 (merged 831340c) + follow-up 3924591 for tests/+examples/ tuple-form callers, released **v0.22.0-alpha**

- **Priority**: P1 — the TUI reads and writes the memory store completely
  unscoped: it recalls every channel's rows, and its own auto-saves land in
  the shared tier where every other surface can recall them (leak in both
  directions)
- **Effort**: S-M
- **Risk**: LOW-MEDIUM (recall behaviour changes for TUI users: other
  conversations' rows stop backfilling — that is the point)
- **Depends on**: none (complements plan 153; both stand alone)
- **Category**: bugfix (memory / TUI)
- **Planned at**: `d0089a4`, 2026-08-17

## Why this matters

The scoping machinery is complete and tested — and the TUI does not use it:

- `recall_layered` (`src/memory/mod.rs:467-521`) surfaces the conversation's
  own rows first, then backfills **only** unscoped ("shared") rows; rows
  scoped to another conversation are filtered (`mod.rs:501-518`). But when
  `conversation_id` is `None` it short-circuits to a fully global
  `memory.recall(query, limit, None)` (`mod.rs:473-475`) — no filter at all.
- Only the gateway sets a conversation id
  (`src/gateway/api_v1.rs:511`, `:612`). The interactive agent's builder
  default is `None` (`src/agent/agent.rs:144`), and nothing in `src/tui/`
  ever calls `set_conversation_id` (`agent.rs:588`).
- Consequences, both directions:
  1. TUI recall is global → Telegram auto-save rows surfaced in a TUI turn
     (live repro, 2026-08-16 — see plan 153).
  2. TUI auto-saves (`agent.rs:985-997`) store with `conversation_scope =
     None` → they sit in the shared tier and backfill into **channel**
     prompts too.
  3. The TUI status line `↺ recalled N memories: N from this conversation`
     (`src/tui/app.rs:4840-4872`) cannot actually know the rows are from
     this conversation — without scope they may be from any past TUI session.
- Channel-side write scoping already works: dispatch stores under
  `conversation_memory_scope` (`src/channels/dispatch.rs:63-66`), the same
  chat-keyed id as history. The TUI is the missing surface.

## The fix

Thread a per-session conversation id through the TUI's agent, using the
existing `ConversationKey` scheme (`src/channels/conversation.rs` — the one
tested place that formats conversation ids; surfaces already include
`"cli"`/`"webhook"`): **`ConversationKey::new("tui", <session_id>).resolve()`
→ `tui:<session_id>`.**

Once set, both sides scope automatically — `turn_inner` already uses
`self.conversation_id` for the auto-save write **and** the `recall_layered`
read (`agent.rs:983-1007`); shared unscoped facts (curated `memory_store`
entries) still backfill, which is the intended shared tier.

### Steps

0. **Investigate the TUI session lifecycle** (read-only): the TUI drives the
   agent through `TuiAgentActor` (`src/tui/async_bridge.rs:31`) and persists
   sessions via the sessions store (`src/sessions/store.rs:115`; the TUI
   opens it via its `open_*_session_store` helper). Enumerate every point
   where the active session changes: agent construction, new session,
   `/resume`-style session switch, and session rename (rename must NOT change
   the id — verify ids are stable UUIDs/slugs, not names). List the
   file:line anchors in the PR description.
1. **Set the scope at each lifecycle point**: call
   `agent.set_conversation_id(Some(ConversationKey::new("tui", &session_id).resolve()))`
   when the agent is created with an active session and whenever the active
   session switches. Follow the gateway precedent (`api_v1.rs:511`) — set per
   change, not per turn, unless the actor's architecture makes per-turn
   simpler; either is correct, pick the one with the smaller diff.
2. **Honesty of the recall notice**: with a real scope, `render_recalled_memories`
   (`src/tui/app.rs:4840`) labeling of `user_msg_*` keys as "from this
   conversation" becomes true for newly written rows. No display change
   required; verify the wording still holds after plan 153 (conversation
   rows no longer injected at all — the notice will mostly name curated
   keys). Adjust the label only if it now lies.
3. **Docs**: `docs/reference/config.md` memory section — one sentence: TUI
   sessions scope their memory like channel chats; unscoped entries remain
   the shared tier.

### Non-goals

- The bare CLI loop's `build_context` (`src/agent/loop_.rs:240-256`) keeps
  `None` — its comment is explicit that the one-shot CLI has no conversation
  identity, and inventing one there changes which memories surface for
  `run_single` users. Separate decision, separate plan if wanted.
- Backfilling a scope onto legacy NULL-scoped `user_msg_*` rows (no
  migration; plan 153 already stops them being injected).
- `memory_recall` tool scoping (plan 156).

### Tests (write first, watch them fail)

- Agent-level (there is a precedent test `turn_stores_memory_under_conversation_scope`
  in `src/agent/tests.rs` — extend beside it): an agent with
  `conversation_id = Some("tui:s1")` recalls via a mock memory that records
  the `session_id` argument → the scoped call must receive `Some("tui:s1")`,
  and the auto-save `store` must receive the same scope. Control: default
  agent (no id) still passes `None` (pins that this plan changes the TUI
  wiring, not the default).
- TUI-level: whatever seam step 0 found (actor construction or session
  switch) — a test that switching the active session updates the agent's
  conversation id, and that a session **rename** does not change it.
- **Mutation check**: remove the `set_conversation_id` call at the session-
  switch site — the switch test must go red.

### Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib agent::
cargo test --lib tui::
cargo test --lib memory::
```

Live drive (tmux, freshly built binary, sandbox `RANTAICLAW_CONFIG_DIR` with
a copied `brain.db` that contains `telegram:*`-scoped rows): in a TUI session,
ask something semantically close to a Telegram row. Before: the row can be
recalled (if plan 153 is not yet merged) or the recall call is observably
unscoped. After: recall runs scoped; `telegram:*`-scoped rows never surface;
a NULL-scoped curated fact still can.

## STOP conditions

- The TUI has no stable per-session identifier (ids regenerate per launch or
  change on rename) — scoping to an unstable id would fragment memory per
  launch; stop and report options instead of picking one silently.
- `TuiAgentActor` owns the agent in a way that makes `set_conversation_id`
  unreachable from the session-switch path without an actor-message change —
  report the seam you'd add before building it.

## Rollback

Single revert; newly scoped rows keep their `session_id` values but plan 153's
category filter keeps them out of prompts either way, and `recall_layered`
treats them as conversation-local — no data cleanup needed.
