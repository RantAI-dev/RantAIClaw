# Plan 264: [claw-ui] Consistent API error handling and a typed config contract

> **REPO: claw-ui** (`/home/sulthannauval/project/rantai/claw-ui`) for the frontend parts; ONE small change in RantAIClaw (`src/gateway/config_api.rs`) for the error-detail leak. Each file is labeled below.
>
> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` (RantAIClaw) when done.
>
> **Drift check (run first)**: claw-ui `git diff --stat -- src/lib/api.ts src/hooks/use-async.ts src/components/ops/config-panel.tsx src/components/ops/providers-panel.tsx src/components/ops/tools-panel.tsx src/components/ops/mcp-panel.tsx src/app/api/rc/[...path]/route.ts` ; RantAIClaw `git diff --stat -- src/gateway/config_api.rs`

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug (frontend) + security (error-detail leak)
- **Planned at**: RantAIClaw commit `0e5fcc9`, 2026-08-27

## Why this matters

- **L6** `describeApiError` (built to distinguish 401 "log in again" from 502 "wait" from 400 "bad input") is bypassed by 5 config panels and by `useAsync` (which stores `e.message`) → an idle-timeout 401 shows as a bare "unauthorized" toast, and the operator retries instead of re-authenticating.
- **L7** Internal error detail (absolute `config.toml` path, gateway host:port) is relayed verbatim to the browser via `err_500` → BFF → raw toast/hint. Host filesystem layout + internal address disclosed to any console session.
- **L8** `api.config()` is untyped `Record<string,unknown>`, hand-cast at 5 consumers → a Rust-side rename compiles clean both sides and degrades silently at runtime (temperature blank, autonomy falls back to "smart", MCP badge 0).

## Current state (confirm before editing)

- claw-ui:
  - `src/lib/api.ts:52-77` — `describeApiError` doc states the problem it solves; `:327` — `config: () => rc<Record<string, unknown>>("config")` (the only untyped config accessor). `:93-97` prefers `detail` over `statusText`.
  - Still flattening to `.message`: `config-panel.tsx:33`, `providers-panel.tsx:70`, `tools-panel.tsx:57`, `mcp-panel.tsx:56,72`.
  - `src/hooks/use-async.ts:29-32` — catch stores `e.message`, discarding the `ApiError` before `PanelFrame` sees it → no panel's load/refresh error gets the mapping. Adopted correctly at `channels-panel.tsx:301`, `knowledge-settings-card.tsx:59`.
  - `src/app/api/rc/[...path]/route.ts:33-36` — the BFF relays the gateway body byte-for-byte; `:38-41` adds `detail: String(err.message)` (carries gateway host/port).
  - Casts: `tools-panel.tsx:17-21`, `mcp-panel.tsx:31`, `channels-panel.tsx:36`, `console-shell.tsx:308-312,377-381`, `config-panel.tsx:22`.
- RantAIClaw: `src/gateway/config_api.rs:85-90` — `err_500` puts `msg.to_string()` straight into the response `detail`; `:241-257` every config write funnels load/save failures through it (path leak).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Build (claw-ui) | `npx next build` | build succeeds, no type errors |
| Build (RantAIClaw) | `cargo build --lib` | exit 0 |
| Test (RantAIClaw) | `cargo test --lib gateway::config_api` | pass |
| Drive | agent-browser against the console | per Test plan |

No eslint config in claw-ui; verify via `next build` + browser drive.

## Scope

**In scope (claw-ui)**: `src/lib/api.ts` (type `config()`), `src/hooks/use-async.ts` (map errors), the 5 panels (use `describeApiError`), `src/lib/types.ts` (new `GatewayConfig` interface), `src/app/api/rc/[...path]/route.ts` (don't leak gateway host in the transport-error detail).
**In scope (RantAIClaw)**: `src/gateway/config_api.rs` (`err_500` logs detail, returns a stable non-specific message).
**Out of scope**: config-panel load states (plan 262); provider-secret editing (plan 263).

## Git workflow

- Branch (claw-ui): `fix/api-error-handling-and-types`
- Branch (RantAIClaw): `fix/config-api-error-detail-leak` (a small separate PR, or fold the one-file RantAIClaw change into plan 232's PR — coordinate; if folding, note it here and skip the RantAIClaw branch)
- Messages: claw-ui `fix: map API errors consistently and type the config contract`; RantAIClaw `fix(security): stop err_500 leaking filesystem paths to the browser`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (L6): route all errors through `describeApiError`

Change `use-async.ts:31` to `setError(describeApiError(e))` so every panel's load/refresh error inherits the mapping. Swap the five `e instanceof Error ? e.message : e` sites (config/providers/tools/mcp panels) for `describeApiError(e)`.

**Verify**: `npx next build` succeeds; drive: force a 401 (idle timeout) → the toast/panel says "sign in again", not a bare "unauthorized".

### Step 2 (L7): stop leaking internal detail

RantAIClaw `config_api.rs:85`: have `err_500` `tracing::error!` the detailed `msg` and return a stable, non-specific `detail` (plus an error code the console can map). claw-ui `route.ts:38`: the transport-error branch should return "the gateway is unreachable" (which `describeApiError` supplies from the 502), not the host/port string.

**Verify (RantAIClaw)**: Test-plan `err_500_does_not_leak_path` passes. **Verify (claw-ui)**: drive with the gateway down → the browser shows "gateway unreachable", no host:port.

### Step 3 (L8): type `api.config()`

Declare a `GatewayConfig` interface in `src/lib/types.ts` covering the fields the console reads (`default_temperature`, `default_provider`, `autonomy.*`, `mcp_servers`, …) and type `api.config()` with it. Update the 5 cast sites to use the typed shape. (The gateway derives `JsonSchema` on the config structs — generating the interface is viable; hand-writing the read subset is acceptable for this PR.)

**Verify**: `npx next build` succeeds with no type errors; the cast sites no longer use `as Record<string, unknown>`.

## Test plan

- RantAIClaw `gateway::config_api`: `err_500_does_not_leak_path` — an error carrying a filesystem path produces a response `detail` WITHOUT the path (assert the path marker is absent), while the path is still logged.
- claw-ui (via `next build` + drive): a 401 shows a re-auth hint (L6); a gateway-down state shows "unreachable" not host:port (L7); `api.config()` is typed (compile-time — a deliberate field typo fails `next build`).
- Verification: `cargo test --lib gateway::config_api` + `npx next build` → all pass.

## Done criteria

- [ ] `cargo build --lib` + `cargo test --lib gateway::config_api` pass; `err_500_does_not_leak_path` present
- [ ] `npx next build` (claw-ui) succeeds with no type errors
- [ ] `grep -rn "as Record<string, unknown>" src/components src/lib | grep config` (claw-ui) shows the config casts removed
- [ ] `use-async.ts` maps via `describeApiError` (`grep`)
- [ ] `git status` in each repo shows only in-scope files
- [ ] `plans/README.md` (RantAIClaw) row updated

## STOP conditions

- The RantAIClaw one-file change is being folded into plan 232 — do that instead of a second branch; note it and skip the RantAIClaw branch here.
- Typing `api.config()` surfaces a real shape mismatch (the console was reading a field the gateway doesn't send) — STOP and report; that's a real contract bug, not a typing exercise.

## Maintenance notes

- Reviewer: confirm a 401 now reads as "re-auth" and that no filesystem path reaches the browser (the `err_500` test).
- L8's typed contract turns future cross-repo drift into a `next build` failure — the highest-leverage part.
