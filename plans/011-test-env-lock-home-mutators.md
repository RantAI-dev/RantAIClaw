# Plan 011: Route the three remaining HOME-mutating test locks through the shared ENV_LOCK

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/test_env.rs src/lifecycle/uninstall.rs src/profile/sentinel.rs src/gateway/mod.rs`
> If any changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: tests
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

`cargo test --lib` runs every unit test in one process across many threads. Tests
that mutate process-global env (`HOME`, `RANTAICLAW_CONFIG_DIR`, …) must
serialize on ONE shared lock or they clobber each other mid-test — this
previously surfaced as flaky `unwrap()`-on-`None` panics and was fixed by
introducing a crate-shared `test_env::ENV_LOCK`. But three test sites still guard
`std::env::set_var("HOME", …)` with their own **private** mutexes that do not
acquire `ENV_LOCK`. A test in one of those modules can run concurrently with a
channel/config test holding `ENV_LOCK`; both mutate `HOME`, reproducing exactly
the clobber the shared lock was created to prevent. The known flake is not
actually closed.

## Current state

- `src/test_env.rs:22` — the crate-shared lock (verified):
  ```rust
  pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());  // tokio::sync::Mutex
  ```
  Its doc-comment explains: sync tests use `ENV_LOCK.blocking_lock()`, async
  tests use `ENV_LOCK.lock().await`.

- Three private locks that bypass it (verified):
  - `src/lifecycle/uninstall.rs:589` — `static HOME_LOCK: Mutex<()> = Mutex::new(());`
    (std Mutex), taken at `:592` `HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner())`,
    sets HOME at `:596` and restores at `:602`.
  - `src/profile/sentinel.rs:123` — `static HOME_LOCK: Mutex<()> = Mutex::new(());`,
    taken at `:126`, sets HOME at `:129`, restores at `:132`.
  - `src/gateway/mod.rs:2537` — `static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());`,
    taken at `:2591` `HOME_ENV_LOCK.lock().unwrap()`, sets HOME at `:2594`,
    restores at `:2625`.
  All three are inside `#[cfg(test)]` code.

- **IMPORTANT — two crate roots.** This repo compiles both `src/lib.rs` and
  `src/main.rs` as crate roots. `src/test_env.rs` is declared as a module; a
  `#[cfg(test)] mod test_env;` must be visible in the crate root that compiles
  each test. `ENV_LOCK` is already used by ~20 channel modules, so the lib-side
  declaration exists. Confirm the path resolves from each of the three sites
  (they are all reachable from the lib crate). Do NOT assume — after wiring, run
  `cargo clippy --all-targets` which compiles both roots.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (both crate roots) | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Affected tests | `cargo test --lib uninstall profile gateway` | all pass |
| Repeat under load | `cargo test --lib` (run 2–3×) | stable, no flakes |

## Scope

**In scope**:
- `src/lifecycle/uninstall.rs` — replace `HOME_LOCK` usage with `ENV_LOCK`.
- `src/profile/sentinel.rs` — same.
- `src/gateway/mod.rs` — same for `HOME_ENV_LOCK`.
- Optionally `dev/ci.sh` or a lint step — a grep guard for new `set_var("HOME"`
  sites (see Step 4; optional).

**Out of scope** (do NOT touch):
- `src/test_env.rs` — the shared lock is correct; reuse it, don't change it.
- Non-test code in any of these files.
- The ~20 channel modules already using `ENV_LOCK`.

## Git workflow

- Branch: `advisor/011-test-env-lock-home-mutators`
- One commit; message e.g.
  `test: route uninstall/sentinel/gateway HOME mutators through shared ENV_LOCK`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Replace `HOME_LOCK` in the two sync sites

In `src/lifecycle/uninstall.rs` and `src/profile/sentinel.rs`, delete the private
`static HOME_LOCK` and replace the `.lock()...` acquisition with the shared lock.
These are sync `#[test]` contexts (no runtime), so use `blocking_lock()`:

```rust
let _g = crate::test_env::ENV_LOCK.blocking_lock();
```
Keep the rest of each test (set HOME, do work, restore HOME) unchanged. The guard
must live for the whole critical section (until HOME is restored), exactly as the
private lock did.

**Verify**: `cargo build --tests 2>&1 | tail -5` → compiles (these are test-only,
so use `--tests`). Also `grep -n "HOME_LOCK" src/lifecycle/uninstall.rs src/profile/sentinel.rs`
→ no matches.

### Step 2: Replace `HOME_ENV_LOCK` in the gateway site

In `src/gateway/mod.rs`, delete the private `static HOME_ENV_LOCK` (line 2537)
and its `.lock().unwrap()` acquisition (line 2591). Determine whether the
enclosing test is `#[test]` (sync) or `#[tokio::test]` (async) by reading the fn
signature above line 2591:
- If sync: `let _g = crate::test_env::ENV_LOCK.blocking_lock();`
- If async: `let _g = crate::test_env::ENV_LOCK.lock().await;`

**Verify**: `grep -n "HOME_ENV_LOCK" src/gateway/mod.rs` → no matches;
`cargo build --tests 2>&1 | tail -5` → compiles.

### Step 3: Verify both crate roots compile and tests are stable

**Verify**:
- `cargo clippy --all-targets -- -D warnings` → exit 0 (this compiles both the
  lib and bin crate roots; catches a missing `mod test_env;` in `main.rs` if the
  path didn't resolve — see Current state note).
- `cargo test --lib uninstall profile gateway` → all pass.
- Run `cargo test --lib` two or three times → no intermittent failures.

### Step 4 (optional): Add a guard against future bypass

If cheap, add a grep-based check to the local CI helper so new
`std::env::set_var("HOME"` sites that don't use `ENV_LOCK` are caught. E.g. a
line in `dev/ci.sh`'s lint path, or a comment in `src/test_env.rs` pointing here.
Keep it minimal; skip if it complicates the CI script disproportionately.

## Test plan

- No new behavior tests — this is test-infra hardening. The verification IS the
  test: both crate roots compile, and repeated `cargo test --lib` runs are
  stable.
- If you want a positive assertion, the existing tests in the three modules that
  set HOME are the coverage; confirm they still pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0 (both crate roots)
- [ ] `grep -rn "HOME_LOCK\|HOME_ENV_LOCK" src/` returns no matches
- [ ] `grep -rn "set_var(\"HOME\"" src/` — every remaining site is inside a block
      that also acquires `ENV_LOCK` (verify by reading each hit)
- [ ] `cargo test --lib` passes on 3 consecutive runs
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- Any of the three private-lock sites doesn't match the excerpts (drift).
- `cargo clippy --all-targets` fails with an unresolved `crate::test_env` path
  from one of the sites — this means the `mod test_env;` declaration isn't
  visible from that crate root; report it (the fix is a `#[cfg(test)] mod
  test_env;` in the missing root, which is a known two-crate-root gotcha).
- A site sets other global env vars besides HOME under the same private lock —
  make sure the shared lock still covers all of them; report if the scope is
  wider than described.

## Maintenance notes

- This is the same class of bug that a prior fix (#231) addressed by introducing
  `ENV_LOCK`; these three were missed. Any future test that mutates `HOME`,
  `RANTAICLAW_CONFIG_DIR`, `RANTAICLAW_WORKSPACE`, or `RANTAICLAW_PROFILE` must
  acquire `ENV_LOCK`, never a private mutex.
- Reviewer should grep for `Mutex::new(())` in `#[cfg(test)]` code as a smell for
  new private env locks.
