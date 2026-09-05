# Plan 123: WhatsApp Web — lifecycle leaks, allowlist bypass, credential logging, session store

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/whatsapp_web.rs src/channels/whatsapp_storage.rs src/channels/qr_terminal.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: plans/115 (adds the `apply_allowed_senders` trait method)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

This is the most exposed and least protected file in the subsystem: 1,052 production
lines with **no test module at all** — the only production file in `src/channels`
with none — shipping in every release binary because `whatsapp-web` is a default
feature, sitting on a pre-1.0 third-party reimplementation of the Signal protocol
that parses untrusted network input.

Four things need fixing before anything else here matters.

Every message body is logged at INFO, including the pairing codes that promote their
holder to owner, **before** the pairing handler runs. The recipient allowlist does
not apply to the address form the agent actually sends, so it provides zero outbound
containment. Every listener restart leaks a live client, a sync worker and a
device-saver onto the same SQLite session file — N restarts, N concurrent writers on
one Signal store. And the session database holding the account's long-term keys is
created at the process umask while every other credential store in this repo is 0600.

Underneath, the store itself has a panic path on malformed key blobs, an asymmetric
MAC round trip, and a snapshot that copies a WAL database without its sidecar —
which produces exactly the malformed blobs the panic path chokes on.

## Current state

### 1. Message bodies, including pairing codes, logged at INFO

`src/channels/whatsapp_web.rs:435-441`:

```rust
                            tracing::info!(
                                "WhatsApp Web message from {} in {}: {}",
                                sender,
                                chat,
                                text
                            );
```

The pairing interception is at `:468` — **after**. No other channel logs message
content; the nine that log anything log only unauthorized-sender warnings.

### 2. The allowlist does not apply to the form the agent sends

`:243-245` and `:334-344`:

```rust
    fn is_jid(recipient: &str) -> bool {
        recipient.trim().contains('@')
    }
```

```rust
        // Validate recipient allowlist only for direct phone-number targets.
        if !Self::is_jid(&message.recipient) {
```

`:288-296` — `resolve_reply_target` always returns a JID string, and that becomes
`ChannelMessage.reply_target` at `:513`, which comes back as `SendMessage.recipient`.
So every agent-driven reply takes the bypass. Same shape in `start_typing`
(`:608-617`) and `stop_typing` (`:636-645`).

A dropped send also returns `Ok(())` (`:337-343`), so the agent records a delivered
reply that was never transmitted.

### 3. The LID gate fails open

`:484-490`:

```rust
                            let is_allowed = if is_lid && resolved_pn.is_none() {
                                let allowed = allowed_numbers.read().ok();
                                allowed.is_some_and(|a| a.iter().any(|n| n == "*") || !a.is_empty())
```

`!a.is_empty()` subsumes the wildcard test, so configuring **any** allowlist entry
admits every unmapped-LID sender. The unverified LID then becomes `sender` formatted
as `+digits` (`:303-309`, `:512`), indistinguishable from a phone number in logs.

### 4. Restarts leak clients, workers and savers

`:574` — `bot.run()` returns one handle; `:590-592` aborts only that, and never
awaits it. The vendored crate spawns two more things that are never cancelled: a sync
worker holding an `Arc<Client>` while the only `Sender` lives inside that same client
(so `recv()` can never return `None`), and an unconditional infinite device-saver
started from inside `build()` that writes to the session DB every 30 seconds.

`:584` — a `tokio::signal::ctrl_c()` arm inside `listen()`'s `select!` returns
`Ok(())` independently of the app's shutdown token, which the supervisor reads as an
unexpected exit and restarts.

`:597-600` — `health_check` returns whether a handle is `Some`, and the handle is
never cleared on `LoggedOut` or `StreamError`, so a dead channel reports healthy.

`:550` — a `_ => {}` arm swallows every terminal event: `Disconnected`,
`ConnectFailure`, `StreamReplaced`, `TemporaryBan`, `ClientOutdated`, `PairError`,
and `UndecryptableMessage`.

### 5. Pairing never ends, and pairs over unreadable sessions

`:725` declares `PairOptions.timeout`; `:748-870` never reads it, so
`PairEvent::Timeout` has exactly one occurrence in the repo — the consumer arm that
handles it.

`:761-861` — a detached `std::thread` with its own runtime, no handle, no
cancellation; `:853` awaits a bot handle that only resolves when the event loop
dies, and it auto-reconnects.

`:392-398` — `listen()` correctly bails when an existing device fails to load;
`:780-786` — `pair_once` ignores both results, so a corrupt or unreadable session DB
looks identical to "no session" and the wizard pairs a fresh device **over existing
key material**.

`:463-478` — the pairing branch runs before the allowlist gate with no throttle.

`:220` — the pairing reply goes to the raw `chat_jid`, not through
`resolve_reply_target`, so on LID-addressed DMs the confirmation lands in a thread
the user cannot see.

`:762` — `Runtime::new().expect("runtime")` in a detached thread; the panic drops
`tx`, so the operator sees "Pairing failed: channel closed" with the real cause
nowhere.

`:729-735` — `PairOptions::default()` sets `session_path: PathBuf::from("wa.db")`, a
relative path.

### 6. The session store

`src/channels/whatsapp_storage.rs:79-96` — `create_dir_all` + `Connection::open`
with no `set_permissions`; `:1248-1250` — snapshots inherit it. Contrast
`src/config/schema.rs:3969` (config 0600), `src/security/pairing_store.rs:137-142`,
`src/security/secrets.rs:192`.

`:1180-1183` — `copy_from_slice` into `[0u8; 64]` and `[0u8; 32]` with no length
check, while the three blobs above at `:1146-1151` **are** checked. A malformed row
panics inside the rusqlite row callback on the connect path.

`:579` stores `value_mac` as `serde_json::to_vec(..)`; `:604` reads it back raw. The
sibling `index_mac` is symmetric on both sides.

`:1242-1259` — `snapshot_db` is `std::fs::copy` of the main file only, while
`:90-93` sets `journal_mode = WAL` — so the sidecar, holding every transaction since
the last checkpoint, is not copied.

`:543-552` — `get_version` turns `QueryReturnedNoRows` into a database error, while
13 sibling readers map it explicitly.

`:577-587`, `:623-630`, `:674-679` — multi-row writes with no transaction.

`:729-733` — the reverse LID lookup filters on `phone_number` with no index on that
column; it runs per inbound message.

`:1237-1240` — `create()` returns the device id without inserting a row, so a
following `exists()` returns false.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::whatsapp` | all pass |

`whatsapp-web` is in the default feature set, so no extra flag is needed.
**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/channels/whatsapp_web.rs`
- `src/channels/whatsapp_storage.rs`
- `src/channels/qr_terminal.rs` — the stderr exposure only

**Out of scope**:
- `src/onboard/provision/whatsapp_web.rs` and the headless raw-payload print — plan
  132 owns the print; the provisioner's consumer loop is plan 133.
- Removing `whatsapp-web` from the default feature set, and the `ureq` second HTTP
  stack — plan 145 (a packaging decision, not a bug fix).
- Message splitting at WhatsApp's limit — plan 129.
- Patching the vendored `wa-rs` crate. If step 4 cannot be done without upstream
  changes, say so and implement the containment half only.

## Git workflow

- Branch: `fix/whatsapp-web-lifecycle-and-exposure`
- Conventional commits, e.g. `fix(whatsapp-web): stop logging message bodies and pairing codes`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Stop logging message content, and move pairing above logging

Drop `text` from the INFO line. Log sender/chat and, at most, `text.len()` at DEBUG.
Move the pairing interception **above** any logging of `text`.

Because codes already emitted are burned, pair this with expiring outstanding
pairing codes rather than trusting log cleanup — state that in the PR body.

Do the same for `qr_terminal.rs`: only render the pair code and QR when stderr is a
TTY (`IsTerminal`); on a non-TTY path log a pointer instead of the value, since a
managed daemon's stderr is captured by the journal.

**Verify**: `grep -n 'message from .*: {}' src/channels/whatsapp_web.rs` returns nothing.

### Step 2: Apply the allowlist to JID recipients, and fail loudly

Parse the recipient into a JID first, then apply the allowlist to any
`s.whatsapp.net` user JID and to `@lid` after PN resolution. Leave `g.us` and
broadcast as a **documented** exemption — a naive fix breaks group replies.

Replace the `return Ok(())` on a blocked send with `anyhow::bail!`, so a dropped
send is an error the agent can see. Check the gateway and agent reply paths for
callers that ignore the `Result`.

**Verify**: `cargo test --lib channels::whatsapp` → all pass.

### Step 3: Close the LID fail-open

For an unmapped LID, admit only on an explicit `"*"` wildcard or an explicit LID
entry in `allowed_numbers`. Otherwise drop with a warning that prints the LID so the
operator can allowlist it.

Keep the LID form visibly distinct from E.164 in `sender` (e.g. a `lid:` prefix) so
it can never be confused with a phone number in configs or logs.

Implement `apply_allowed_senders` (plan 115's trait method) while you are here.

**Verify**: `cargo test --lib channels::whatsapp` → all pass.

### Step 4: Make restart survivable

- Guard `listen()` against re-entry: if a handle exists, abort **and await** it before
  building a new client.
- Await the aborted handle before returning, so the old socket is not still draining
  when the new one dials.
- Delete the `tokio::signal::ctrl_c()` arm; the passed cancellation token is already
  in the same `select!`.
- Clear the handle on `LoggedOut` / `StreamError` so `health_check` can report false.
- Replace the `_ => {}` arm with explicit handling of the terminal variants: mark a
  health error and return from `listen()` so the supervisor restarts, **except**
  `LoggedOut` and `TemporaryBan`, which should stop and demand re-pair. Log
  `UndecryptableMessage` at warn with the sender.

If the leaked wa-rs internals cannot be stopped without an upstream change, implement
everything above anyway and record the residual leak in the PR plus a note in
`Cargo.toml` next to the pin.

**Verify**: `cargo test --lib channels::whatsapp` → all pass.

### Step 5: Make pairing terminate and refuse to pair over a broken session

- Wrap the pairing wait in `tokio::time::timeout(opts.timeout, ..)` and emit
  `PairEvent::Timeout` on elapse, then shut the runtime down.
- Give `pair_once` a `CancellationToken` (or return a guard) and abort the bot on
  `Connected` / `Failed` / receiver-dropped, ending with `Runtime::shutdown_timeout`.
- Match both session-load results and emit `PairEvent::Failed` with the underlying
  error, matching `listen()`'s behaviour.
- Replace `expect("runtime")` with a matched `Result` that sends `PairEvent::Failed`.
- Add a per-sender attempt counter with cooldown before the pairing store probe.
- Route the pairing reply through `resolve_reply_target`, as the forward path does.
- Drop the `Default` impl for `PairOptions`, or default `session_path` to the profile
  root — never a relative path for key material.

**Verify**: `cargo test --lib channels::whatsapp` → all pass.

### Step 6: Harden the session store

- `0600` on the DB and `0700` on its parent right after creation, and on every
  snapshot, under `#[cfg(unix)]`. Follow `src/security/pairing_store.rs:137-142`.
- Extend the length guard at `:1146` to cover the signature and ADV secret blobs, or
  use `try_into()` mapped through the existing error helper.
- Make the mutation MAC round trip symmetric — store both MACs as raw blobs and drop
  the `serde_json` hop on both sides. The table is derived state, so clearing it once
  on upgrade is acceptable; say so in the PR.
- Replace the snapshot's `fs::copy` with `VACUUM INTO` (or the online backup API)
  against the live connection, and create the companion file 0600.
- Map `QueryReturnedNoRows` in `get_version` to a default, matching the 13 siblings.
- Wrap the three multi-row writes in one transaction each, preparing the statement
  once outside the loop.
- Add an index on `(phone_number, device_id, updated_at DESC)`.
- Make `create()` either insert a row or be documented as single-device with an
  explicit tie to `exists()`'s behaviour.
- Add `PRAGMA busy_timeout`, matching `src/channels/history_store.rs:55`.

Recommend re-linking the device in the PR if the DB was previously world-readable.

**Verify**: `cargo test --lib channels::whatsapp` → all pass.

### Step 7: Extract three pure seams and read what remains

This file has no tests because almost nothing in it is callable without a network.
Extract:

- `classify(&Event) -> Action` — the step-4 match body
- `allow_recipient(recipient, allowlist) -> Decision` — the step-2/3 gate
- `map_inbound(msg, info) -> ChannelMessage` — including carrying `info.id` into
  `ChannelMessage.id` and the message timestamp into `timestamp` with a **checked**
  conversion, instead of minting a UUID and `Utc::now()`

Also switch the event handler's `tx.send(..).await` to `try_send` or `send_timeout`
with a warn-and-drop path, so a busy agent cannot park the protocol loop; and remove
the write-only `tx` field, or clear it alongside `client` on shutdown.

Then read the ~650 production lines of this file the audit never opened and report
anything new in the PR body without fixing it here.

**Verify**: `cargo test --lib channels::whatsapp` → all pass.

## Test plan

This file currently has **zero** tests. The three seams from step 7 plus the existing
tempdir-testable `handle_pairing_for` harness make most of this plan unit-testable
without a network.

1. `allow_recipient_applies_to_jid_form` — table over `+1555…`, `…@s.whatsapp.net`,
   `…@lid`, `…@g.us`, each against an empty, a specific, and a wildcard allowlist.
2. `unmapped_lid_is_rejected_when_the_allowlist_is_non_empty` — and admitted on `"*"`.
3. `blocked_send_is_an_error` — `send()` to a non-allowlisted number is `Err`.
4. `classify_marks_terminal_events` — `LoggedOut`, `StreamReplaced`, `TemporaryBan`,
   `ClientOutdated` each map to the intended action.
5. `map_inbound_carries_the_platform_id_and_timestamp`.
6. `pair_once_times_out` — with a 1 ms timeout, yields `Timeout`.
7. `pair_once_fails_on_an_unreadable_session` — point it at a garbage DB file; assert
   `Failed`, not a fresh pairing.
8. `pairing_reply_uses_the_resolved_target` — a LID chat maps to the PN thread.
9. Storage round trips: identity, session, prekey, signed prekey, sender key, sync
   key, mutation MAC — put → get → overwrite → delete → get-returns-None.
10. `malformed_key_blob_is_an_error_not_a_panic`.
11. `snapshot_is_readable_after_a_write` — write, snapshot, open the snapshot, assert
    the row is present (this is what the WAL bug breaks).
12. `session_db_is_0600` — mirror `src/security/pairing_store.rs:434-439`.
13. `no_message_body_is_logged_at_info` — capture with a `tracing` subscriber.

**Mutation check (required).** For test 1, restore `!Self::is_jid(..)` and confirm it
**fails**. For test 13, restore the `text` argument and confirm it **fails**. For
test 10, remove the length guard and confirm it **fails** (panics, which the test
must treat as a failure — use `catch_unwind` or assert on the `Result`). Restore all
three.

**Verify**: `cargo test --lib channels::whatsapp` → all pass, including all thirteen.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::whatsapp` passes, including all thirteen new tests
- [ ] All three mutation checks performed and failed as expected
- [ ] `src/channels/whatsapp_web.rs` has a `#[cfg(test)] mod tests` block where it
      previously had none
- [ ] `grep -n 'ctrl_c' src/channels/whatsapp_web.rs` returns nothing
- [ ] `grep -n '_ => {}' src/channels/whatsapp_web.rs` returns nothing in the event match
- [ ] `grep -n 'fs::copy' src/channels/whatsapp_storage.rs` returns nothing
- [ ] Step 7's read-through is reported in the PR body
- [ ] The PR body states the pairing-code and device re-link guidance
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 123 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — step 3 implements its trait method.
- The leaked wa-rs sync worker and device-saver cannot be stopped from this side.
  Implement the containment half, record the residual, and do **not** vendor-patch
  the crate as part of this plan.
- Applying the allowlist to JIDs breaks group replies in a way the `g.us` exemption
  does not cover — report the shape rather than widening the exemption until it
  passes.
- Changing the mutation-MAC encoding turns out to invalidate live session state in a
  way that forces a re-pair for existing users. That is an operator-visible cost and
  they should decide.
- Any of the three mutation checks still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 145 decides whether `whatsapp-web` stays in the
  default feature set — if it moves out, this file stops shipping to every user and
  the urgency of several items here drops. Plan 133 fixes the provisioner that feeds
  it. Plan 129 adds message splitting.
- **What a reviewer should scrutinise**: that step 1's reordering puts pairing above
  **all** logging of `text`, not just the INFO line; and that step 2's group exemption
  is documented in code rather than implied by the JID suffix check.
- **Deliberately deferred**: the `ureq` second HTTP stack (plan 145), and capping and
  delimiting inbound text before it reaches the model. The latter is real but the
  right fix is a shared untrusted-content boundary in the channel pipeline, not a
  WhatsApp-only wrapper — record it for a pipeline-level plan rather than solving it
  once here.
