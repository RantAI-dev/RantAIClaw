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

| PR | Scope | Risk | Capability change? |
|---|---|---|---|
| **PR1** | Unify the system prompt builder (one builder + surface-hint sections; persona/safety parity on channels) | Low | **No** — text only |
| **PR2** | Collapse the two agent loops into one runtime | High (structural) | No (behavior-preserving) |
| **PR3** | `ApprovalBackend` trait + owner-authority gate; remove `channel_name=="cli"` | High (security) | **Yes** — gated behind owner allowlist, AutoDeny default |
| **PR4** | `ConversationResolver` + layered memory | Medium | No |

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
- **Caveat to handle:** the Strict-preset Safety text says "shell is NOT
  registered" — true on TUI (filtered in `Agent::from_config`) but channels do
  not currently apply that filter. PR1 must reconcile shell-registration parity
  before emitting preset text on channels, or scope the preset text accurately.

## 4. Non-goals

- No new heavy dependencies.
- No silent broadening of permissions (CLAUDE.md §3.6).
- No mega-patch: each PR lands and reverts independently.
- No promise of cross-surface identity auto-linking (pairing is explicit).
