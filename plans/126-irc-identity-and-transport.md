# Plan 126: IRC — account-based identity, dead-writer reset, flood pacing

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat f189422..HEAD -- src/channels/irc.rs`
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged first.
> That is expected and is not a stop condition. Relocate by symbol name and continue.
> STOP only if the *code itself* no longer matches the "Current state" excerpt
> semantically — i.e. the logic changed, not its position.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/115 (adds `apply_allowed_senders`)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

An IRC nick is a first-come lease, not an identity. Anyone who connects while the
owner is offline — or forces them off via a netsplit or ghost — takes the owner's
nick and is resolved as that owner: full toolset, plus authority to approve shell
commands. Unlike the email case there is no mitigating factor; the impersonator also
receives the agent's replies.

Three transport defects compound it. After any disconnect the channel keeps a dead
write half, so `send()` writes into a half-closed socket, returns `Ok(())`, and the
reply is lost with no error anywhere. Replies are sent as back-to-back protocol lines
with no pacing, which most networks disconnect for as excess flood — so longer
replies fail more reliably than short ones. And the health check dials a fresh
TCP+TLS connection every heartbeat, which is the thing most likely to get the bot
K-lined.

## Current state

`src/channels/irc.rs:641` — the identity:

```rust
                        sender: sender_nick.to_string(),
```

`:426-516` — SASL negotiation exists but authenticates **the bot's own connection**;
the client never requests `account-tag` or `extended-join`, so no per-message account
information is available. `:600-620` — `/claim` persists the raw nick into
`allowed_users` and `approval_owners`.

`:262-270` — `is_user_allowed` compares with `eq_ignore_ascii_case`, while the owner
gate in `src/approval/mod.rs:258-267` compares case-**sensitively**. The two gates
disagree; plan 122 owns the owner-gate half.

`:447` — `*guard = Some(writer);` is the only assignment. No path in `listen()` — the
read timeout at `:457`, the `n == 0` bail at `:462`, or any `?` — resets it to `None`.
`:383-386` — `send()` treats `Some(writer)` as connected.

`:401-405` — all PRIVMSG chunks written back-to-back:

```rust
        for chunk in chunks {
            Self::send_raw(writer, &format!("PRIVMSG {} :{chunk}")).await?;
        }
```

Compare `src/channels/discord.rs:241-243`, which sleeps 500 ms between chunks.

`:667-677` — `health_check` opens a fresh TCP+TLS connection, writes `QUIT`, and
drops it without a shutdown.

`:567-575` — on `433` the nick gains one `_` per collision with no cap.

`:299-322` — when `verify_tls` is false the client is built with
`.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify))`; `NoVerify`
(`:334-372`) accepts any peer. No warning is logged, and the same config carries
`sasl_password`, `nickserv_password` and `server_password`. Default is safe
(`verify_tls.unwrap_or(true)`).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::irc` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/irc.rs`

**Out of scope**: the owner-gate case-sensitivity in `src/approval/mod.rs` (plan 122);
the cross-file `pairing_profile_root` extraction (plan 129); message splitting policy
— IRC's own splitter is correct and deliberately divergent because its limit is 512
**bytes per line**, which the shared char-budget splitter cannot express.

## Git workflow

- Branch: `fix/irc-identity-and-transport`
- Conventional commits, e.g. `fix(irc): require an account tag before granting owner authority`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Ask for an account, and use it

Add `account-tag` (and `extended-join`) to the `CAP REQ` at `:428`. Carry the
resulting account name as the primary `sender`, demoting the nick to
`sender_aliases`.

**Refuse owner authority when no account tag is present.** Keep the nick for the
chat allowlist so networks without services still work, but the owner gate must fail
closed. Document that `approval_owners` on IRC holds NickServ account names.

**Verify**: `cargo test --lib channels::irc` → all pass.

### Step 2: Clear the writer when the listener exits

Set `*self.writer.lock().await = None` on **every** exit path from `listen()` — use a
small RAII guard, or restructure the loop body into an inner function whose result is
handled once. `send()` already has an "IRC not connected" error path, so clearing the
slot routes into existing behaviour.

**Verify**: `cargo test --lib channels::irc` → all pass.

### Step 3: Pace the chunk loop

Insert a short delay (or a small token bucket, roughly two lines per second after a
brief burst) between `send_raw` calls, mirroring `discord.rs:241-243`. Cap the `433`
nick-retry loop so a rejecting server does not produce an unbounded NICK flood.

**Verify**: `cargo test --lib channels::irc` → all pass.

### Step 4: Make the health check cheap

Report on the live session rather than dialling — once step 2 lands,
`self.writer.lock().await.is_some()` is a truthful liveness signal. If a real probe
is wanted, rate-limit it to well under the network's connect budget.

**Verify**: `cargo test --lib channels::irc` → all pass.

### Step 5: Warn when TLS verification is off

Emit a `tracing::warn!` naming the server on the `NoVerify` branch, and refuse the
combination of `verify_tls = false` with any configured password unless a second
explicit opt-in is set. SASL sends the password as reversible base64, so a
credentialed link with no peer authentication is a credential disclosure.

Recommend rotating any credential used over such a link.

**Verify**: `cargo test --lib channels::irc` → all pass.

### Step 6: Implement the runtime allowlist trait method

Implement `apply_allowed_senders` (plan 115) and convert `allowed_users` to
`Arc<RwLock<Vec<String>>>` if it is not already, copying the shape from
`src/channels/slack.rs:40`.

**Verify**: `cargo test --lib channels::irc` → all pass.

## Test plan

1. `nick_without_an_account_tag_is_not_an_owner` — **the plan's primary test**.
2. `account_tag_is_the_primary_sender_and_the_nick_is_an_alias`.
3. `send_after_listener_exit_is_an_error` — assert `send()` returns `Err`, not `Ok(())`.
4. `chunks_are_paced` — assert the delay is applied between writes (inject a clock or
   assert on the call sequence; do not sleep in the test).
5. `nick_retry_is_capped`.
6. `verify_tls_false_with_a_password_is_refused_without_the_opt_in`.
7. `allowlist_edit_reaches_the_channel`.

**Mutation check (required).** For test 1, remove the account requirement and confirm
it **fails**. For test 3, restore the writer-retention behaviour and confirm it
**fails**. Restore both.

**Verify**: `cargo test --lib channels::irc` → all pass, including all seven.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::irc` passes, including the seven new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'account-tag' src/channels/irc.rs` returns a hit in the CAP request
- [ ] The health check no longer opens a connection
- [ ] The PR body states the credential-rotation recommendation from step 5
- [ ] No files outside `src/channels/irc.rs` are modified (`git status`)
- [ ] `plans/README.md` status row for 126 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — step 6 implements its trait method.
- The IRC crate in use does not expose `account-tag` per message. Do not infer
  identity from anything else; report it and ship steps 2–6 with owner authority on
  IRC disabled entirely, stating that plainly.
- Clearing the writer (step 2) breaks a reconnect path that relied on the stale
  handle. That would mean reconnection was depending on the bug; report the shape.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 122 makes the owner gate case-consistent; plan
  129 extracts the duplicated `pairing_profile_root` helper across all platform files
  including this one.
- **What a reviewer should scrutinise**: that step 1 fails **closed** on a network
  with no account services rather than falling back to the nick for owner decisions.
- **Deliberately deferred**: IRC's own 512-byte splitter stays. It is correctly
  divergent, documented in place, and is not drift.
