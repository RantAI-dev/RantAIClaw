# Plan 197: Fix the rate limiter disabling itself when host uptime is under 1 hour

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/policy.rs`
> On any change to the `ActionTracker` region, compare against the excerpt
> below before proceeding.

## Status

- **Priority**: P0 (security — the primary runaway guard silently no-ops)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`max_actions_per_hour` is, per the config schema's own words, "the primary
runaway guard actually enforced in the agent loop" (`src/config/schema.rs`
autonomy doc). It is enforced by `ActionTracker`, a sliding one-hour window of
`Instant` timestamps.

`Instant` on Linux is `CLOCK_MONOTONIC`, whose epoch is approximately system
boot. When the host has been up for **less than one hour**,
`Instant::now().checked_sub(Duration::from_hours(1))` underflows (there is no
representable instant one hour before "now"), returns `None`, and the code
falls back to `cutoff = Instant::now()`. The retain step
`actions.retain(|t| *t > cutoff)` then evicts **every** timestamp (all are
`<= now`), so the window is cleared on every call and `record()` always
returns `1`.

Net effect: on any freshly-booted host, container, CI runner, or
recently-rebooted VM — exactly the environments where an autonomous agent is
most likely to run unattended — `max_actions_per_hour` never triggers. The
fallback chose the maximally insecure direction: unable to compute "one hour
ago", it discarded the whole window instead of keeping it.

## Current state

### `ActionTracker::record` / `count` — `src/security/policy.rs:54-74`

```rust
    /// Record an action and return the current count within the window.
    pub fn record(&self) -> usize {
        let mut actions = self.actions.lock();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_hours(1))
            .unwrap_or_else(Instant::now);      // <-- BUG: fallback = now clears the window
        actions.retain(|t| *t > cutoff);
        actions.push(Instant::now());
        actions.len()
    }

    /// Count of actions in the current window without recording.
    pub fn count(&self) -> usize {
        let mut actions = self.actions.lock();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_hours(1))
            .unwrap_or_else(Instant::now);      // <-- same bug
        actions.retain(|t| *t > cutoff);
        actions.len()
    }
```

The consumers are correct and must not change: `record_action` (`policy.rs:990`,
`count <= max`) and `is_rate_limited` (`policy.rs:996`, `count >= max`).

## The fix

When "one hour ago" is not representable (uptime < 1h), the correct cutoff is
"the beginning of the monotonic clock", i.e. keep **every** recorded action —
never `Instant::now()`. On underflow, skip the retain entirely (or retain with
a cutoff that keeps all entries).

Replace both `unwrap_or_else(Instant::now)` fallbacks. Simplest correct form:

```rust
    pub fn record(&self) -> usize {
        let mut actions = self.actions.lock();
        // When uptime < 1h, `now - 1h` is not representable on the monotonic
        // clock. In that case *keep every* recorded action (the window spans
        // the whole uptime so far) — never fall back to `now`, which would
        // evict the entire window and disable the limit on fresh boots/CI/VMs.
        if let Some(cutoff) = Instant::now().checked_sub(Duration::from_hours(1)) {
            actions.retain(|t| *t > cutoff);
        }
        actions.push(Instant::now());
        actions.len()
    }
```

Apply the identical change to `count()` (retain only when the cutoff is
representable; no `push`).

Ensure `Duration` is in scope (it already is via the existing
`std::time::Duration::from_hours` call; keep the same path or add a `use`).

## Files

- **In scope**: `src/security/policy.rs` — the `ActionTracker` `record` and
  `count` methods only.
- **Out of scope**: everything else. Do not change the consumers, the `<=`/`>=`
  boundaries, or `max_actions_per_hour` defaults.

## STOP conditions

- If `record`/`count` have been refactored to use a wall-clock (`SystemTime`)
  or an absolute base instant instead of `checked_sub`, the underflow may
  already be handled — verify the new logic keeps entries on a fresh boot and
  report rather than editing blindly.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib security::policy` passes, including the new
   test.
4. New test proves entries survive when no "1h ago" cutoff is representable.
   Because a real fresh-boot `Instant` cannot be forged in a unit test, assert
   the invariant directly against the tracker's observable behavior:

```rust
#[test]
fn action_window_does_not_self_clear_within_the_first_hour() {
    let t = ActionTracker::new();
    // Many rapid records, all within one hour of process start.
    for _ in 0..50 { t.record(); }
    // The window must retain them — not reset to 1 on each call.
    assert_eq!(t.count(), 50, "window was cleared — the underflow-clears bug is present");
}
```

Verify the test FAILS against the current `unwrap_or_else(Instant::now)` code
(it returns 1 there **only** when uptime < 1h; on a long-uptime dev box it may
pass either way — so ALSO reason through the fix and note in the PR that the
regression is uptime-dependent). The fix makes the assertion hold regardless of
uptime.

## Test plan

Add the test above to the `ActionTracker` test area in `policy.rs`. Note in the
PR description that the pre-fix failure only reproduces on a host with < 1h
uptime (fresh container/CI is the reliable repro); the fix makes the window
uptime-independent.

## Risk & rollback

- **Risk**: LOW — pure counting logic; the boundary tests for
  `record_action`/`is_rate_limited` still hold (they exercise counts, not the
  cutoff).
- **Rollback**: single-file revert; no schema/API/migration impact.

## Maintenance note

If the tracker is ever migrated to a persistent or cross-process store, keep
the "unrepresentable cutoff = keep all" invariant — the failure mode here is a
silent security downgrade, not a crash, so it will not surface in a green
suite.
