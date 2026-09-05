# Plan 312: Decide where MCP supervision lives, and whether channels and cron get MCP tools

> **Executor instructions**: this plan asks for a decision with evidence, then executes it.
> Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/mcp/ src/channels/ src/cron/ src/agent/agent.rs`

## Status

- **Priority**: P1 (ledger W2-1, part f) · **Effort**: L · **Risk**: MED
- **Category**: architecture / direction
- **Planned at**: commit `bf77d26`, 2026-09-05
- **Closes**: issues #282 and #283, open since 2026-07-20

## Why this matters

Two related gaps, and the audit's note that they must be decided together still holds: where
supervision lives determines how other runtimes consume it.

**#282 — supervision is dead code.** `McpRegistry`, `McpHandle` and `spawn_supervisor` have no
callers outside `src/mcp/`; 531 lines describing respawn-with-backoff that never runs. The
module doc claims crash recovery that does not exist. Plan 287 gave the *gateway* a pooled
lifetime, which is real progress and also means the supervision question now has a natural
home rather than an abstract one.

**#283 — channels and cron never get MCP tools.** `grep -ril mcp src/channels src/cron`
returns nothing. An operator who configures an MCP server reasonably expects it everywhere;
instead capability depends silently on which surface you reach the agent through, with nothing
in the logs.

## Steps

1. **Decide supervision first, and write the reasoning down.** Options: extend the gateway
   pool from plan 287 to own restart-with-backoff and make that the single lifetime owner; or
   delete the registry/handle/supervisor stack and document one-shot spawn as the contract.
   Deleting is legitimate and cheap; what is not legitimate is keeping 531 lines that claim
   recovery. Note the audit's detail if wiring: the failure counter gives up on the fifth
   crash while the log says "attempt n/5".
2. **Then decide reach, because it follows from step 1.** Channels and cron assemble their
   tool list from the tool factory, not from `Agent::from_config`. Giving them MCP means one
   of: they borrow the supervised pool (natural if step 1 wires it), or MCP stays a
   TUI/gateway capability and that limitation becomes documented rather than silent.
3. **Whichever way, make the limitation visible.** If channels do not get MCP tools, `doctor`
   and the config docs should say so. The current failure mode is silence — the tools simply
   are not offered, with no error.
4. **Do not let this expand into per-channel MCP lifecycles.** If several channels each spawn
   servers, cost and process management multiply. A shared pool with one owner is the only
   shape worth building.
5. **Tests follow the decision.** If supervision is wired: a fake server that dies is
   respawned, and gives up when it should, with the log matching the behaviour. If deleted:
   nothing to test, but the module doc must stop claiming recovery.

## Done criteria

- No code claims MCP crash recovery that does not exist.
- Channel and cron MCP availability is either implemented or documented — not silent.
- Issues #282 and #283 can both be closed with a link to the decision.

## STOP conditions

- Wiring supervision would make an MCP server restart while a tool call is in flight, with no
  defined behaviour for that call → STOP and define it first.
- The decision would give channels their own subprocess lifecycles → STOP; see step 4.

## Maintenance note

This is the second time an MCP lifetime decision has been deferred. Whatever is chosen belongs
in the `src/mcp` module doc, which currently describes a supervisor that never runs.

## Rollback

If supervision is wired, it reverts to the pooled one-shot behaviour plan 287 established —
which is a working state, not a broken one.
