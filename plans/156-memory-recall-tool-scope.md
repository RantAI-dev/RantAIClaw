# Plan 156 (optional): scope the `memory_recall` tool to the active conversation

> **Executor instructions**: One PR, one concern: the explicit
> `memory_recall` tool's recall scope. This plan is OPTIONAL hardening — the
> operator may drop it; confirm it is still wanted before starting. Execute
> after plan 154 (it reuses the conversation id 154 threads through). If
> anything under "STOP conditions" occurs, stop and report. When done, add
> this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat d0089a4..HEAD -- src/tools/memory_recall.rs src/tools/mod.rs src/agent/agent.rs src/channels/dispatch.rs src/gateway/api_v1.rs`
> All line numbers below are from `d0089a4`. If this diff is non-empty,
> re-verify each cited line before editing.

## Status

- **DONE 2026-08-17** — PR #562 (merged 8735d9e), released **v0.22.1-alpha**; channels/webhook keep global reads per the STOP-condition fallback (shared slot would race)

- **Priority**: P3 — defence in depth, not a live leak. The default
  deployment is already protected: `guest_allowed_tools` defaults to empty
  (`src/config/schema.rs:2863`), and the guest gate in the shared tool loop
  denies non-allowlisted tools for non-owners (`src/agent/loop_.rs:1253-1270`),
  so a channel guest cannot invoke `memory_recall` unless an operator
  explicitly grants it
- **Effort**: S-M (the seam is the work, not the logic)
- **Risk**: MEDIUM (touches the shared tool registry's relationship to
  per-conversation state — the exact coupling §6.4 warns about; keep the
  seam minimal)
- **Depends on**: plan 154
- **Category**: hardening (memory / tools)
- **Planned at**: `d0089a4`, 2026-08-17

## Why this matters

`MemoryRecallTool::execute` calls `self.memory.recall(query, limit, None)`
(`src/tools/memory_recall.rs:58`) — a **global, scope-ignoring** search. After
plans 153/154 close silent injection, this stays the one mechanical path that
crosses conversation scopes: any surface where the model can call the tool
can pull another conversation's auto-saved rows (`telegram_*`,
`user_msg_*` — excluded from *injection* by plan 153, still fully
searchable here).

Exposure honestly stated:

- **Owner surfaces (TUI/CLI/gateway)**: the owner searching their own store
  globally is arguably a feature. The risk is quality, not confidentiality.
- **Channels with a widened guest allowlist**: if an operator adds
  `memory_recall` to `guest_allowed_tools`, a guest in one chat can search
  every other chat's saved turns. That is the case worth closing.

## The fix

Route the tool through the same layered read the injection path uses:
`recall_layered(memory, query, limit, conversation_scope)`
(`src/memory/mod.rs:467`) — conversation-local rows first, shared unscoped
tier as backfill, other conversations' rows filtered.

The seam problem: `Tool::execute(args)` (`src/tools/traits.rs:33`) carries no
per-turn context, and tools are constructed once
(`all_tools_with_runtime`, wired in `src/tools/mod.rs`) while the
conversation changes per message (channels) / per session (TUI, after 154).

**Chosen seam — shared scope slot**: give `MemoryRecallTool` an
`Arc<RwLock<Option<String>>>` conversation-scope slot.

- `Agent::set_conversation_id` (`src/agent/agent.rs:588`) writes the slot
  (the agent already owns the memory handle and the registry; hold the slot
  on the Agent and hand a clone to the tool at construction).
- Channel dispatch sets it per message alongside the scope it already
  computes (`conversation_memory_scope`, `src/channels/dispatch.rs:63`) —
  find where the channel runtime builds/borrows its tool registry and thread
  the same `Arc` there; if the channel path's registry cannot see the slot
  without invasive plumbing, see STOP conditions.
- Slot `None` (bare-builder agents, one-shot CLI) → today's global recall,
  unchanged.

Do NOT widen the `Tool` trait with a context parameter for one tool
(fat-interface change, touches every implementor; explicitly rejected).

### Steps

1. `MemoryRecallTool`: add the slot; `execute` reads it and calls
   `recall_layered` when `Some`, `memory.recall(..., None)` when `None`.
   Update the tool `description` so the model knows recall is scoped to the
   current conversation plus shared memory.
2. Wire the slot: agent construction site (`from_config_with_observer`,
   `agent.rs:454-467` region) and `set_conversation_id`; channel dispatch per
   message; gateway inherits via `set_conversation_id` (already called,
   `api_v1.rs:511/612`).
3. Docs: `docs/reference/config.md` (memory) + the security notes where
   `guest_allowed_tools` is documented — `memory_recall` no longer crosses
   conversation scopes even when guest-granted.

### Tests (write first, watch them fail)

- Tool-level: mock memory recording the `session_id` argument. Slot set to
  `Some("telegram:42")` → scoped call observed (and backfill call with
  `None`, per `recall_layered`'s two-phase read); slot `None` → single global
  call. Control both directions.
- The guest scenario, end-to-end at the loop level if a fixture exists (the
  guest-gate tests around `loop_.rs:1253` are the precedent): a guest-granted
  `memory_recall` in conversation A must not return a row scoped to
  conversation B; a shared unscoped row must still return.
- **Mutation check**: make `execute` ignore the slot — the scoped-call test
  must go red.

### Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib tools::memory_recall
cargo test --lib agent::
cargo test --lib channels::
```

## STOP conditions

- Threading the slot into the channel path requires restructuring how
  channels own their tool registry (more than passing one `Arc` through
  existing constructors) — stop; report the coupling; options: (a) channel
  path keeps global recall + docs state the guest-allowlist caveat, (b) a
  dedicated follow-up plan for registry context. Do not do the restructure
  inside this plan.
- Any existing surface **depends** on cross-conversation recall via this tool
  (grep prompts/docs for promises like "recall from any chat") — report
  before changing the contract.

## Rollback

Single revert; no stored data changes.
