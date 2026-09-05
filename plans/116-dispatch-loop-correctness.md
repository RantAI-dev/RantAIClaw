# Plan 116: Dispatch-loop correctness — panic-safe completion, lock order, no `eprintln!`, sanitized errors

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/115 (serialized chain over `src/channels/mod.rs`)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Five defects on the path every inbound channel message takes. The first can take
the whole fleet down silently.

A panic anywhere in the message path — provider, tool loop, renderer, a channel's
`send` — skips the completion handshake, so the next message from that sender waits
forever on a signal that will never come. The worker also holds a concurrency permit
it never releases. After 8–64 such events the dispatch loop stops draining its queue
and **all 18 platforms stop answering**, with nothing logging a deadlock because the
task never finishes.

Alongside it: two mutexes are taken in both orders, so a natural-looking
simplification elsewhere deadlocks the loop permanently; three `eprintln!` calls
re-introduce a TUI-corruption regression a comment in the same function says was
fixed; raw error chains carrying local paths and internal URLs are delivered into
chat on one path while the sibling path sanitizes; and the assistant turn is
recorded before delivery succeeds, so the model can believe it answered when the
user got nothing.

## Current state

### 1. The completion handshake is not panic-safe

`src/channels/mod.rs:2240-2252` — `mark_done()` is the last statement of the worker,
with no guard, no `catch_unwind`, and no `JoinSet` fallback:

```rust
            process_channel_message(…).await;
            …
            completion.mark_done();
```

`:2229-2237` — the next message from the same sender waits on it:

```rust
                previous.cancellation.cancel();
                previous.completion.wait().await;
```

`:2208` — and the permit lives inside the same closure:

```rust
        let _permit = permit;
```

`src/channels/mod.rs:306-311` — `wait()` also has a lost-wakeup window:
`notify.notified()` does not register until first polled, and
`notify_waiters()` stores no permit, so a `mark_done()` landing between the
`done` check and the first poll is missed.

`compute_max_in_flight_messages` (`:1605-1612`) caps the semaphore at 8–64.

### 2. Two mutexes taken in both orders

`src/channels/mod.rs:809-816` — the guard is a temporary that lives to the end of the
tail expression, so `default_route_selection` runs **while holding** `route_overrides`:

```rust
fn get_route_selection(ctx: &ChannelRuntimeContext, sender_key: &str) -> ChannelRouteSelection {
    ctx.route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(sender_key)
        .cloned()
        .unwrap_or_else(|| default_route_selection(ctx))
}
```

`default_route_selection` → `runtime_defaults_snapshot` (`:548-556`) acquires the
global `runtime_config_store()` mutex.

`:818-823` does the opposite, deliberately:

```rust
fn set_route_selection(ctx: &ChannelRuntimeContext, sender_key: &str, next: ChannelRouteSelection) {
    let default_route = default_route_selection(ctx);
    let mut routes = ctx
        .route_overrides
        .lock()
```

Both are `std::sync::Mutex` held inside async tasks. The orders do not currently
collide only because `maybe_apply_runtime_config_update` scopes its store guard
closed before touching `route_overrides` (`:754-788`).

### 3. Three `eprintln!` on the message path

`src/channels/mod.rs:2062`, `:2084`, `:2108` — context overflow, LLM error, timeout.

`:1659-1666` — the comment in the same function explaining why they must not exist:

```rust
    // Pre-v0.6.7 used `println!` here, which leaks into the TUI's
    // alt-screen and corrupts rendering when channels are auto-started
    // alongside `rantaiclaw` (see screenshot in v0.6.6 tester report: …)
    // Tracing routes to the log file in TUI mode …
    tracing::info!(
```

Only the happy path was converted.

### 4. Raw error chains delivered into chat

`src/channels/mod.rs:2091` and `:2096`:

```rust
                        .finalize_draft(&msg.reply_target, draft_id, &format!("⚠️ Error: {e}"))
                            &SendMessage::new(format!("⚠️ Error: {e}"), &msg.reply_target)
```

`:1687` — the sibling path, done correctly:

```rust
            let safe_err = providers::sanitize_api_error(&err.to_string());
```

The sanitized form is also length-capped (`src/providers/mod.rs:812-825`); the raw
one is not.

### 5. History recorded before delivery, and not at all on failure

`:1997-2001` appends the assistant turn (and write-throughs to `brain.db`) before the
send at `:2012-2035`; a failed send only logs at `:2033`. The timeout arm
(`:2103-2129`), the LLM-error arm (`:2084-2101`) and the cancellation arm
(`:1963-1976`) append nothing, leaving the user turn from `:1752` unpaired — and
`normalize_cached_channel_turns` then merges consecutive user turns into one blob
(`:425-434`).

### 6. The JSON-artifact stripper re-parses the tail per `{`

`:1399-1443` — on a rejected candidate the cursor advances one char and the next `{`
re-parses nearly the same remaining string. Runs on every delivered reply.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/mod.rs` only.

**Out of scope**:
- The hot-reload gaps (owners dropped on the failure branch, boot-pinned fields,
  the global static) — plan 117, next in this chain.
- The conversation-history **key** and retention — plan 118.
- Dead code removal — plan 119. The channel factory — plan 120. Decomposition — 121.
- The approval-reply interception at `:2169-2182` — plan 122 owns that call, and it
  depends on this plan. Do not change it here beyond leaving it compiling.
- `src/providers/mod.rs` — reuse `sanitize_api_error`, do not modify it.

## Git workflow

- Branch: `fix/dispatch-loop-correctness`
- Conventional commits, e.g. `fix(channels): release the in-flight handshake on panic`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Make completion release on unwind

Hold the `Arc<InFlightTaskCompletion>` in a guard struct whose `Drop` calls
`mark_done()`, so an unwind still releases waiters and the `in_flight` map entry is
cleaned up. The permit at `:2208` is already dropped by unwinding once the closure's
locals drop — verify that, and if the permit is held by something that survives,
move it into the same guard.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 2: Close the lost-wakeup window in `wait()`

Rewrite `InFlightTaskCompletion::wait` to register before re-checking:

```rust
        let n = self.notify.notified();
        tokio::pin!(n);
        n.as_mut().enable();
        if self.done.load(Ordering::Acquire) {
            return;
        }
        n.await;
```

Add a bounded `tokio::time::timeout` around the `previous.completion.wait()` call at
`:2229-2237` as a backstop, logging when it fires. Two overlapping turns for one
sender is strictly better than a permanent hang.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 3: Fix the lock order

In `get_route_selection`, bind the lookup to a local so the guard drops before
`default_route_selection` runs:

```rust
    let existing = {
        ctx.route_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sender_key)
            .cloned()
    };
    existing.unwrap_or_else(|| default_route_selection(ctx))
```

Add a comment naming the invariant: **the runtime-config store is always acquired
before `route_overrides`, never inside it.**

**Verify**: `cargo test --lib channels::` → all pass.

### Step 4: Convert the three `eprintln!` to `tracing`

Use `warn!`/`error!` with structured fields (`elapsed_ms`, `channel`, `sender`),
matching `:1666` and `:2007`. The user already receives these messages over the
channel; only the local console surface changes.

**Verify**: `grep -n 'eprintln!' src/channels/mod.rs` returns nothing.

### Step 5: Sanitize errors before they reach a chat

Wrap both `:2091` and `:2096` in `providers::sanitize_api_error(&format!("{e:#}"))`,
mirroring `:1687`. Keep the unredacted error in the `tracing` record only.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 6: Record the assistant turn only after delivery, and pair the failure arms

- Move the `append_sender_turn` for the assistant turn after a successful send, or
  roll it back on send failure. Pick one and say which in the PR.
- On the timeout, LLM-error and cancellation arms, append a short synthetic
  assistant turn (e.g. `(previous attempt failed: timeout)`) so
  `normalize_cached_channel_turns` does not silently glue two user questions
  together.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 7: Make the JSON stripper linear

Check `is_line_isolated_json_segment`'s cheap left condition — that the candidate's
line prefix is blank — **before** attempting the parse. That rejects mid-line braces
in O(1) and removes most of the repeated parsing. Also skip candidates whose line
starts inside a fenced code block.

The function has dense existing coverage; it must all stay green.

**Verify**: `cargo test --lib channels::` → all pass.

## Test plan

New tests in this file's test module.

1. `panicking_worker_releases_the_next_message` — spawn a worker whose message path
   panics; assert a second message from the same sender is processed rather than
   hanging. Bound the test with a timeout so a regression fails instead of hanging CI.
2. `panicking_worker_releases_its_permit` — drive `max_in_flight` panicking messages,
   then assert a further message still gets a permit.
3. `route_selection_does_not_hold_two_locks` — the ordering is hard to assert
   directly; instead add a test that acquires the runtime-config store and then calls
   `get_route_selection` from the same task, which would deadlock under the old code.
   Bound it with a timeout.
4. `channel_error_replies_are_sanitized_before_delivery` — drive the real dispatch
   loop with a provider that fails with a secret-shaped token in its error, and
   assert the delivered reply carries `[REDACTED]` and not the token.

   **Correction to this plan as first written.** The original wording asked for a
   test that an absolute path does not survive into the reply. That is wrong:
   `sanitize_api_error` (`src/providers/mod.rs:812-825`) scrubs secret-shaped
   token prefixes and caps length — it does **not** strip filesystem paths. The
   sibling path at `:1687` does not strip them either, so path leakage is a
   separate pre-existing defect present on **both** arms, not something this
   change fixes. Do not widen scope to "fix" it here: `sanitize_api_error` is
   shared by every provider, so changing it belongs in its own plan with its own
   blast-radius review. Recorded as a new finding; see the note in
   `plans/README.md`.

   Assert against the call site, not against `sanitize_api_error` directly — a
   test that calls the sanitizer still passes when the call site is reverted, so
   it would prove nothing about this change.
5. `assistant_turn_is_not_recorded_when_delivery_fails` — with a channel whose `send`
   returns `Err`, assert history does not gain an assistant turn.
6. `timeout_leaves_no_unpaired_user_turn` — after a timeout, assert the next turn does
   not merge two user questions.
7. `json_stripper_is_linear_in_braces` — not a timing assertion; assert the parse
   attempt count (instrument behind `#[cfg(test)]`) does not grow with brace count on
   mid-line JSON.

**Mutation check (required).** For test 1, remove the `Drop` guard from step 1 and
confirm it **fails** (hangs to its timeout). For test 4, revert step 5 and confirm it
**fails**. Restore both.

**Verify**: `cargo test --lib channels::` → all pass, including all seven new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` passes, including the seven new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'eprintln!' src/channels/mod.rs` returns nothing
- [ ] `grep -n 'format!("⚠️ Error: {e}")' src/channels/mod.rs` returns nothing
- [ ] Every new test that could hang is bounded by a timeout
- [ ] No files outside `src/channels/mod.rs` are modified (`git status`)
- [ ] `plans/README.md` status row for 116 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — this chain is serialized over one file.
- The semaphore permit turns out **not** to be released by unwinding (step 1). That
  changes the fix shape and is worth confirming out loud rather than guessing.
- Moving the assistant-turn append (step 6) breaks a test that asserts the current
  ordering deliberately — read the test name and comment before assuming it is wrong.
- Any new test hangs rather than failing. A hanging test in CI is worse than the bug;
  bound it and report.
- Test 1 or 4 still passes after you revert the corresponding fix.

## Maintenance notes

- **What interacts with this**: plan 117 rewrites parts of the same reload path and
  plan 121 moves this code into new modules. Land in chain order.
- **What a reviewer should scrutinise**: that step 1's guard fires on **every** exit
  path including early returns, not just the happy one; and that step 6's chosen
  semantics (roll back vs append-after-send) is stated explicitly rather than left to
  the reader.
- **Deliberately deferred**: moving the config reload and the approval ack off the
  intake loop (they are awaited before the worker spawns, so one slow ack blocks
  every channel). That is a real finding, recorded as T3-07, and it needs its own
  plan — the reload's placement is load-bearing for owner changes applying before the
  next reply is authorized, so it cannot be moved casually.
