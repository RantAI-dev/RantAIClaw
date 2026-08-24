# ADR 0001 — The `/api/v1` surface is the gateway ↔ console contract

- **Status:** Accepted
- **Date:** 2026-08-24
- **Context:** RantaiClaw (this repo) and the web console ([claw-ui](https://github.com/RantAI-dev/claw-ui)) are separate repositories that ship on independent release cadences.

## Decision

The gateway's `/api/v1/*` JSON API is the **only** interface between the two
repositories. The console never reaches into runtime internals; it consumes the
documented HTTP surface, and nothing else.

Consequently:

1. **A change to a documented `/api/v1` request or response shape is a breaking
   change.** It must update `docs/reference/api-v1.md` (and
   `api-v1-streaming.md` for SSE events) **and** the claw-ui TypeScript types in
   the same change set, or an announced deprecation window.
2. **Additive fields are safe.** New optional request fields (`#[serde(default)]`)
   and new response fields are non-breaking; an older console ignores what it
   does not read, and the gateway ignores unknown request keys.
3. **The console is loopback-by-default and login-optional.** The gateway binds
   to `127.0.0.1` unless the operator configures a tunnel or sets
   `[gateway] allow_public_bind = true`; the Argon2id console login
   (`[gateway.login]`) is off by default. Network reachability, not the login
   gate, is the primary access boundary — so an exposed bind with login off
   emits a startup warning.

## Why

This contract is what let a field like `StatusInfo.autonomy_preset` drift between
the two repos undetected before it was written down: without a stated interface,
either side could change a shape and only discover the mismatch at runtime in the
browser. Naming the `/api/v1` surface as the contract makes the coupling explicit
and puts the doc + types update on the same PR as the code change.

## Consequences

- Reviewers of a gateway PR that touches an `/api/v1` handler check whether a
  response shape changed and whether the reference doc + claw-ui types moved with
  it.
- A console feature that needs new runtime data adds an `/api/v1` field or route
  here first; it does not reach around the API.
