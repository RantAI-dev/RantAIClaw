# Plan 219: Policy test & dead-code cleanup — un-skip the vacuous/dead tests, fix the fixture drift, remove the unreachable fork-bomb check

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/policy.rs src/approval/policy_writer.rs`

## Status

- **Priority**: P3 (tech-debt — false test coverage + dead code)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (but touches the same files as several security plans —
  land after Wave 1–2 to avoid churn)
- **Category**: tests / tech-debt
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

Four small policy-layer defects give false confidence or dead weight:

1. **`full_autonomy_still_respects_forbidden_paths` is mutation-vacuous.**
   `is_path_allowed` (`policy.rs:881-936`) has no autonomy branch — it returns
   identically for ReadOnly/Supervised/Full. So the test at `policy.rs:2111`
   cannot detect any autonomy-specific regression; inverting a (non-existent)
   autonomy guard would not fail it. It asserts a guarantee the code does not
   actually make (the real Full-mode risk is shell reads bypassing paths — see
   plan 214).
2. **`next_cycles_in_canonical_order` never runs.** The function at
   `policy_writer.rs:720` is missing its `#[test]` attribute, so it compiles as
   an ordinary unused fn. The canonical `next()` order — including
   `Strict.next() == Off`, the one rung that walks into the "no prompts" Off
   preset — is unverified (only partially covered by `cycle_never_reaches_off`).
3. **`SecurityPolicy::default()` (test fixture) diverges from production.** It
   sets `max_actions_per_hour: 20` and an allowlist including `"date"`
   (`policy.rs:195`), while `AutonomyConfig::default()` — the shape production
   uses — sets `200` and omits `"date"`. So the policy suite validates a cap and
   allowlist production never runs; a regression that dropped `"date"` from the
   real default or changed the 200 cap would not be caught.
4. **The fork-bomb `contains` check is unreachable.** `policy.rs:530` tests a
   single segment for `":(){:|:&};:"`, but `split_unquoted_segments` already
   split on `;`/`|`/`&` upstream, so a segment can never contain the full string.
   Dead code that implies a guard that does nothing (fork bombs already fail the
   allowlist because `:` isn't allowlisted).

## Current state

- `src/security/policy.rs:2111-2117` — the vacuous Full-mode test.
- `src/approval/policy_writer.rs:719-720` — `fn next_cycles_in_canonical_order()`
  with no `#[test]`.
- `src/security/policy.rs:195` — fixture `max_actions_per_hour: 20`, allowlist
  with `"date"`; `src/config/schema.rs` `default_forbidden_paths`/allowlist +
  `AutonomyConfig::default()` are the production shape (200, no `date`).
- `src/security/policy.rs:530-535` — the fork-bomb `contains` on a post-split
  segment.

## The fix

### Step 1 — retarget the vacuous Full-mode test

Rename/rewrite `full_autonomy_still_respects_forbidden_paths` to assert
something real. Either (a) test that `is_path_allowed` denies a forbidden path
regardless of autonomy (the honest, autonomy-independent property — drop the
"full" framing), or (b) if plan 214 Option A lands (shell honors forbidden_paths
under Full), assert the Full-mode shell case. Do not leave a test that names an
autonomy guarantee the code does not make.

### Step 2 — add `#[test]` to `next_cycles_in_canonical_order`

Add the missing attribute so the canonical `next()` order (including
`Strict → Off`) is actually verified. If the assertions are stale, update them
to the current order.

### Step 3 — reconcile the test-fixture default with production

Build test policies from `AutonomyConfig::default()` (with an explicit
`.with_max_actions_per_hour(20)` where a small cap is deliberately wanted for a
test), so fixtures track the production shape. Reconcile the `"date"` allowlist
difference: decide whether `date` belongs in the real default and make both
consistent. Keep the intentional small-cap tests explicit about overriding.

### Step 4 — remove the dead fork-bomb check

Delete the unreachable `joined_segment.contains(":(){:|:&};:")` branch at
`policy.rs:530` (the allowlist already denies `:`). If a fork-bomb guard is
genuinely wanted, move it **before** the segment split — but the allowlist
already covers it, so deletion is correct; note this in the PR.

## Files

- **In scope**: `src/security/policy.rs` (Steps 1, 3, 4), `src/approval/policy_writer.rs`
  (Step 2), possibly `src/config/schema.rs` (Step 3 `date` reconciliation).
- **Out of scope**: the enforcement changes those tests relate to (plans 198,
  205, 214) — this plan fixes the tests/dead-code, not the enforcement.

## STOP conditions

- If `next_cycles_in_canonical_order`'s assertions fail once `#[test]` is added,
  that is a REAL finding (the `next()` order regressed) — STOP and report before
  "fixing" the test to match; verify which is correct against the Shift+Tab cycle
  behavior.
- If removing the `"date"` divergence would change production behavior (not just
  the fixture), scope Step 3 to the fixture only and report the production
  question separately.

## Done criteria

1. `cargo fmt`/`clippy` clean.
2. `cargo test -p rantaiclaw --lib security::policy approval::policy_writer` passes.
3. `next_cycles_in_canonical_order` now RUNS (confirm it appears in
   `cargo test ... -- --list` or the test count increases).
4. The retargeted Full-mode test asserts a property the code actually makes
   (mutation-check: inverting the relevant guard now fails it).
5. The dead fork-bomb branch is gone and no test regressed.

## Test plan

These are test-quality fixes, so the "test" is the tests themselves behaving:
the un-skipped test runs, the retargeted test is non-vacuous (prove by
temporarily inverting the guard it targets and seeing it fail), and the fixture
now derives from `AutonomyConfig::default()`.

## Risk & rollback

- **Risk**: LOW — test + dead-code changes; the only behavioral touch is the
  optional `"date"` allowlist reconciliation (Step 3), scoped to the fixture
  unless a production question is surfaced.
- **Rollback**: revert the touched files.

## Maintenance note

A test that cannot fail under mutation is worse than no test — it advertises
coverage that isn't there. When adding a security test, mutation-check it: invert
the guard and confirm the test fails. The fixture-vs-production drift here is why
`SecurityPolicy::default()` should be reserved for tests that explicitly want a
non-production shape.
