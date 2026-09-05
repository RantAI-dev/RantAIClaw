# Plan 311: The `/tasks` surface — promote it properly, or stop serving it by default

> **Executor instructions**: this plan asks for a decision with evidence, then executes it.
> Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/gateway/mod.rs src/gateway/task_handlers.rs src/tasks/ src/config/schema.rs`

## Status

- **Priority**: P2 (ledger W2-6) · **Effort**: M · **Risk**: LOW
- **Category**: security posture / contract
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

Nine `/tasks*` routes are registered on the gateway's root router, enabled by default, and
they are outside everything that governs the rest of the API. They sit outside `/api/v1`, so
they miss the `api_rate_limit` layer applied to the other routers. They appear in no
documentation — not `api-v1.md`, not `config.md`, not `commands.md`. They have no tests. And
their handlers echo raw error strings, which includes the absolute path of `tasks.db` — the
same information-disclosure class that was fixed elsewhere.

It is an authenticated surface, so this is not an open door. It is an unreviewed one that the
project does not describe to itself.

## Steps

1. **Establish whether anything uses it.** The agent-side task tools are real consumers; the
   HTTP routes may have none — claw-ui does not call them, and the kanban PR that would have
   was never merged. Check both repos and say so in the PR body. That answer decides the rest.
2. **If nothing consumes the HTTP routes**: default `tasks.enabled` to `false` and document
   the flag. The agent tools keep working; the unreviewed surface stops being served to every
   install by default.
3. **If something does consume them**: move the routes under `/api/v1`, inside the rate
   limiter, document all nine in `api-v1.md` (its own maintenance rule requires it), and add
   the routed integration test that does not exist.
4. **Either way, stop leaking paths.** Route the handlers' errors through the redacting helper
   the other routers use.
   **Verify**: a forced failure returns no filesystem path.
5. **Say which it is in `config.md`.** A default-on surface with no documentation is the part
   that made this a finding.

## Done criteria

- `/tasks*` is either off by default or fully inside the `/api/v1` contract with tests.
- No handler returns a filesystem path.
- `docs/reference/config.md` documents `tasks.enabled` and its default.

## STOP conditions

- Moving the routes would break an external consumer → STOP; then it is a deprecation, and the
  old paths need a release of overlap.

## Maintenance note

The rule this restores: a route served by default is part of the public contract and is
documented, rate-limited and tested like the rest of it.

## Rollback

One commit. If the default flips to off, note it in the CHANGELOG as a behaviour change.
