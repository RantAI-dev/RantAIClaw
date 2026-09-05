# Plan 306: Make token usage real, then make the cost cap mean something

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/agent/ src/providers/traits.rs src/cost/ src/security/policy.rs`

## Status

- **Priority**: P1 (ledger W2-1 part e + W2-4 half) · **Effort**: L · **Risk**: MED
- **Category**: bug / feature
- **Planned at**: commit `bf77d26`, 2026-09-05
- **Supersedes**: the deferred spike 217

## Why this matters

Every token count in the product is a hardcoded zero. `empty_usage` fills the field the agent
loop emits, no provider parses the `usage` block that every major API returns, and
`ChatResponse` has nowhere to put it. The TUI renders those zeros as though they were data.

`CostTracker` exists and is never constructed. `max_cost_per_day_cents` is stored, displayed
by `status` as a security setting, and enforced nowhere — while the daemon's heartbeat runs an
agent turn per task per tick, unattended, with no ceiling. A runaway loop has no brake and no
counter.

This is the largest single gap between what the product reports and what it does.

## Steps

1. **Add usage to the provider contract.** `ChatResponse` gains an optional usage field.
   Populate it from each backend that returns one — start with the OpenAI-compatible client
   and the Rig path, which together cover most providers, and leave the rest returning `None`
   honestly rather than zero.
   **Verify**: `None` and `Some(0)` must be distinguishable downstream; a zero that means
   "unknown" is how this started.
2. **Thread it through the loop.** The `Usage` event carries real numbers, and
   `AgentEnd { tokens_used, cost_usd }` stops being `None` where the provider reported them.
   Remove `empty_usage`.
   **Verify**: the double-`Usage`-per-turn emission the audit found (loop and agent both emit)
   is resolved in this PR — the event contract says once.
3. **Construct `CostTracker` where turns happen**, and record real usage into it.
4. **Enforce the cap.** Decide and document what happens at the limit: refuse new turns with a
   clear error is the safe default. Cron and heartbeat runs must be subject to it — they are
   the unattended paths the cap exists for.
   **Verify**: a test that exceeds the cap and asserts the next turn is refused.
5. **Reconcile the config.** `security.max_cost_per_day_cents` and `cost.daily_limit_usd` are
   two keys for one idea, read by different code. Pick one, migrate the other, and make
   `status` stop presenting an unenforced number as a security control.
6. **Make the surfaces honest.** Where usage is `None`, the TUI and console must show "not
   reported", not `0`.

## Done criteria

- A turn against a usage-reporting provider records non-zero tokens end to end.
- Exceeding the cap refuses the next turn, including from cron and heartbeat.
- No surface displays a zero that means "unknown".
- `cargo test --lib agent`, `--lib providers`, `--lib cost` pass.

## STOP conditions

- Threading usage requires changing the `Provider` trait in a way that breaks external
  implementers → STOP and report; `examples/custom_provider.rs` is already misleading and a
  trait change needs its own migration note.
- Enforcement would silently kill an in-flight cron job mid-run → STOP; decide the semantics
  first (refuse to start vs interrupt) and write them down.

## Maintenance note

Split this if it grows: usage plumbing (steps 1-2) is independently valuable and revertable;
enforcement (steps 3-5) depends on it. Landing step 1-2 alone is a legitimate outcome.

## Rollback

Two commits if split. Enforcement reverts to reporting-only without data loss.
