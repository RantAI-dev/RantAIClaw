# Cross-Surface Agent Architecture (TUI vs Channels vs Gateway)

> **Status:** Reference / analysis snapshot. Describes *current* behavior as of
> branch `fix/update-allow-cargo-install`. Captures **why the agent behaves
> differently across surfaces** (TUI, `rantaiclaw channels` polling, gateway /
> web-ui, Telegram, WhatsApp, Discord, Slack, …).
>
> File:line references are anchors into the source at time of writing; verify
> before relying on exact line numbers.

## 1. TL;DR

A user message reaches the same agent loop on every surface, but two things
diverge based on **which surface dispatched the message**:

1. **Capability gating** — tools that require approval are *interactively
   approvable on the TUI* but *hard auto-denied on every non-CLI surface* (unless
   `autonomous_tools = true`).
2. **System prompt assembly** — the TUI builds the prompt through a different
   code path than channels/gateway, so it includes **Persona** and **Safety /
   autonomy** sections that channels never see.

The single discriminator behind divergence #1 is the `channel_name` string:
`"cli"` can approve; anything else cannot.

## 2. Surfaces and their entry points

| Surface | Entry point | Agent construction | `channel_name` |
|---|---|---|---|
| **TUI** (interactive) | `src/tui/app.rs:5354` `run_tui()` | `Agent::from_config()` (`src/agent/agent.rs:323`) | `"cli"` |
| **CLI** (`agent run`, one-shot) | `src/main.rs` → `src/agent/loop_.rs` `run()` | `Agent::from_config()` | `"cli"` |
| **Channels** (polling daemon) | `src/channels/mod.rs` `start_channels_with_cancellation()` (~`2790`) | `tools::all_tools_with_runtime()` + `run_tool_call_loop` | `"telegram"`, `"discord"`, `"slack"`, … |
| **Gateway / web-ui** (HTTP/webhook) | `src/gateway/mod.rs` request handler (~`981`) | shared `tools_registry` + `run_tool_call_loop` | `"webhook"` |

All paths ultimately call `run_tool_call_loop()` in `src/agent/loop_.rs:1302`.

## 3. Capability gating (the "works in TUI, not in Telegram" cause)

A tool action passes through up to **two stacked approval layers**.

### Layer A — whole-tool approval gate

Location: `src/agent/loop_.rs:1208-1266` (`execute_tools_sequential`).

```rust
let decision = if channel_name == "cli" {
    mgr.prompt_cli(&request)        // TUI: interactive Y/N/A prompt
} else {
    ApprovalResponse::No            // every non-CLI surface: HARD AUTO-DENY
};
```

- Applies to any tool where `ApprovalManager::needs_approval(name)` is `true`
  (i.e. the tool is **not** in the autonomy `auto_approve` list).
- On non-CLI surfaces the denial message is (`loop_.rs:1255-1263`):
  > `Tool '<name>' denied: requires approval at current autonomy level. Ask your
  > supervisor to promote your autonomy level to use this tool.`
- Approval-gated tool batches are forced **sequential** so each can be gated
  individually (`should_execute_tools_in_parallel`, `loop_.rs:1160-1177`).

**Whether Layer A is active at all** is decided when channels start
(`src/channels/mod.rs:3260-3271`):

```rust
channel_approval: if config.channels_config.autonomous_tools {
    None                                       // gate OFF → unattended tool use
} else {
    Some(ApprovalManager::from_config(&config.autonomy))  // DEFAULT → gate ON
}
```

The gateway mirrors this (`src/gateway/mod.rs:1021-1035`), passing
`channel_name = "webhook"`.

> **Default config (`autonomous_tools = false`) is exactly the condition that
> makes channels auto-deny what the TUI would prompt for.**

### Layer B — per-command shell allowlist

Location: `src/tools/shell.rs:104-179` (cascading approval loop).

Even when the `shell` tool itself runs, each command basename is validated
against the security allowlist. On an allowlist miss it calls:

```rust
let decision = approvals
    .request_decision(basename.clone(), command.to_string(), "")
    .await;
```

The behavior of `request_decision` depends on the surface:

| Surface | `PendingApprovals` behavior |
|---|---|
| **TUI** | Waits **indefinitely** for `/allow` — `PendingApprovals::default()` has no timeout (`src/security/pending.rs:217`). |
| **Gateway chat channels** (WhatsApp / Linq / Nextcloud Talk) | In-chat turn-based approval: bot replies "reply Y / A / N" (`src/gateway/channel_approval.rs`). |
| **Plain `rantaiclaw channels` polling** (Telegram/Discord daemon) | No approval relay wired → cannot be satisfied → effectively denied / times out. |

Hard blocks (high-risk commands, redirects, subshell expansion — no single
basename to approve) return an error immediately on all surfaces
(`shell.rs:128-143`).

### Layer interaction

- If a tool is gated at **Layer A** on a channel, it is auto-denied **before**
  reaching Layer B.
- `autonomous_tools = true` disables Layer A but **not** Layer B — `shell` still
  enforces its own command allowlist via `SecurityPolicy`.

## 4. System prompt divergence

The TUI and channels/gateway assemble the system prompt through **two different
builders that emit different sections**.

### Path A — TUI (`Agent` struct)

`src/agent/agent.rs:698-729` (`build_system_prompt`) →
`src/agent/prompt.rs:44-62` (`SystemPromptBuilder::with_defaults`):

```
PersonaSection      → persona.toml (active profile)
IdentitySection     → SOUL.md / AGENTS.md / … or AIEOS identity
ToolsSection        → registered tool list + dispatcher protocol
SafetySection       → autonomy preset (Strict/Smart/Manual/Off) + allowed commands
SkillsSection       → available skills
WorkspaceSection    → working directory
DateTimeSection     → current local date/time/timezone
RuntimeSection      → host, OS, model
```

### Path B — Channels / Gateway

`src/channels/mod.rs` `build_system_prompt_with_mode()` (~`1885-2041`);
gateway uses `crate::channels::build_system_prompt()` (`src/gateway/mod.rs:981-994`).
Per-message wrapper `build_channel_system_prompt()` (`src/channels/mod.rs:288-298`)
adds channel-specific delivery hints.

### Section comparison

| Section | TUI (Path A) | Channels / Gateway (Path B) |
|---|---|---|
| Persona (`persona.toml`) | ✅ | ❌ |
| **Safety / autonomy preset + allowed-command list** | ✅ | ❌ |
| Identity (bootstrap files / AIEOS) | ✅ | ✅ |
| Tools list + protocol | ✅ | ✅ |
| Skills | ✅ | ✅ |
| Workspace / DateTime / Runtime | ✅ | ✅ |
| Channel delivery hints (e.g. Telegram media markers) | ❌ | ✅ (Telegram only) |

**Consequence:** on channels the model receives neither the persona nor the
"here is what you are allowed to run" guidance, so it behaves differently *and*
only discovers the approval wall by hitting it.

## 5. What is the *same* across all surfaces

- **Tool registry**: every surface registers the same set via
  `tools::all_tools_with_runtime()`. Tools are not added/removed per surface
  (one exception: `shell` is dropped when the **Strict** autonomy preset is
  active — `src/agent/agent.rs:376-396`).
- **Security policy enforcement**: `SecurityPolicy` (forbidden paths, rate
  limits, command validation) applies everywhere.
- **Identity / workspace files**: loaded from the same workspace directory.

The difference is **execution approval** and **prompt assembly**, not the tool
catalog.

## 6. Data-flow diagram

```
incoming message
      │
      ├── TUI ─────────────► Agent::from_config ──► SystemPromptBuilder (Persona+Safety)
      │                                              channel_name = "cli"
      │                                              Layer A: prompt_cli (interactive)
      │                                              Layer B: wait indefinitely for /allow
      │
      ├── channels (poll) ─► build_system_prompt_with_mode (no Persona/Safety)
      │                       channel_name = "telegram" | "discord" | …
      │                       Layer A: AUTO-DENY (unless autonomous_tools)
      │                       Layer B: no relay → cannot approve
      │
      └── gateway/web-ui ──► channels::build_system_prompt (no Persona/Safety)
                              channel_name = "webhook"
                              Layer A: AUTO-DENY (unless autonomous_tools)
                              Layer B: in-chat Y/A/N on supported channels
                                       (gateway/channel_approval.rs)
```

## 7. Config levers

| Lever | Location | Effect |
|---|---|---|
| `[channels_config] autonomous_tools` | `src/config/schema.rs` | `true` disables Layer A on channels/gateway (Layer B still enforced for shell). |
| Autonomy preset (Strict/Smart/Manual/Off) | profile `policy/` + `[autonomy]` | Controls what `needs_approval` returns and the Safety section text. |
| `[autonomy] auto_approve` | `src/config/schema.rs` | Per-tool allowlist; tools listed here skip Layer A. |
| `[agents.<name>] system_prompt` | `src/config/schema.rs` (`DelegateAgentConfig`) | Full system-prompt override for delegate sub-agents. |

## 8. Key source references

| Concern | File:line |
|---|---|
| Tool approval gate (`cli` vs auto-deny) | `src/agent/loop_.rs:1208-1266` |
| Denial messages | `src/agent/loop_.rs:1255-1263` |
| Sequential-vs-parallel gating | `src/agent/loop_.rs:1160-1177` |
| Tool-call loop signature (`channel_name`) | `src/agent/loop_.rs:1302-1318` |
| Channel approval-manager creation | `src/channels/mod.rs:3260-3271` |
| Channel prompt builder | `src/channels/mod.rs:~1885-2041` |
| Channel per-message prompt wrapper | `src/channels/mod.rs:288-298, 1442` |
| Gateway prompt + approval | `src/gateway/mod.rs:981-994, 1021-1035` |
| Gateway in-chat approval flow | `src/gateway/channel_approval.rs` |
| TUI agent construction | `src/tui/app.rs:5354`, `src/agent/agent.rs:323-490` |
| TUI prompt builder | `src/agent/agent.rs:698-729`, `src/agent/prompt.rs:44-333` |
| Shell per-command approval | `src/tools/shell.rs:104-179` |
| Pending-approval timeout semantics | `src/security/pending.rs:217` |
| Strict-mode shell removal | `src/agent/agent.rs:376-396` |
