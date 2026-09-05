# Plan 017 (decision spike): Reconcile the two stale tool-iteration tests with the intentional loop redesign

> **Executor instructions**: This is a **decision/reconciliation spike**, NOT a
> mechanical fix. The two `#[ignore]`'d tests encode OLD (v0.4) loop behavior
> that was deliberately replaced. Your job is to characterize the conflict
> precisely and STOP for a maintainer decision — do NOT "fix" the loop to make
> the old tests pass, because that would revert intentional features. Follow
> Phase A, produce the written report, and STOP at the decision gate.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/agent/ src/channels/mod.rs`
> If either changed since this plan was written, re-verify the line references
> below; on a mismatch, treat it as a STOP condition.
>
> **REVISED after cold review**: the earlier draft framed this as "diagnose + fix
> a regression" and told the executor to re-arm a `bail!` limit error and fix
> "history threading." Both premises are wrong (see Current state). This is a
> spec conflict between stale tests and an intentional redesign.

## Status

- **Priority**: P2
- **Effort**: M (spike + a decision; the eventual code change depends on the decision)
- **Risk**: MED
- **Depends on**: none (recommend plan 001 land first so re-enabled tests run pre-merge)
- **Category**: direction
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

Two `#[ignore]`'d tests plus in-code notes claim the agent tool-iteration loop
"regressed" (v0.4 → v0.6): the agent returns raw tool-call payload instead of
looping, and a `max_tool_iterations` limit error no longer reaches the channel.
**Cold review established this is not a mechanical regression — the loop was
intentionally redesigned**, and the two tests assert the pre-redesign behavior.
Resolving this correctly (keep the new design and update the tests, OR restore
the old behavior) is a product decision about how the agent should behave when it
loops or hits the iteration cap. Shipping a "fix" that just makes the old tests
pass would silently revert two deliberate features. The value here is getting
that decision made with the facts on the table, then executing whichever way it
goes.

## Current state (verified against live code at 4d35107 — no drift)

- The two disabled tests, in `src/channels/mod.rs`:
  - `:5389` `#[ignore]` `process_channel_message_respects_configured_max_tool_iterations_above_default`
    — sets `IterativeToolProvider { required_tool_iterations: 11 }`,
    `max_tool_iterations: 12`, and asserts the reply contains
    `"Completed after 11 tool iterations."` (asserts at `:5453-5456`).
  - `:5462` `process_channel_message_reports_configured_max_tool_iterations_limit`
    — asserts the reply contains
    `"⚠️ Error: Agent exceeded maximum tool iterations (3)"` (assert at `:5528`).
  - Notes at `:5384-5388` and `:5459-5461` blame `agent.rs::turn_inner`.

- **The `bail!` limit error the second test expects DOES NOT EXIST** — it was
  replaced by a soft-cap. On hitting `max_iterations` the loop calls
  `force_final_summary(...)` (`src/agent/loop_.rs:1699-1725`), whose comment says
  "instead of bailing... force one final tools-disabled provider call." The
  asserted string `"Agent exceeded maximum tool iterations"` appears only in
  comments (`src/config/schema.rs:416`, `src/tui/commands/calls.rs:5`) and the
  test assertion itself — never as a runtime error. (`grep -rn "exceeded maximum tool iterations" src/`.)

- **The first test's early stop is the loop-detector, not history threading.**
  Tool-result threading already works (proven by the PASSING test
  `process_channel_message_executes_tool_calls_instead_of_sending_raw_json`).
  `IterativeToolProvider` (required=11) calls the same `mock_price{symbol:BTC}`
  every iteration; `MockPriceTool` returns identical output each time, so the
  loop-detector's `(name, args, result_hash)` triple repeats and fires
  `force_final_summary` at the 3rd repeat (`src/agent/loop_.rs:1638-1683`,
  `repeats >= 3` at `:1654`) — long before iteration 11. So the test can never
  reach "11 iterations" unless the loop-detector is neutralized.

- **The channel path calls `run_tool_call_loop`, not `run_structured_loop`
  directly.** `process_channel_message` → `run_tool_call_loop`
  (`src/channels/mod.rs:1868`), the flat-`Vec<ChatMessage>` adapter at
  `src/agent/loop_.rs:1735` that wraps `run_structured_loop`. Both the
  interactive `Agent` path (`agent.rs:911`) and the channel path go through the
  SAME loop — there is no working-vs-broken divergence to copy.

- Test helpers all exist: `RecordingChannel` (`channels/mod.rs:4001`),
  `IterativeToolProvider` (`:4197`, field `required_tool_iterations` `:4198`),
  `MockPriceTool` (`:4309`), `tool_call_payload()` (`:4096`).
  `run_structured_loop` signature at `loop_.rs:1394-1416`.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| Run BOTH ignored tests (correct multi-name syntax) | `cargo test --lib -- --include-ignored process_channel_message_respects_configured_max_tool_iterations process_channel_message_reports_configured_max_tool_iterations` | both currently FAIL (reproduces the conflict) |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Loop tests | `cargo test --lib -- loop_` | pass |

Note: `cargo test A B` with two bare positionals ERRORS (`unexpected argument`).
Filters must go after `--`, e.g. `cargo test --lib -- name1 name2`.

## Scope

**Phase A (this spike) — in scope**:
- Read-only investigation of `src/agent/loop_.rs` (loop-detector `:1638-1683`,
  soft-cap `force_final_summary` `:1699-1725`, `run_structured_loop`,
  `run_tool_call_loop`) and the two tests.
- A written **conflict report + options** (in the PR body or
  `plans/notes/017-loop-conflict.md`).

**Phase B (only after a maintainer decision) — in scope**:
- EITHER update/replace the two tests to assert the intended redesigned behavior
  (soft-cap summary + loop-detector), OR (if the decision is to restore v0.4
  semantics) reintroduce the hard-error/iteration behavior — whichever the
  maintainer picks.

**Out of scope**:
- Do NOT delete the loop-detector or soft-cap to make the old tests pass.
- Do NOT change the loop's public signature or other loop features (cancellation,
  approval gating).

## Git workflow

- Branch: `advisor/017-loop-tests-reconcile`
- Commit the Phase A report first. STOP for the decision. Then Phase B.
- Do NOT push or open a PR unless instructed. Open for review; do not self-merge.

## Steps

### Phase A — Characterize the conflict (read-only; produce a report)

### Step A1: Reproduce and attribute the failures precisely

Run the two tests with the correct syntax (Commands table). For EACH, state the
exact assertion that fails and the exact line in `loop_.rs` that causes the
early exit:
- Test 1 (`...respects...above_default`): fails because the loop-detector
  (`loop_.rs:1638-1683`) fires `force_final_summary` at the 3rd identical
  `mock_price` result, so the reply is a summary, not "Completed after 11 tool
  iterations." Confirm by reading the detector.
- Test 2 (`...reports...limit`): fails because there is no `bail!` producing
  "Agent exceeded maximum tool iterations"; the cap triggers `force_final_summary`
  (`loop_.rs:1699-1725`) instead. Confirm with
  `grep -rn "exceeded maximum tool iterations" src/`.

**Verify**: your report names both early-exit lines and quotes the design-intent
comments at `loop_.rs:1654` and `:1699-1725`.

### Step A2: Write the conflict report + options

Produce a short document stating:
1. The two tests assert v0.4 behavior: (a) loop past identical tool results up to
   N; (b) hard-error at the cap.
2. The current loop intentionally does neither: (a) loop-detector force-summarizes
   on repeated identical calls; (b) soft-cap force-summarizes instead of erroring.
3. **The decision needed**: do the *tests* encode intended behavior (→ restore
   v0.4 hard-error + let identical-result loops run to N) or do the *heuristics*
   (→ rewrite the two tests to assert summary-on-repeat and summary-at-cap, and
   drop the "Completed after 11 iterations" / "exceeded maximum" expectations)?
4. For each option: what code changes, what user-visible behavior results, and
   the risk. Recommend one (the heuristics look deliberate and recent — a
   loop-detector and a graceful soft-cap are usually the better UX than raw
   iteration counts and hard errors — but say so as a recommendation, not a
   decision).

### Step A3: STOP for the maintainer decision

**This is a hard STOP.** Do not proceed to Phase B without an explicit decision
(A or B) from the maintainer. Post the report and wait.

### Phase B — Execute the chosen direction (only after decision)

- **If "heuristics are intended" (likely):** rewrite the two tests to assert the
  current design — e.g. that a repeated-identical-tool loop ends in a
  force-summary reply, and that exceeding `max_tool_iterations` ends in a
  force-summary (not an error). Remove the `#[ignore]` and the stale
  `TODO(agent-loop)` notes. Keep `IterativeToolProvider`/`MockPriceTool` but
  adjust expectations (e.g. `MockPriceTool` returning DISTINCT output per call
  would defeat the loop-detector and let iteration count matter — decide with the
  maintainer whether that's the behavior to test).
- **If "restore v0.4":** reintroduce the hard-error at the cap and the
  loop-to-N-on-identical-results behavior; re-enable the tests unchanged. This is
  a larger behavior change touching `force_final_summary`/loop-detector — treat as
  a separate high-risk change.

**Verify** (either way): `cargo test --lib -- process_channel_message_respects_configured_max_tool_iterations process_channel_message_reports_configured_max_tool_iterations`
→ both pass WITHOUT `--include-ignored`; `cargo test --lib -- loop_` → pass;
`cargo clippy --all-targets -- -D warnings` → 0.

## Test plan

- Phase B's re-enabled/rewritten tests are the acceptance criteria, but ONLY
  after the decision fixes what "correct" means. Until then, the deliverable is
  the Phase A report.
- No network dependence (existing tests use in-process mocks).

## Done criteria

**Phase A (this spike):**
- [ ] Conflict report exists, names both early-exit lines (`loop_.rs:1654`
      detector, `:1699-1725` soft-cap), and lists the two options with a recommendation
- [ ] `grep -rn "exceeded maximum tool iterations" src/` result is documented
      (confirming no runtime bail exists)
- [ ] A maintainer decision (A or B) is recorded before any Phase B code change
- [ ] `plans/README.md` status row updated (to BLOCKED-pending-decision or DONE-Phase-A)

**Phase B (post-decision), whichever applies:**
- [ ] The two tests pass without `--include-ignored`, asserting the DECIDED behavior
- [ ] `grep -n "TODO(agent-loop)" src/channels/mod.rs` → no matches
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0

## STOP conditions

Stop and report (this plan is STOP-heavy by design):

- **Always STOP after Phase A** for the maintainer decision — do not pick a
  direction yourself.
- If Phase A reveals the loop-detector/soft-cap are NOT intentional (e.g. no
  design comment, recent accidental change in git blame) — report that, it
  changes the recommendation.
- If restoring v0.4 (option B) would touch more than `loop_.rs` + the two tests —
  report the blast radius first.

## Maintenance notes

- The tests' `#[ignore]` notes are themselves misleading (they blame
  `turn_inner`/history-threading, which are not the cause). Whichever option is
  chosen, correct or remove those notes so the next reader isn't misdirected.
- Reviewer should treat this as a behavior-spec change, not a bugfix: the PR must
  state which behavior the agent now guarantees at the iteration cap and on
  repeated identical tool calls.
- Follow-up (deferred): audit whether gateway chat / api_v1 `agent/chat` surface
  the soft-cap summary sensibly to their clients too.
