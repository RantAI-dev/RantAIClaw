# Plan 129: Remaining platforms — listen contract, health checks, message splitting, and the cross-file extractions

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **This plan runs LAST among the per-platform plans.** It performs two changes that
> touch every platform file, so it must not race 124–128.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/ src/channels/traits.rs`
>
> **Line numbers in this plan WILL have drifted** — 124–128 will have merged before
> it. That is expected and is not a stop condition. Relocate by symbol name and
> continue. STOP only if the *code itself* no longer matches the "Current state"
> excerpt semantically.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/124, 125, 126, 127, 128 (all must be merged first)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Four things remain after the per-channel plans, and three of them are cross-file by
nature — which is why they are collected here rather than split.

**Only three of eighteen channels split long messages.** The other fifteen post the
whole rendered reply in one request, so any answer longer than the platform cap fails
the entire send. The user gets **nothing** rather than a chunked answer — and long
replies with code, tables and logs are exactly what this agent produces.

**Four webhook channels discard the platform message id** and mint a UUID, so a
redelivery is undetectable and the agent runs again. That QQ and the Lark websocket
path each hand-rolled their own dedup is the evidence this is operational, not
theoretical.

**`listen()`'s cancellation contract is undocumented and ignored by fourteen of
eighteen implementers**, and `health_check` is implemented with a real network probe
by sixteen channels and called by exactly one thing — a one-shot CLI command. The
daemon's own 30-second health tick marks every channel OK unconditionally.

**`pairing_profile_root()` is copy-pasted into fourteen files**, and the iMessage copy
silently dropped its error log.

## Current state

Splitting is present in `src/channels/discord.rs:214` (`format::split`),
`src/channels/telegram.rs:1258` (`split_paired`), and `src/channels/irc.rs:401` (its
own 512-byte line splitter, correctly divergent). Absent in: `slack.rs:107-113`,
`mattermost.rs:186-191`, `lark.rs:985-994`, `whatsapp.rs:320`,
`whatsapp_web.rs:348`, `signal.rs:387`, `qq.rs:234`, `dingtalk.rs:194`,
`nextcloud_talk.rs:312`, `linq.rs:321`, `matrix.rs:552`.

Platform id discarded: `whatsapp.rs:288`, `linq.rs:295`, `lark.rs:647` and `:959`,
`dingtalk.rs:355` — all `Uuid::new_v4()` while the payload's id is parsed a few lines
above. Done correctly in `nextcloud_talk.rs:257`, `mattermost.rs:423`,
`slack.rs:260`, `matrix.rs:653`, `email_channel.rs:448`.

`src/channels/traits.rs:97-101` — the `cancel: CancellationToken` parameter has **no
doc comment**. Honoured by discord, slack, telegram, whatsapp_web; discarded as
`_cancel` by fourteen others. The supervisor compensates at
`src/channels/mod.rs:1564-1572` with a comment calling itself "a backstop for
channels that ignore the token".

`src/channels/traits.rs:104-106` — `health_check` with a `true` default, overridden
by sixteen channels. Its only production caller is `doctor_channels`. The supervisor's
health loop (`src/channels/mod.rs:1551-1572`) calls
`crate::health::mark_component_ok(&component)` unconditionally and never consults it.

`src/channels/imessage.rs:54-58` — the drifted helper copy:

```rust
        ProfileManager::active().ok().map(|p| p.root)
```

against the identical `match` body with a `tracing::warn!` in thirteen others
(`discord.rs:102` is a good canonical reference).

Two `health_check`s cannot fail for the condition they exist to catch:
`src/channels/slack.rs:280-288` returns `r.status().is_success()` while Slack returns
HTTP 200 with `{"ok":false}` for a revoked token — and the same file's `send()`
checks the `ok` field correctly at `:138-145`. `src/channels/whatsapp_web.rs:597-600`
is fixed by plan 123.

`src/channels/linq.rs:336`, `:421`, `:438` — the recipient is interpolated raw into
the API URL, while `src/channels/nextcloud_talk.rs:277-280` correctly percent-encodes
its room segment.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Lint (lark) | `cargo clippy --features channel-lark --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/traits.rs`, `src/channels/pairing.rs`, and the platform
files for the four changes below only.

**Out of scope**: anything 124–128 already fixed in their files; `src/channels/mod.rs`
(the supervisor's health tick belongs to the group-A chain — this plan only documents
the contract, it does not change the supervisor); `format::split`'s internals (plan
131).

## Git workflow

- Branch: `fix/remaining-platforms-and-cross-file-cleanups`
- **One commit per step.** Steps 3 and 4 touch many files; keeping them separate is
  what makes the diff reviewable.
- Conventional commits, e.g. `fix(channels): split long replies on every platform`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Write the `listen()` contract into the trait, then honour it where it matters

Document `cancel` on `src/channels/traits.rs:97`: **`Ok(())` means cancellation or a
closed sender; `Err` means a transport fault; implementations SHOULD return promptly
on `cancel`, and the supervisor's drop is a backstop, not the contract.** Plan 128
established this for three channels; here it becomes the written rule.

Then retrofit **only** the channels where an abandoned future leaks something real —
IRC (needs to send `QUIT`), Email (needs `LOGOUT`), DingTalk and QQ (WebSocket close
frames), Lark (its axum server needs `with_graceful_shutdown`, or an immediate
restart hits "address already in use"). Leave the rest; a documented contract plus a
working backstop is enough for channels with no teardown obligation, and retrofitting
fourteen loops is not worth the churn.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 2: Make `health_check` mean something, or say that it does not

`health_check` is paid for by sixteen implementations and consulted by one CLI
command. Pick one and say which in the PR:

- **Preferred**: have the supervisor's health tick call it under the same 10-second
  timeout `doctor_channels` already uses, with a small consecutive-failure threshold
  so one flaky probe does not flap the component state. **The timeout is not
  optional** — a hung probe on a 30-second tick would stall the supervise loop.
- **Acceptable**: leave the tick as it is and document on the trait that
  `health_check` is a diagnostic-only surface, so nobody assumes the daemon uses it.

Either way, fix Slack's probe to require `ok == true`, reusing the check its own
`send()` already performs. A probe that cannot fail for a revoked token is worse than
no probe.

**Verify**: `cargo test --lib channels::slack` → all pass.

### Step 3: Split long messages on every platform that needs it

For each of the eleven channels listed in "Current state", add a
`const *_MAX_MESSAGE_LENGTH` and route `send()` through `format::split`, following
`src/channels/discord.rs:213-245`. Include the inter-chunk pacing where the platform
needs it.

**The limit constant is the risk in this step, not the code.** Getting one wrong
either truncates or fails. Cite your source for each constant in the PR — platform
documentation, not inference — and where you cannot find one, say so and leave that
channel unsplit rather than guessing.

Start with Slack, Mattermost and both WhatsApps: lowest limits, highest traffic.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 4: Carry the platform message id

In `whatsapp.rs`, `linq.rs`, `lark.rs` (both paths) and `dingtalk.rs`, use
`format!("{channel}_{platform_id}")` for `ChannelMessage.id`, falling back to a UUID
only when the platform id is genuinely absent.

Then replace QQ's and Lark's hand-rolled dedup with one shared bounded helper, since
a real id now makes a shared implementation possible.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 5: Extract the duplicated profile-root helper

Add `pub fn profile_root(channel: &str) -> Option<PathBuf>` to
`src/channels/pairing.rs` with the canonical `match` body and a `{channel} pairing: …`
warning. Replace all fourteen private copies. iMessage gains the missing error log as
a side effect — call that out in the PR, because it is the one behavioural change in
an otherwise mechanical step.

**Verify**: `grep -rn 'fn pairing_profile_root' src/channels/` returns nothing.

### Step 6: Percent-encode the Linq recipient

Wrap the recipient path segment at `linq.rs:336`, `:421` and `:438` in
`utf8_percent_encode(.., NON_ALPHANUMERIC)`, or validate it against the expected
chat-id shape first. The value originates in the inbound webhook payload and the
request carries a bearer token.

Check against real Linq chat ids before encoding — if a literal `/` is meaningful in
their id format, encoding it breaks delivery. Report rather than guess.

**Verify**: `cargo test --lib channels::linq` → all pass.

## Test plan

1. `long_reply_is_split_on_<channel>` — one per channel changed in step 3, asserting
   the reply arrives as multiple chunks and none exceeds the limit.
2. `platform_message_id_is_carried` — one per channel changed in step 4.
3. `redelivered_message_is_deduplicated` — using the real id.
4. `slack_health_check_fails_on_ok_false` — a 200 response with `{"ok":false}` must
   report unhealthy.
5. `profile_root_logs_on_failure` — including for iMessage, which previously did not.
6. `linq_recipient_with_a_metacharacter_is_encoded`.
7. If step 2 chose the preferred option: `health_tick_marks_error_on_a_failing_probe`
   and `a_hung_probe_does_not_stall_the_tick`.

**Mutation check (required).** For test 4, restore `r.status().is_success()` and
confirm it **fails**. For one channel in test 1, remove the split call and confirm it
**fails**. Restore both.

**Verify**: `cargo test --lib channels::` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0, and again with `--features channel-lark`
- [ ] `cargo test --lib channels::` passes, including all new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -rn 'fn pairing_profile_root' src/channels/` returns nothing
- [ ] `grep -rn 'Uuid::new_v4()' src/channels/` returns no hit in the four channels of step 4
- [ ] Every message-limit constant added in step 3 has its source cited in the PR
- [ ] `git log --oneline` shows one commit per step
- [ ] `plans/README.md` status row for 129 updated

## STOP conditions

Stop and report back if:

- Any of plans 124–128 is unmerged. This plan touches their files.
- You cannot find an authoritative message-limit for a platform. Leave that channel
  unsplit and say so — a wrong constant fails sends in a way that looks like an
  outage.
- Encoding the Linq recipient breaks a real chat-id format.
- Step 2's preferred option would call a probe without a timeout anywhere in the path.
  Stop; an unbounded probe on the supervise loop is worse than the current
  unconditional OK.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 131 fixes `format::split`'s empty-chunk contract
  — step 3 multiplies the number of callers exposed to it, so 131 should land close
  behind. Plan 141 tests the allowlist gate as an applied guard and needs the handler
  extraction this plan does not do.
- **What a reviewer should scrutinise**: the eleven limit constants (each is a
  separate factual claim), and that step 5 is a pure move except for the iMessage log.
- **Deliberately deferred**: retrofitting cancellation into the nine channels with no
  teardown obligation, and the ~2,150 unread production lines across the platform
  files. Both are recorded; neither is worth the churn now.
