# Pillar 3 — Tools, Approvals, and Security

> **ClickUp:** [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) → Setup: Approvals + Resilience: Audit log · **Maturity:** App-layer stable · **Modules:** `src/tools/`, `src/approval/`, `src/security/`

The blast radius surface. Every tool call routes through one approval gate before execution. Application-layer security is already strong (allowlist, injection blocking, path traversal protection, rate limiting, basic audit log). OS-layer containment is out of scope for v0.6.0.

## What this pillar covers

- Built-in tools (shell, file, memory, browser, hardware, MCP-bridged)
- Approval gate (L1 strict → L4 auto presets, per-agent overrides)
- Command allowlist + path traversal + injection blocking
- Rate limiting (default 20 actions/hour)
- Basic audit log under `<profile>/audit/`

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Approval gate (single chokepoint) | ✅ | ✅ | TBD |
| L1-L4 policy presets | ✅ | TBD | TBD |
| Command allowlist (deny-by-default) | ✅ | TBD | TBD |
| Injection blocking (`$()`, backticks, `&&`, `>`) | ✅ | TBD | TBD |
| Path traversal protection | ✅ | TBD | TBD |
| Basic audit log | ✅ | TBD | TBD |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Approval gate (single path between LLM and execution) | Stable |
| L1-L4 presets + per-agent overrides | Stable |
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
rantaiclaw setup approvals              # pick L1-L4 preset
```

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

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Setup: Approvals validates L1-L4 path coverage; Resilience: Audit log verifies the v0.5.0 basic audit log survives restart + corruption
