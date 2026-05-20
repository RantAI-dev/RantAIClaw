# Pillar 3 — Tools, Approvals, and Security

> **ClickUp:** [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) → Setup: Approvals + Resilience: Audit log · **Maturity:** App-layer stable · **Modules:** `src/tools/`, `src/approval/`, `src/security/`

The blast radius surface. Every tool call routes through one approval gate before execution. Application-layer security is already strong (allowlist, injection blocking, path traversal protection, rate limiting, basic audit log). OS-layer containment is out of scope for v0.6.0.

## What this pillar covers

- Built-in tools (shell, file, memory, browser, hardware, MCP-bridged)
- Approval gate (Manual / Smart / Strict / Off presets, per-agent overrides)
- v0.6.50 Smart/Strict UX overhaul (inline Y/A/N prompt, plan-mode Strict, cascading approvals)
- Command allowlist + path traversal + injection blocking
- Rate limiting (default 20 actions/hour)
- Basic audit log under `<profile>/audit/`

## v0.6.50 approval UX (Claude Code parity)

When the agent attempts a shell command not on the active preset's allowlist:

1. **Boxed inline prompt** replaces the input row — amber border, command preview, action chips (`[Y]` yes once · `[A]` always (persist) · `[N]` no · `[Esc]` deny). Single keypress resolves.
2. **No auto-deny timeout** — the prompt sits indefinitely until you act. Matches CC's pause semantics; the LLM is genuinely frozen while waiting.
3. **Deny cancels the entire turn**, not just the tool call. Stops the LLM from trying alternative commands behind your back.
4. **Cascading approvals** walk `&&` chains — approving `cd` then re-prompts for the next blocking basename (e.g. `python3`), capped at 6 prompts per call.
5. **Strict preset = plan mode.** The `shell` tool is dropped from the model's tool list entirely. The agent describes commands instead of attempting them. CC plan-mode analog.
6. **Preset switching is live.** `Shift+Tab` cycles in the TUI; the runtime rebuilds the `SecurityPolicy` on each switch and the TUI re-subscribes to the fresh `PendingApprovals` broadcast (no more silent dropped approvals after a switch).
7. **The bundle is now the source of truth.** `<policy_dir>/command_allowlist.toml` patterns are bridged into `config.autonomy.allowed_commands` at preset-apply time; the runtime gate reads the bridged list (previously the bundle was write-only).

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Approval gate (single chokepoint) | ✅ | ✅ | TBD |
| Manual / Smart / Strict / Off policy presets | ✅ | TBD | TBD |
| Command allowlist (deny-by-default) | ✅ | TBD | TBD |
| Injection blocking (`$()`, backticks, `&&`, `>`) | ✅ | TBD | TBD |
| Path traversal protection | ✅ | TBD | TBD |
| Basic audit log | ✅ | TBD | TBD |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Approval gate (single path between LLM and execution) | Stable |
| Manual / Smart / Strict / Off presets + per-agent overrides | Stable |
| Command allowlist | Stable |
| Path-traversal + injection blocking | Stable |
| Rate limiting | Stable |
| Channel allowlist (deny-by-default) | Stable |
| Audit log (basic) | Stable · v0.6 Resilience test verifies it survives restart + corruption |

## Architecture

```
LLM tool call
  ↓
src/approval/      ← policy gate (autonomy + allowlist + forbidden_paths)
  ↓ approve
src/tools/         ← Tool trait dispatch
  ↓ execute
src/security/      ← rate limit + audit
  ↓ result
<profile>/audit/   ← basic audit log
```

## Trait extension point

- `Tool` — `src/tools/traits.rs`
- Strict parameter schema; validate + sanitize all inputs
- Return structured `ToolResult`; never panic in runtime path
- Risk classification: Low / Medium / High

## CLI / config

```bash
rantaiclaw setup approvals              # pick Manual / Smart / Strict / Off preset (wizard)
rantaiclaw autonomy                     # print active preset + 4 options
rantaiclaw autonomy <preset>            # switch (manual | smart | strict | off | full)
```

Inside the TUI:
- `Shift+Tab` — cycle Manual → Smart → Strict → Off → Manual
- `/autonomy` — interactive picker; `/autonomy <preset>` for direct switch
- `[Y]` / `[A]` / `[N]` / `[Esc]` — resolve the inline approval prompt
- `/allow <basename> [--persist]` — fallback for non-TUI channels (Telegram, webhook)

```toml
[autonomy]
level = "supervised"  # readonly | supervised | full
allowed_commands = ["git", "ls", "cat", "grep", "find"]
forbidden_paths = ["/etc", "/root", "~/.ssh"]
require_approval_for_medium_risk = true
block_high_risk_commands = true
max_actions_per_hour = 20
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Setup: Approvals validates the four preset paths (Manual / Smart / Strict / Off); Resilience: Audit log verifies the v0.5.0 basic audit log survives restart + corruption
