# Plan 303: Make every accepted config key either do something or stop being accepted

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/config/schema.rs src/cron/ src/persona/ src/providers/reliable.rs`

## Status

- **Priority**: P2 (ledger W2-1, part b) · **Effort**: M · **Risk**: MED (schema bump)
- **Category**: tech-debt / contract
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

A config key the runtime accepts is a promise. Several keys are accepted, stored, sometimes
displayed back — and read by nothing. An operator who sets one gets silence, not an error, and
reasonably concludes it took effect.

The audit found these, and each needs the same binary decision: implement it, or stop
accepting it. Leaving them is the option that keeps lying.

## Current state (verified at `bf77d26`)

Candidates, each to be re-confirmed by the executor before acting:

| Key | Symptom |
|---|---|
| `cron` `session_target: "main"` | accepted and advertised in the tool schema; the scheduler's match arm treats `Main` and `Isolated` identically |
| `persona.always_on_kbs` | stored and served over the API; no server-side reader (the console applies it client-side) |
| `reliability.api_keys` | rotation logs "rotated API key" and applies nothing |
| `memory.chunk_max_tokens` | no reader; still written by setup |
| `multimodal.max_images`, `sign_events`, `max_memory_mb` | no readers |
| `[security.*]` | `SecurityConfig` is not a field of `Config`, so the whole section is an unknown top-level key |

`[security.*]` is deliberately out of scope here — it belongs to plan 305, which decides the
fate of the layer it configures.

## Steps

1. **Re-derive the list, do not trust the table.** For each candidate, `rg` for readers of the
   field, excluding tests and the code that merely stores or serialises it. Produce the
   confirmed list in the PR description. A key with a real reader drops out.
2. **Decide per key and say why.** Default to removal — YAGNI (§3.2) and the repo's own rule
   that unsupported paths should error rather than pretend. Two likely exceptions worth
   implementing instead: `session_target: "main"`, which has an obvious meaning a user asked
   for, and `always_on_kbs`, which already works client-side and only needs the server half.
3. **Remove accepted-but-dead keys with a migration**, following the pattern PR #695 used:
   schema bump plus a migrate arm that drops the key, and regenerated snapshots. Removing a
   key silently would break configs that contain it.
4. **For anything implemented instead of removed**, add the test that proves the behaviour it
   promises — for `session_target: "main"`, that a "main" job actually shares session context.
5. **Stop setup writing a key that has no reader** (`memory.chunk_max_tokens`).

## Done criteria

- `cargo test --test schema_drift`, `cargo test --lib config`, `cargo test --lib cron` pass.
- Every key in the confirmed list is either removed with a migration or covered by a test that
  fails when its behaviour is reverted.
- `docs/reference/config.md` matches the resulting set.

## STOP conditions

- A key is read only by claw-ui or another external consumer → STOP for that key; removing it
  is a cross-repo contract change.
- The list grows beyond roughly eight keys → STOP and split; a schema bump touching many
  unrelated keys is hard to review and harder to roll back.

## Maintenance note

CI has an advisory check for config keys with no runtime reader. Once this lands, consider
making it blocking — an advisory gate is how this set accumulated.

## Rollback

Carries a schema bump: a rollback needs the config migrated back. State it in the PR body.
