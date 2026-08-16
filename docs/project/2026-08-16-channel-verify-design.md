# `channel verify`: what a round trip can actually prove unattended

**Date**: 2026-08-16
**Plan**: `plans/149-channel-verification-status.md`, step 4
**Status**: design recorded. Implementation is a separate decision, per the plan.

---

## 0. The short version

The plan scopes step 4 as "one scripted round trip per channel: send, receive,
assert echo". The **send** half is buildable today and worth building. The
**receive** half is not, and the reason is structural rather than a matter of
effort: on every platform in our roster a bot cannot deliver a message to
itself, so "receive" needs a *second identity* — a test user account, per
platform, with its own credentials and its own lifecycle.

That is a test-account provisioning story across eighteen services. Plan 149
names exactly this as the stop condition:

> Resist the pull toward a full E2E platform. If the design starts needing
> fixtures, replay, or a test-account provisioning story, that is the signal to
> stop and write it down instead.

So this is the writing-down. The recommendation is to build the achievable half
under an honest name, and to leave the echo half unbuilt with the reason on
record rather than half-built and misleading.

---

## 1. Why the echo half does not survive contact

`channel doctor` today calls `Channel::health_check()` — for most channels a
credential probe against the platform's API (Telegram `getMe`, Discord
`/users/@me`, and so on). It answers *"are these credentials live?"* and nothing
about delivery.

A round trip would answer more. To close it, something must **send to the bot**.
The options, and why each fails:

| approach | why it does not work |
|---|---|
| Bot messages itself | Telegram, Discord, Slack, and Mattermost all refuse bot→self delivery, or drop bot-authored messages before they reach our inbound path (the `is_bot` / `bot_id` filters we deliberately apply). |
| A second bot messages the first | Telegram bots cannot message other bots at all. Discord and Slack can be configured to allow it, but the receiving side's inbound filter drops bot authors — so we would be testing the filter's bypass, not the delivery path. |
| A human sends on cue | Works, and is exactly what the plan-127/135/147/148 live drives already ask for. It is not automation, and a manually-dispatched workflow cannot wait on it. |
| A recorded fixture replayed at the transport | This is the "fixtures, replay" the plan tells us to stop at. It also proves nothing about the platform — only about our parser, which unit tests already cover. |

The honest conclusion: **inbound verification is a human-in-the-loop activity on
every platform we support.** No amount of harness removes the second identity.

---

## 2. What `channel verify` should be instead

One strictly stronger step than `doctor`, and no more:

```
rantaiclaw channel verify <name> [--to <target>]
```

For each configured channel, in order:

1. **Credentials** — the existing `health_check()`. Unchanged; this is the
   `doctor` behaviour, reused rather than reimplemented.
2. **Delivery** — send one message to a **verification target** the operator
   configured, and assert the platform *accepted* it (2xx plus a platform
   message id where the API returns one). The message carries a nonce so an
   operator reading the chat can tell a verification ping from real traffic:

   ```
   RantaiClaw verification ping · <nonce> · <iso-8601>
   ```

3. **Report** — one line per channel, mirroring `doctor`'s shape and column
   widths so the two read as one family:

   ```
   🔎 RantaiClaw Channel Verify

     ✅ telegram  credentials ok · delivered (message_id 4417)
     ⚠️  discord   credentials ok · no verify target configured — skipped delivery
     ❌ slack     credentials ok · delivery refused (403 not_in_channel)
     ⏱️  irc       timed out (>10s)

   1 verified · 1 refused · 1 skipped · 1 timed out
   ```

**Exit code**: `0` only when every configured channel with a target verified.
Skipped channels do not fail the run but are counted and named, so "everything
passed" can never be produced by verifying nothing — the failure mode that makes
a green check worse than no check.

### The verification target

A new optional key per channel, `verify_target`, holding a chat/channel id the
operator owns. Not a new credential — it is an address, and an absent one means
"skip delivery for this channel", never "pass".

This is the one config addition the design needs. Everything else reuses what
`doctor` and the factory already build.

---

## 3. What it deliberately is not

- **Not an echo test.** See §1. The command's name and output say "delivered",
  never "verified end to end", because it cannot observe the far end.
- **Not per-PR CI.** Eighteen third-party dependencies in a required check would
  be red more often than green, and an ignored check is worse than none.
- **Not a fixture or replay harness.** Those test our parser, which is already
  covered, while implying platform coverage that is not there.
- **Not test-account provisioning.** That is the stop condition; if a future
  effort wants real round trips, it should start by deciding whether eighteen
  test accounts are a thing the project is willing to own.

---

## 4. The workflow

Manually dispatched, never on push or pull_request:

```yaml
name: Channel Verify
on:
  workflow_dispatch:
    inputs:
      channels:
        description: "Comma-separated channel names, or 'all'"
        default: "all"
```

It builds the binary, writes a config from repository secrets, and runs
`rantaiclaw channel verify`. Secrets are per channel and optional — a channel
whose secret is unset is skipped and reported as skipped, which is the same
contract the CLI has.

Two properties worth stating because they are easy to lose later:

- The job must **not** be added to the required-checks gate. It is an
  operator-run pre-release instrument, not a merge gate.
- It must never print a message body or a target id — verification traffic goes
  to a real chat, and the log is not a private surface.

---

## 5. Cost, honestly

The delivery half is small: every channel already implements `Channel::send`, so
the command is a loop over the factory's built channels plus one config key and
a report. The estimate is a day's work including tests, most of it in the
report's honesty (skipped-vs-passed accounting) rather than the sending.

The echo half is not costed here because the recommendation is not to build it.

---

## 6. Recommendation

Build §2. Leave §1 written down. Revisit only if the project decides it wants
eighteen platform test accounts, which is a much larger commitment than a
verification command and should be chosen on its own merits, not arrived at by
extending this one.
