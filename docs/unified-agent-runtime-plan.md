# Unified Agent Runtime — Migration Plan

> **Status:** Active design + migration plan. Branch: `feat/unified-agent-runtime`.
> Large overhaul, shipped in small reversible slices (one concern per PR).
> Companion to `docs/cross-surface-agent-architecture.md` (the *current-state*
> analysis this plan migrates away from).

## 1. Problem (current state)

The agent behaves differently across surfaces (TUI vs Telegram/WhatsApp/Discord/
Slack/gateway) for two structural reasons, not config drift:

1. **Two agent loops.** The TUI runs the `Agent` struct loop (`src/agent/agent.rs`);
   CLI one-shot, channels, and gateway run the free `run_tool_call_loop`
   (`src/agent/loop_.rs`). They drifted apart.
2. **Two prompt builders.** The `Agent` struct uses `SystemPromptBuilder`
   (`src/agent/prompt.rs`, includes **Persona** + **Safety/approval preset**);
   everything else uses `build_system_prompt_with_mode` (`src/channels/mod.rs`,
   **lacks** Persona/Safety, adds Hardware/Task/Channel-Capabilities hints, and
   uses a lighter tool representation).
3. **Approval is keyed off `channel_name == "cli"`** — interactive on TUI,
   hard auto-deny everywhere else, with no model of *who* may approve.

See `docs/cross-surface-agent-architecture.md` for the full file:line map.

## 2. Target design

Validated against the two ancestors RantaiClaw derives from:

- **Hermes** (`refs/hermes-agent`): one `AIAgent.run_conversation()` loop; surfaces
  feed it via callbacks; one cached prompt builder; channels auto-deny + ingress
  allowlist (`TELEGRAM_ALLOWED_USERS`); deterministic `build_session_key()`.
- **OpenClaw** (`refs/openclaw`): one `buildAgentSystemPrompt()`; channel plugins
  normalize into a canonical turn context; **pluggable per-surface approval
  adapters**; **four independent gates** (sender / command-owner / route /
  activation); explicit **pairing store** for cross-surface identity.

### Principle

> **Unify everything that defines agent capability or behavior.
> Keep surface-specific only: conversation identity, delivery formatting, and
> the approval *UI* (never the approval *authority*).**

```
Incoming message
   │
   ▼
Surface Adapter (Telegram/WhatsApp/Discord/Slack/TUI/Gateway)
   │  normalizes → AgentRequest { surface, user_id, conversation_id,
   │                              thread_id, workspace_id, message, capabilities }
   ▼
Conversation Resolver  (surface-specific id mapping; Hermes session-key scheme)
   ▼
Unified AgentRuntime  (ONE loop)
   ├─ Unified SystemPromptBuilder
   │     stable prefix: Persona · Identity · Tools · Safety/autonomy · Skills
   │     volatile/below cache boundary: Workspace · DateTime · Runtime · surface hints
   ├─ ToolRegistry        (same catalog everywhere)
   ├─ SecurityPolicy      (same enforcement everywhere)
   └─ ApprovalManager + ApprovalBackend (UI per surface; AUTHORITY separate)
   ▼
Surface Renderer (per-surface formatting)
```

### Approval model (the security-critical part)

Two **independent** concerns — today RantaiClaw conflates them:

1. **Mechanism (per surface, pluggable):**
   ```rust
   trait ApprovalBackend {
       async fn request(&self, req: &ApprovalRequest, ctx: &ApprovalContext) -> Decision;
   }
   ```
   - `CliApproval` — terminal Y/N/A (TUI)
   - `ChatRelayApproval` — post message, await reply (channels/gateway)
   - `WebModalApproval` — web-ui modal
   - `AutoDeny` — default when no owner is configured (secure-by-default)

2. **Authority (separate from the UI, defaults to deny):**
   ```rust
   fn can_approve(surface: Surface, approver_id: &str, owners: &OwnerList) -> bool
   ```
   `ChatRelayApproval` accepts a `Decision` **only** from a replier where
   `can_approve(..) == true`. The requester ≠ the approver. Arbitrary chat
   senders cannot approve. Mirrors OpenClaw's separate `commandOwnerAllowFrom`
   gate.

   Config (new, additive; unset ⇒ AutoDeny on that surface):
   ```toml
   [approval.owners]
   telegram = ["123456789"]
   discord  = ["98765..."]
   ```

### Memory scoping

Unify global/user/workspace memory; scope **conversation** memory per surface.
Cross-surface identity only via **explicit pairing**, never auto-merged.

Conversation id per surface (Hermes scheme):

| Surface | conversation_id |
|---|---|
| TUI / Web | selectable session id |
| Telegram | `chat_id` (+ `thread_id` for forum topics) |
| WhatsApp | `chat_id` / `group_id` |
| Discord | `guild_id:channel_id:thread_id` (DM: dm channel id) |
| Slack | `workspace_id:channel_id:thread_ts` |
| Gateway | `request.conversation_id` (create if absent) |

## 3. PR sequencing (each independently shippable + revertible)

| PR | Scope | Risk | Status |
|---|---|---|---|
| **PR1.0** | Persona parity on channels (shared `render_persona_section`) | Low | ✅ done (`c3ee0d4`) |
| **PR1.1** | Builder convergence — channels/gateway run the one `SystemPromptBuilder` | Low–Med | ✅ done (`77455e2`) |
| **PR3** | `ApprovalBackend` + owner-authority gate; remove `channel_name=="cli"` | High (security) | ✅ done (`71b1768`) |
| **PR3-relay** | `ChatRelayApprovalBackend` — in-chat owner `/approve` for whole-tool gating on polling channels | High (security) | ✅ done (`cd3ba8e`) |
| **PR3-webmodal** | `WebModalApprovalBackend` — in-browser approve/deny modal for the console SSE chat (`POST /api/v1/approvals/{id}`) | High (security) | ✅ done (`6f8649a`) |
| **PR3b-strict** | Strict shell-filter parity on channels | Med | ✅ done (`a2e634b`, `222d6ea`) |
| **PR3b-safety** | Channel-accurate safety/preset text (couples to approval) | Med | ✅ done (`39c17d4`) |
| **PR4-foundation** | `ConversationKey` (one tested conversation-id) | Low | ✅ done (`59df725`) |
| **PR4-memory-read** | `recall_layered` — conversation-scoped + global layering | Low | ✅ done (`d8c0478`) |
| **PR4-memory-loader** | Memory loader routes through `recall_layered` (conversation_id param) | Low | ✅ done (`34746b9`) |
| **PR4-memory-agent** | Agent read+write conversation scoping (builder `conversation_id`) | Low | ✅ done (`7e0986d`) |
| **PR4-memory-channels** | Conversation-scope channel recall/store via `ConversationKey` | Low | ✅ done (`d9b145c`) |

> **PR4-memory is complete** for every surface that recalls memory for context:
> Agent/TUI (`recall_layered` + scoped store), polling channels (`build_memory_context`
> via `recall_layered` + scoped store), and the gateway (its conversation context
> is `channel_approvals.history`, already keyed by `ConversationKey`). Cross-surface
> identity **pairing** remains a deliberately separate auth feature (plan §4
> non-goal: "pairing is explicit, never auto-merged"), not part of memory layering.

**All work items complete.** The two agent loops are collapsed into one
`run_structured_loop`; see the PR2-rest rows above.
| **PR2-step1** | Extract shared LLM-call + streaming/cancel core | Med | ✅ done (`001dd5b`) |
| **PR2-rest-a** | Unify `ParsedToolCall` + `ToolExecutionResult` types | Med | ✅ done (`ce4b7d3`) |
| **PR2-rest-b** | Shared tool executor (both loops use `execute_tool_calls_collecting`) | Med | ✅ done (`379ace8`,`91a226e`,`b7cb699`) |
| **PR2-rest-c** | Merge orchestration bodies over one history model (`run_structured_loop`) | High | ✅ done (`2dde1d3`,`9df2908`) |

> **PR2 is complete.** The two agent loops are collapsed into one
> `run_structured_loop` (ConversationMessage + dispatcher). `Agent::turn_inner`
> calls it directly (passing its structured history + dispatcher, no
> conversion); `run_tool_call_loop` is a thin adapter (wrap → run → flatten) so
> channels/gateway/CLI/delegate callers are unchanged.
>
> The flaky `sse_chat_emits_chunk_then_done` failure seen during the first
> attempt was diagnosed as a **pre-existing `XDG_DATA_HOME` test-isolation race**
> (two gateway tests clobber the process-global env var), not a fault in the
> transformation — fixed in `e598205` (`EnvVarGuard` now serializes env-mutating
> tests). With that fix, the transformation is stable: gateway suite 10/10, and
> agent/channels/memory/approval all green.

> **Note:** PR3 shipped before PR1.1/PR2 because it is the actual fix for the
> original report ("can't do X on Telegram") and is self-contained. The
> security invariant still holds: with no `approval_owners` configured, channels
> remain at the old auto-deny safety level.

**Safety invariant across all PRs:** channels never gain the ability to run an
approval-required tool *unless* an owner is explicitly configured AND explicitly
approves. PR1–PR2 stay at today's "auto-deny on channels" safety level; only PR3
adds an approval path, behind the owner gate.

### PR1 detail (in progress)

- Introduce a shared tool descriptor so both call sites feed one `ToolsSection`.
- Add surface-hint sections (`HardwareSection`, `TaskSection`,
  `ChannelCapabilitiesSection`) gated by a `Surface`/`SurfaceHints` input.
- Route `build_system_prompt_with_mode` and the gateway through
  `SystemPromptBuilder`, preserving the stable-prefix-then-volatile ordering for
  prompt-cache hit rate.

#### Slices

- **PR1.0 — persona parity (DONE, commit `c3ee0d4`).** Extracted
  `render_persona_section()` as a single source of truth; injected into the
  channel/gateway prompt. Text-only, decoupled from approval, tested.
- **PR1.1 — structural builder convergence (pending).** Route channels/gateway
  through one builder via a shared tool descriptor. This *changes channel prompt
  output* (full vs tz-only timestamp, tool schemas, section order), so it is a
  reviewable, outward-facing product change and will touch the ~30 channel
  prompt tests — not a free refactor.

#### Discoveries (refine the design)

1. **The Safety/approval-preset section moves to PR3, not PR1.** It is coupled
   to the approval model: the Strict text claims "shell is NOT registered",
   which is true on the TUI (`Agent::from_config` filters it at
   `src/agent/agent.rs:389`) but **false on channels** —
   `src/channels/mod.rs:2878` (`all_tools_with_runtime`) never applies that
   filter. And today every approval-required tool is auto-denied on channels
   regardless of preset, so preset text would mislead the model about what it
   can do. Port the Safety section only once PR3 gives channels a real,
   owner-gated approval path that honors the preset.
2. **Strict-mode shell filter is missing on channels** — a pre-existing
   read-only-policy gap (Strict is meant to remove `shell`, but channels keep
   it registered; it is auto-denied at the gate but still advertised). Fold the
   filter into PR3 so Strict means the same thing on every surface.

## 3.1 Remaining work — ready-to-execute specs

Shipped this pass: PR1.0 (`c3ee0d4`), PR3 (`71b1768`), PR3b-strict (`a2e634b`),
docs (`0d051b8`, `f6f3eb9`, `e45d8e7`), fmt (`1f9725f`). All compile, are tested,
and are rustfmt/clippy-clean. The pieces below are deliberately **not** rushed —
they are larger and (PR2 especially) touch every surface's agent path.

### PR1.1 — structural prompt-builder convergence (Low–Med)

Goal: one builder feeds every surface. Blocker: `SystemPromptBuilder` consumes
`&[Box<dyn Tool>]` (with schemas) but the channel path passes `&[(&str,&str)]`.

Steps:
1. Add `pub struct ToolDescriptor { name, description, schema: Option<Value> }`
   in `src/agent/prompt.rs`; change `PromptContext.tools` to `&[ToolDescriptor]`.
2. TUI builds descriptors from `Box<dyn Tool>` (schema `Some`); channels/gateway
   from `(name, desc)` (schema `None`). Real tool registry IS in scope at
   `src/channels/mod.rs:~2912` (`tools_registry`) if richer descriptors wanted.
3. Add surface-hint sections gated by a `Surface` field: `HardwareSection`,
   `TaskSection` (native vs xml), `ChannelCapabilitiesSection` — port verbatim
   from `build_system_prompt_with_mode`.
4. Route `build_system_prompt_with_mode` (`src/channels/mod.rs:1885`) and the
   gateway (`src/gateway/mod.rs:983`) through `SystemPromptBuilder`.
5. **Expect prompt-output changes** → update the ~30 channel prompt tests
   (`src/channels/mod.rs` `#[test] build_system_prompt_*`). This is an
   outward-facing change (every channel user's prompt) — diff-review it.

### PR3b-safety — Safety/autonomy-preset section on channels (Med) — ✅ done (`39c17d4`)

`SafetySection` is now surface-aware: the channel builder resolves the active
preset and `SafetySection` renders **channel-accurate** approval text for
Smart/Manual (owner `/approve` in chat, decline otherwise — not the TUI's
inline Y/N/A), while Strict/Off read the same on both surfaces. The shell
allowlist globs are deliberately not surfaced on channels (Layer-A gating
makes them moot). Folded into PR1.1's shared builder.

### PR3-relay — in-chat owner tool approval (High) — ✅ done (`cd3ba8e`)

Completes PR3's `ApprovalBackend` design. `ApprovalBackend::decide` is async;
`ChatRelayApprovalBackend` (channels) posts a pending tool call to the chat
and awaits an authorized owner's `/approve` / `/deny` via a dedicated
`PendingApprovals` registry (5-min auto-deny). `try_handle_tool_reply`
resolves it, owner-gated by `can_approve` (approve owner-only; deny anyone).
The backend is threaded through the unified loop as an optional
`&dyn ApprovalBackend` (`None` ⇒ name-derived default, so TUI/gateway/delegate
are behavior-preserving) and built per channel message only when gating is
active AND an owner is configured AND a reply target exists. With no
`approval_owners`, channels still auto-deny — no silent broadening.

### PR3-webmodal — in-browser tool approval (High) — ✅ done (`6f8649a`)

Fourth `ApprovalBackend` from PR3's design. The console chat runs the `Agent`
over an open SSE stream (`api_v1::agent_chat_stream`) and previously did no
Layer-A gating. Now `Agent` accepts an optional injected
`(ApprovalManager, ApprovalBackend)` (`set_approval`); the console handler sets
an `ApprovalManager` + `WebModalApprovalBackend`, gated off when
`autonomous_tools` is on. The backend emits `AgentEvent::ApprovalRequest`
(new variant; TUI no-op arm) over the SSE stream and awaits the client's reply
via a dedicated `PendingApprovals` registry on `AppState.web_approvals` (id in
the basename slot, 5-min auto-deny, fail-closed if the receiver is gone).
`POST /api/v1/approvals/{id}` (auth-gated) resolves the paused turn. TUI /
`agent run` are unchanged (both injections default to `None`). **All four
ApprovalBackends from PR3's design now exist** (CLI / AutoDeny / ChatRelay /
WebModal).

### PR2 — collapse the two agent loops (High, largest)

`Agent::turn_inner` (`src/agent/agent.rs:872`) and `run_tool_call_loop`
(`src/agent/loop_.rs:1302`) are independent loops with different feature sets
**and different history data models** — the blocker discovered while scoping
this: `Agent` iterates `Vec<ConversationMessage>` (tool metadata + streaming
events + `classify_model` + memory enrichment + `trim_history`), while
`run_tool_call_loop` iterates `Vec<ChatMessage>` with `ApprovalBackend` +
multimodal config. Collapsing them is not an extraction; it requires unifying
that representation across every surface (TUI streaming, channel approval,
gateway turn-based replay), so it is the one item that must be its own branch
slice with a full `./dev/ci.sh all`, not rushed alongside the others.
**Compiler-verified divergences** (each must be reconciled first; found by
attempting the merge): the loops use *separate* `ParsedToolCall` types
(`loop_::ParsedToolCall { name, arguments }` vs
`dispatcher::ParsedToolCall { name, arguments, tool_call_id }`, threaded through
~6 parse fns), separate tool-result types (`String` vs `ToolExecutionResult`),
separate history models (`ChatMessage` vs `ConversationMessage`), and per-quirk
behavior (success-always-true on the Agent path, parallel-with-events policy,
`ToolLoopCancelled` vs `Err(true)` cancellation). A shared executor won't even
compile until the `ParsedToolCall` types are unified first.

Recommended approach:
1. First unify the parse types (`ParsedToolCall`) and the history type
   (`ConversationMessage`); teach `run_tool_call_loop` to consume them — behind
   tests, no behavior change.
2. Extract the shared per-iteration core (LLM → parse → execute → feed),
   parameterized by `ApprovalBackend` (PR3) and prompt (PR1.1).
3. Migrate channels/gateway to drive the shared core, preserving their
   per-`(channel,sender)` history via `ConversationKey` (PR4-foundation).
4. Land per-surface tests; verify TUI streaming, channel owner-gated approval,
   and gateway replay each still work.

### PR4 — ConversationResolver + layered memory (Med)

1. `ConversationResolver`: map `(surface, sender, thread)` → conversation id
   (Hermes scheme `{surface}:{sender}[:{thread}]`). Replace ad-hoc
   `format!("{channel}:{sender}")` at `src/gateway/mod.rs:1102` and the channels
   `conversation_histories` keys; thread-aware so Discord/Slack threads scope
   separately. Plumb `thread_id` from channel adapters into `process_channel_chat`.
2. Memory layers global/user/workspace/conversation; cross-surface identity via
   explicit pairing only (never auto-merge). Additive over `src/memory/`.

## 4. Non-goals

- No new heavy dependencies.
- No silent broadening of permissions (CLAUDE.md §3.6).
- No mega-patch: each PR lands and reverts independently.
- No promise of cross-surface identity auto-linking (pairing is explicit).
