# Pillar 10 — Web Console

> **ClickUp:** _task TBD_ · **Maturity:** Beta · **Modules:** `src/gateway/api_v1.rs`, `src/webui.rs`, and the separate [claw-ui](https://github.com/RantAI-dev/claw-ui) repository

The browser surface. The web console is a **separate Next.js app** (claw-ui),
deliberately not bundled into the binary. The CLI fetches a pinned release, refuses
to install it unless its cosign signature verifies (`--allow-unverified` is the only
way past that, and says so), and runs it as its own process; the console talks to the runtime only
through the gateway's `/api/v1` HTTP surface.

## What this pillar covers

- The console app and how it is installed/run (`rantaiclaw ui install` / `ui start`)
- The gateway ↔ console contract: the `/api/v1` JSON API is the only interface
  between the two repositories (see the ADR below)
- The exposure model: the gateway binds to `127.0.0.1` by default and refuses a
  public bind without an explicit `[gateway] allow_public_bind` — a configured
  tunnel is not a substitute, since every provider proxies loopback
- The optional Argon2id console login (`[gateway.login]`), off by default
- The chat surface (sessions, streaming, in-browser tool approvals), the Status
  panel (`/status`, `/doctor`, `/insights`), and the Persona editor
  (`/personality`, `/personality/presets`)

## Boundaries and safety

- **Loopback by default.** The console is only reachable from the same host
  unless the operator opts into a tunnel or a public bind. When the gateway is
  reachable beyond localhost *and* no console login is configured, it prints a
  startup warning: anyone who can reach the port can drive the agent.
- **Server-side credentials.** The gateway bearer token is held by the console's
  server-side BFF; the browser holds only a signed session cookie. The browser
  never talks to the gateway directly.
- **The `/api/v1` responses are a cross-repo contract.** A change to a documented
  response shape is breaking and must update `docs/reference/api-v1.md` and the
  claw-ui TypeScript types in lockstep — see
  [ADR 0001](../contributing/adr/0001-gateway-console-api-contract.md).

## Current state by maturity

| Area | State |
|---|---|
| Chat (sessions, streaming, approvals) | Stable |
| Status / doctor / insights | Stable |
| Persona editor | Beta |
| Session fork / export | Beta (fork route shipped; export is a client feature) |
| Login gate (Argon2id) | Stable (optional, off by default) |

## Reference

- `docs/reference/api-v1.md`, `docs/reference/api-v1-streaming.md` — the contract
- `docs/contributing/adr/0001-gateway-console-api-contract.md` — the cross-repo interface decision
- [claw-ui](https://github.com/RantAI-dev/claw-ui) — the console repository
