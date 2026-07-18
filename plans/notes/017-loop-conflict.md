# Plan 017 Phase A — Conflict Report: tool-iteration loop tests vs. current loop design

**Status**: Phase A complete. STOPPED for maintainer decision (Option A vs Option B below).
**Verified at**: commit `4d35107` (branch `advisor/017-loop-tests-reconcile`, no drift from the
commit the plan was written at — `git diff --stat 4d35107..HEAD -- src/agent/ src/channels/mod.rs`
is empty).

## Execution note

Per this spike's constraints, no `cargo` commands were run; verification is via static reading of
the live source (`Read`/`grep`), not by executing the test binary. This is sufficient for a
deterministic call here: both the mock provider (`IterativeToolProvider`) and the mock tool
(`MockPriceTool`) used by the two tests return **fixed, non-varying** output on every call, so the
loop-detector's `(name, args, result_hash)` key is provably identical across iterations without
needing to execute anything — the code paths below are traced to their unconditional return values.

## Step A1 — exact attribution for each test

### Test 1: `process_channel_message_respects_configured_max_tool_iterations_above_default`
(`src/channels/mod.rs:5389-5457`, `#[ignore]` at `:5389`)

- Setup: `IterativeToolProvider{required_tool_iterations:11}`, `max_tool_iterations:12`, tool =
  `MockPriceTool`.
- `IterativeToolProvider::chat_with_history` (`channels/mod.rs:4222-4236`) returns
  `tool_call_payload()` every call until `completed_tool_iterations >= 11`.
  `tool_call_payload()` (`channels/mod.rs:4096-4101`) is a **fixed** string:
  `{"name":"mock_price","arguments":{"symbol":"BTC"}}` — identical every iteration.
- `MockPriceTool::execute` (`channels/mod.rs:4364-4379`) returns the **fixed** output
  `{"symbol":"BTC","price_usd":65000}` for every `symbol=BTC` call — identical every iteration.
- In `run_structured_loop` (`agent/loop_.rs:1394-1421`, bounded `for _iteration in 0..max_iterations`
  at `:1435`), the loop-detector (`agent/loop_.rs:1638-1683`) computes
  `key = (call.name, args_json, hash(result))` per tool call (`:1641-1648`) and counts repeats in a
  5-entry sliding window (`:1649-1653`). Because name/args/result never vary here, the same key is
  pushed every iteration; `repeats >= 3` (`:1654`) fires on the **3rd** tool call — far short of the
  11 iterations `IterativeToolProvider` requires before it would say "Completed after 11 tool
  iterations."
- On fire, the loop returns `force_final_summary(...)` (`:1668-1682`) with a "you're stuck in a
  loop" nudge (`:1661-1667`) — a normal `Ok(String)` model-generated summary. It never reaches
  `IterativeToolProvider`'s "Completed after N tool iterations." branch because the loop never gets
  that far.
- **Failing assertion**: `sent_messages[0].contains("Completed after 11 tool iterations.")`
  (`channels/mod.rs:5455`).
- **Early-exit line**: `agent/loop_.rs:1654` (loop-detector), **not** `agent.rs::turn_inner` as the
  stale `#[ignore]`/TODO note (`channels/mod.rs:5384-5388`) claims.

### Test 2: `process_channel_message_reports_configured_max_tool_iterations_limit`
(`src/channels/mod.rs:5462-5529`, `#[ignore]` at `:5462`)

- Setup: `IterativeToolProvider{required_tool_iterations:20}`, `max_tool_iterations:3`.
- Same identical-call/identical-result mechanics as Test 1: the loop-detector fires on the 3rd tool
  call, at essentially the same point the 3-iteration budget would also exhaust. Either trigger —
  loop-detector (`:1654`) or the `for` loop exhausting its bound and falling through to the soft-cap
  block (`:1699-1725`) — terminates via `force_final_summary`, which is unconditionally
  `-> Result<String>` (`agent/loop_.rs:1797,1810`) and never constructs an `Err` for the
  cap/loop-detector case (only infrastructure errors, e.g. multimodal prep at `:1815` or an
  underlying provider failure, could produce an `Err`).
- `grep -rn "exceeded maximum tool iterations" src/` (run as part of this spike) → 3 hits, **zero**
  at runtime:
  - `src/config/schema.rs:416` — comment only.
  - `src/tui/commands/calls.rs:5` — comment only.
  - `src/channels/mod.rs:5528` — the test assertion itself.
  No `bail!`/`Err`-construction anywhere in `src/` produces this string.
- The `"⚠️ Error: {e}"` prefix the test expects (`channels/mod.rs:5528`) is only emitted by the
  channel's generic `Err` branch (`channels/mod.rs:2030`, `:2035`), reached only when
  `run_tool_call_loop` (called at `channels/mod.rs:1868`) returns `Err`. The success arm
  (`channels/mod.rs:1916`, `LlmExecutionResult::Completed(Ok(Ok(response)))`) is what actually fires
  here, since `force_final_summary` returns `Ok`.
- **Failing assertion**:
  `sent_messages[0].contains("⚠️ Error: Agent exceeded maximum tool iterations (3)")`
  (`channels/mod.rs:5528`).
- **Early-exit line**: `agent/loop_.rs:1699-1725` (soft-cap), with the loop-detector (`:1654`) also
  independently sufficient to trigger the same outcome in this fixture. **Not** `agent.rs::turn_inner`
  as the stale note (`channels/mod.rs:5459-5461`) claims.

## Step A2 — the conflict and the options

1. Both ignored tests assert v0.4 semantics:
   (a) the loop keeps calling the provider/tool through identical results up to N iterations before
       stopping ("Completed after 11 tool iterations.");
   (b) hitting `max_tool_iterations` is a hard failure surfaced to the user as
       `"⚠️ Error: Agent exceeded maximum tool iterations (N)"`.
2. The current loop intentionally does neither:
   - **Loop-detector** (`agent/loop_.rs:1638-1683`, `repeats >= 3` at `:1654`) — design-intent
     comment (`:1427-1431`, `:1638-1640`): "If the *exact* triple (same tool + same args + same
     result) appears 3 times in this window, the model is clearly stuck — break early..."
   - **Soft-cap** (`agent/loop_.rs:1699-1725`) — design-intent comment, quoted verbatim: "instead of
     bailing with no output (which surfaces as \"[no response from model]\" in the TUI), force one
     final tools-disabled provider call so the user gets a real summary of what was attempted.
     Mentions `/continue` so the user knows how to extend the budget if more work is needed."
3. **The decision needed**: does the *test* encode intended product behavior, or do the
   *heuristics*?
   - Option B keeps the tests as the spec: restore v0.4 — let identical-tool-call loops run to N,
     hard-error at the cap.
   - Option A keeps the heuristics as the spec: rewrite the two tests to assert force-summary-on-
     repeat and force-summary-at-cap, dropping the "Completed after 11 iterations" /
     "exceeded maximum" expectations.
4. **Options in detail**:

   **Option A — keep the redesign, rewrite the tests (recommended)**
   - Code change: none to `loop_.rs`. Rewrite/replace the two tests in `channels/mod.rs` to assert:
     (i) a provider+tool that returns identical tool calls/results ends in a force-summary reply
     after 3 repeats (not an error, not "Completed after N"); (ii) exceeding `max_tool_iterations`
     with a provider that never converges also ends in a force-summary reply (not
     `"⚠️ Error:..."`). Remove `#[ignore]` and correct the misattributing `TODO(agent-loop)` notes
     (`channels/mod.rs:5384-5388`, `:5459-5461`) — they blame `agent.rs::turn_inner` /
     history-threading, which this investigation shows is not the mechanism (proven independently by
     the already-passing `process_channel_message_executes_tool_calls_instead_of_sending_raw_json`,
     which confirms tool-result threading works).
   - User-visible behavior: unchanged from what ships today — repeated/exhausted tool loops end with
     a graceful natural-language summary instead of silence or a raw error.
   - Risk: LOW. No runtime behavior changes; only test assertions and stale comments change.

   **Option B — restore v0.4 hard-error + loop-to-N semantics**
   - Code change: remove or substantially relax the loop-detector (`agent/loop_.rs:1638-1683`) so
     identical tool calls are allowed to repeat up to `max_tool_iterations`, AND replace the
     soft-cap's `force_final_summary` call (`:1711-1725`) with a `bail!`/`Err` carrying
     `"Agent exceeded maximum tool iterations (N)"` so it flows through the channel's existing `Err`
     → `"⚠️ Error: {e}"` path (`channels/mod.rs:2030`/`:2035`). This is a reversion of two
     deliberate, independently-commented features, not a small change.
   - User-visible behavior: an agent stuck in an identical-call loop would burn its full iteration
     budget before failing (worse latency/cost), and hitting the cap becomes a hard user-facing
     error instead of a best-effort summary.
   - Risk: MED-HIGH. Touches two independent guardrails at once, and both are shared by the
     interactive `Agent` path (`agent.rs:911`) and the channel path (`channels/mod.rs:1868`) via the
     same `run_structured_loop`/`run_tool_call_loop` — not a channel-scoped change.

   **Recommendation: Option A.** The loop-detector and soft-cap read as deliberate, recent,
   well-commented safety/UX features (not accidental regressions — no unexplained drift, explicit
   rationale in comments, including a `/continue` UX hook), and a graceful summary is generally
   better UX than a raw iteration-count error or letting a stuck loop burn its full budget. This is a
   recommendation, not the decision — a maintainer must confirm before Phase B proceeds.

## STOP conditions checked

- Loop-detector/soft-cap intentionality: confirmed via explicit design-intent comments at
  `agent/loop_.rs:1427-1431`, `:1638-1640`, `:1699-1710` — not accidental (no drift from the
  planned commit; comments state rationale directly).
- Blast radius of Option B: confirmed it exceeds `loop_.rs` + the two tests — it also requires
  re-verifying the channel's `Err`-formatting path (`channels/mod.rs:2030`/`:2035`, currently
  correct/unchanged) and re-examining the interactive `Agent` path (`agent.rs:911`), since both
  consume the same shared loop implementation. Flagged per the plan's stated STOP condition.

## Verified citations (file:line)

- Loop-detector: `src/agent/loop_.rs:1638-1683` (repeat check `:1654`)
- Soft-cap: `src/agent/loop_.rs:1699-1725`
- `force_final_summary`: `src/agent/loop_.rs:1797-1810`+ (signature `-> Result<String>`; no bail on
  cap/loop-detector path)
- `run_structured_loop`: `src/agent/loop_.rs:1394-1421` (bounded for-loop `:1435`, `max_iterations`
  computed `:1417-1421`)
- `run_tool_call_loop` (flat adapter): `src/agent/loop_.rs:1735`
- Channel call site: `src/channels/mod.rs:1868` (`run_tool_call_loop`), `:1916` (Ok success path),
  `:2030`/`:2035` (Err → `"⚠️ Error: {e}"`)
- Test 1: `src/channels/mod.rs:5389-5457` (`#[ignore]` `:5389`, TODO `:5384-5388`, assertions
  `:5453-5456`)
- Test 2: `src/channels/mod.rs:5462-5529` (`#[ignore]` `:5462`, TODO `:5459-5461`, assertion
  `:5528`)
- Test helpers: `RecordingChannel` `:4001`, `IterativeToolProvider` `:4197-4237`
  (`required_tool_iterations` `:4198`), `MockPriceTool` `:4309`/`:4344-4380`, `tool_call_payload()`
  `:4096-4101`
- `grep -rn "exceeded maximum tool iterations" src/`:
  - `src/config/schema.rs:416` (comment)
  - `src/tui/commands/calls.rs:5` (comment)
  - `src/channels/mod.rs:5528` (test assertion)
  - Zero runtime occurrences.

## Next step

Hard STOP per the plan. Phase B (rewriting the tests per Option A, or reverting the loop per
Option B) requires an explicit maintainer decision and is out of scope for this spike.
