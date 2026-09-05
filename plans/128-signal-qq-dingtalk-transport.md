# Plan 128: Signal / QQ / DingTalk — dedup, backoff, ACK, SSE framing

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/signal.rs src/channels/qq.rs src/channels/dingtalk.rs src/channels/matrix.rs src/channels/nextcloud_talk.rs`
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged first.
> That is expected and is not a stop condition. Relocate by symbol name and continue.
> STOP only if the *code itself* no longer matches the "Current state" excerpt
> semantically — i.e. the logic changed, not its position.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: plans/115 (adds `apply_allowed_senders`)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Three channels share a family of transport defects, plus this plan carries the
allowlist-storage conversion for the five channels whose pairing currently grants
nothing.

The sharpest is a reconnect storm: DingTalk reports a WebSocket **error** to the
supervisor as a clean exit, and the supervisor's clean-exit arm marks a health error
*and* resets the backoff — treating one event as both failure and success. An expired
ticket or a rate limit therefore reconnects every two seconds forever, with no
escalation toward the 60-second cap, burning the exact API budget that backing off
would protect. Signal has its own version, resetting on connect rather than on a
healthy stream.

QQ's dedup evicts an arbitrary half of a `HashSet` under a comment claiming it evicts
the oldest — so a just-inserted id can be dropped, and a dedup miss costs a **complete
extra LLM turn** plus a duplicate reply, by far the most expensive thing per message
here. Signal drops any SSE chunk that splits a multi-byte character, so non-Latin
traffic is silently lossy. And DingTalk never ACKs the frames it filters out —
including every message from a not-yet-paired user, precisely the population the
pairing flow exists to serve.

## Current state

`src/channels/dingtalk.rs:236-243` — a transport error becomes a clean exit:

```rust
        while let Some(msg) = read.next().await {
            let msg = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    tracing::warn!("DingTalk WebSocket error: {e}");
                    break;
                }
```

`src/channels/mod.rs:1578-1600` — the supervisor's arm:

```rust
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
```

`src/channels/signal.rs:447` resets `retry_delay_secs = 2` the moment the HTTP
response is 2xx, before a single event is read; `:541` then sleeps a literal 2
seconds after the stream ends rather than using the backoff variable. This `listen()`
never returns `Err`, so the supervisor's own backoff never engages either.

`src/channels/qq.rs:211-217` — the dedup eviction:

```rust
        // Evict oldest half when at capacity
        if dedup.len() >= DEDUP_CAPACITY {
            let to_remove: Vec<String> = dedup.iter().take(DEDUP_CAPACITY / 2).cloned().collect();
```

`dedup` is a `HashSet<String>` (`:35`), whose iteration order is unspecified.

`src/channels/signal.rs:462-468` — the SSE chunk decode:

```rust
            let text = match String::from_utf8(chunk.to_vec()) {
                Ok(t) => t,
                Err(e) => { tracing::debug!("Signal SSE invalid UTF-8, skipping chunk: {}", e); continue; }
            };
```

`src/channels/dingtalk.rs:296`, `:316`, `:319-324` — all `continue` before the ACK,
which is only sent at `:342-351`. Compare `src/channels/lark.rs:541-547`, which ACKs
first with an explicit "Feishu requires within 3 s" comment.

`src/channels/dingtalk.rs:327-333` — two `session_webhooks` entries inserted per
message, no eviction anywhere, no use of the `sessionWebhookExpiredTime` DingTalk
ships in the same payload. `:184-215` — the read guard is held across the whole
outbound POST because the URL borrows from it.

`src/channels/dingtalk.rs:311` and `:95-101` — the reply URL is read from the inbound
payload and stored **before** the allowlist gate at `:319`, with no host check.

`src/channels/signal.rs:237-241` — anything that is neither E.164 nor a UUID becomes a
group id.

Allowlist storage as a plain `Vec<String>`, so `/claim` grants nothing until restart:
`src/channels/matrix.rs:26`, `src/channels/dingtalk.rs:17`, `src/channels/qq.rs:31`,
`src/channels/nextcloud_talk.rs:14`. (Lark's copy is plan 124's.)

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::signal channels::qq channels::dingtalk channels::nextcloud_talk` | all pass |
| Matrix (cannot build) | see STOP conditions | — |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/signal.rs`, `src/channels/qq.rs`,
`src/channels/dingtalk.rs`, and the **allowlist-storage conversion only** in
`src/channels/matrix.rs` and `src/channels/nextcloud_talk.rs`.

**Out of scope**: the supervisor's clean-exit arm in `src/channels/mod.rs` — plan 116
owns it, and this plan fixes the channels' side of the contract (return `Err` on a
fault) rather than the supervisor's side. Everything else in `matrix.rs` and
`nextcloud_talk.rs` — plan 129. The cross-file helper extraction — plan 129.

## Git workflow

- Branch: `fix/signal-qq-dingtalk-transport`
- Conventional commits, e.g. `fix(dingtalk): report a transport fault as an error, not a clean exit`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Return `Err` on a transport fault

Make `dingtalk::listen` return `Err` on the WebSocket error break, distinguishing it
from `Message::Close` (which is a clean server-side close and may stay `Ok`).

Do the same audit for Signal: its `listen()` never returns `Err`, so give it an error
return on a genuine transport failure.

State the contract in the PR: **`Ok(())` means cancellation or a closed sender; `Err`
means a transport fault.** Plan 129 writes it into the trait doc.

**Verify**: `cargo test --lib channels::dingtalk channels::signal` → all pass.

### Step 2: Reset Signal's backoff on a healthy stream, not on connect

Reset `retry_delay_secs` only after the stream has been alive past a threshold, or
after the first successfully parsed envelope. Use the backoff variable for the
post-stream sleep at `:541` instead of the literal `2`.

**Verify**: `cargo test --lib channels::signal` → all pass.

### Step 3: Make QQ's dedup evict in order

Keep the `HashSet` for O(1) membership and add a `VecDeque<String>` for FIFO
eviction; on overflow, pop the front of the deque and remove that key from the set.
Copy the bounded pattern at `src/channels/matrix.rs:231`.

**Verify**: `cargo test --lib channels::qq` → all pass.

### Step 4: Frame Signal's SSE stream properly

Accumulate into a `Vec<u8>`, split on `b'\n'`, and decode each complete line — so a
character split across a chunk boundary stays buffered instead of discarding the
chunk. Alternatively use `tokio_util::codec::LinesCodec` if it is already a
dependency; do not add one for this.

**Verify**: `cargo test --lib channels::signal` → all pass.

### Step 5: ACK DingTalk frames before filtering

Hoist the ACK to immediately after `parse_stream_data` succeeds, before the
empty-content, pairing-consumed and unauthorized-user filters — mirroring the Lark
websocket path.

**Verify**: `cargo test --lib channels::dingtalk` → all pass.

### Step 6: Bound and validate DingTalk's session-webhook map

Store `(url, expires_at)` using the expiry DingTalk already sends; drop expired
entries on insert and on lookup. Clone the URL out of the guard before dropping it
and issuing the request, so the read lock is not held across the POST.

Validate the URL before inserting: require `https` and a host matching a configurable
DingTalk domain suffix list. The value arrives on the message plane and is stored
ahead of the allowlist gate; today its integrity rests entirely on DingTalk's own.

**Verify**: `cargo test --lib channels::dingtalk` → all pass.

### Step 7: Reject unrecognised Signal recipients

Make `parse_recipient_target` return `Option<RecipientTarget>`, treat the `group:`
prefix as the only route to `Group`, and have `send()` bail with a clear message on
`None`.

**Verify**: `cargo test --lib channels::signal` → all pass.

### Step 8: Convert the allowlists and mirror pairing

For `qq.rs`, `dingtalk.rs`, `nextcloud_talk.rs` and `matrix.rs`: convert
`allowed_users` to `Arc<RwLock<Vec<String>>>`, add the `add_allowed_*_runtime`
mutator, call it after a successful pairing, and implement `apply_allowed_senders`.
Copy `src/channels/slack.rs:40` exactly — do not invent a variant, and use one lock
flavour across all four.

**Verify**: `cargo test --lib channels::qq channels::dingtalk channels::nextcloud_talk`
→ all pass.

## Test plan

1. `dingtalk_transport_error_returns_err` — **the plan's primary test**; a socket
   error must not surface as `Ok(())`.
2. `signal_backoff_does_not_reset_on_a_bare_connect`.
3. `qq_dedup_evicts_the_oldest` — insert past capacity and assert the most recent id
   is still present.
4. `signal_sse_survives_a_split_multibyte_character` — feed a CJK string split across
   two chunks; assert the message arrives intact.
5. `dingtalk_acks_a_filtered_frame` — a message from a non-allowlisted sender is still
   ACKed.
6. `dingtalk_rejects_an_off_domain_reply_url` — and an `http://` one.
7. `signal_rejects_a_malformed_recipient`.
8. `pairing_grants_immediate_access` — for each of the four converted channels.

**Mutation check (required).** For test 1, restore the `break`-into-`Ok(())` and
confirm it **fails**. For test 3, restore `iter().take(..)` and confirm it **fails**
(seed deterministically so the assertion is not flaky). Restore both.

**Verify**: the scoped test command → all pass, including all eight.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] The scoped tests pass, including the eight new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'iter().take(DEDUP_CAPACITY' src/channels/qq.rs` returns nothing
- [ ] The `Ok(())` vs `Err` contract is stated in the PR body
- [ ] `git diff --stat` shows only the allowlist-storage change in `matrix.rs` and
      `nextcloud_talk.rs` — nothing else in those two files
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 128 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — step 8 implements its trait method.
- `matrix.rs` cannot be compiled (it cannot — `matrix-sdk 0.16` overflows the rustc
  recursion budget, and no CI job builds it). Make the allowlist conversion by
  inspection, state in the PR that it is **unverified by any build**, and do not let
  that block the other four channels.
- Test 3 is flaky. `HashSet` iteration order is unspecified, so a test that
  occasionally passes under the old code proves nothing — seed it so the old code
  fails deterministically, or report that you could not.
- Hoisting the DingTalk ACK (step 5) turns out to ACK frames the gateway later
  rejects in a way that loses them. Report the shape rather than choosing.

## Maintenance notes

- **What interacts with this**: plan 116 fixes the supervisor's clean-exit arm, which
  is the other half of step 1's contract — until it lands, a channel returning `Err`
  escalates correctly but the `Ok(())` path still mis-resets. Plan 129 writes the
  contract into `traits.rs` and does the cross-file helper extraction.
- **What a reviewer should scrutinise**: that step 1 distinguishes a clean server
  close from a fault rather than making everything an error, and that step 8 used one
  lock flavour across all four channels rather than matching each file's local habit.
- **Deliberately deferred**: `qq.rs`'s ~350 unread lines in `listen`, and everything
  in `matrix.rs` beyond the allowlist field. Both are recorded as residual unread
  surface.
