# Plan 305: Decide the three dead enforcement layers — sandbox, audit trail, resource limits

> **Executor instructions**: this plan asks for a decision with evidence, then executes it.
> Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/security/ src/config/schema.rs README.md docs/pillars/3-tools-approvals.md`

## Status

- **Priority**: P1 (ledger W2-1, part d) · **Effort**: L (wire) / M (delete) · **Risk**: HIGH if wired
- **Category**: security / direction
- **Planned at**: commit `bf77d26`, 2026-09-05
- **Supersedes**: the deferred spikes 215 and 218

## Why this matters

Three controls exist as code, are configurable, and enforce nothing.

`create_sandbox` and its Landlock, bubblewrap, firejail and Docker backends have no production
caller, so `[security.sandbox] backend = …` is a no-op. `AuditLogger` records config-API
changes only — `log_command` and `log_command_event` have zero callers, so no tool execution
is ever audited. `setrlimit` does not appear in the source at all.

The configuration for all of it cannot even be loaded: `SecurityConfig` is not a field of
`Config`, so `[security.*]` is an unknown top-level key. An operator writes a security policy
into a section the parser discards.

The README is honest about this; **pillar 3 is not** — it lists "Audit log (basic)" as Stable
and cites a resilience test that does not exist. That combination is the dangerous one: an
evaluator reading the maturity table concludes there is a tool-execution audit trail.

## Steps

1. **Decide each of the three separately.** They share a config section but not a fate.
   Recommended defaults, all cheap and all honest:
   - **Sandbox**: delete the layer, or keep exactly one backend and wire it. Do not keep four
     unwired backends. Note `[runtime].kind` (native/docker) is the confinement that does work
     today — if sandbox goes, say so where sandbox was documented.
   - **Audit**: wire `log_command` at the single approval chokepoint. This is the cheapest of
     the three and buys the most: one call site, and the trail operators already believe exists.
   - **Resource limits**: delete the config surface. Nothing has ever implemented it.
2. **Whatever survives must be reachable from config.** If any of it stays, `SecurityConfig`
   becomes a real field of `Config` with a schema bump and a migration; if none stays, remove
   the section and the types so an operator gets an unknown-key warning instead of silence.
   **Verify**: writing `[security.sandbox] backend = "bubblewrap"` either takes effect or
   produces a warning. Silence is the one outcome this plan must eliminate.
3. **Fix pillar 3 in the same PR**, whichever way the decisions go. Its "Stable" rows and its
   architecture diagram both claim the audit trail.
4. **If audit is wired**, add the test the doc already claims: the log survives a restart and a
   truncated file. Do not restore the claim without the test.
5. **If code is deleted**, delete its tests with it. Roughly 20 tests exercise the sandbox
   backends and prove nothing about the product.

## Done criteria

- No configurable security control silently does nothing.
- Pillar 3, the README and the code agree.
- `cargo test --lib security` passes; if audit was wired, the restart/corruption test is real.

## STOP conditions

- Wiring the sandbox would change what existing tool calls are allowed to do → STOP; that is a
  behaviour change needing its own release note and a staged rollout, not a spike outcome.
- Any decision here would widen an exposure boundary → STOP (CLAUDE.md §10).

## Maintenance note

The failure mode to prevent recurring: a control that is configurable before it is wired. If a
future security layer lands, its config key and its enforcement belong in the same PR.

## Rollback

Deletions revert from history. If audit is wired, it only writes a log — reverting cannot
break a working install.
