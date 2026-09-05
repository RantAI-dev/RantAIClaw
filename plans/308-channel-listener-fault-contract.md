# Plan 308: Make every channel listener report faults the way the supervisor expects

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/channels/`

## Status

- **Priority**: P1 (ledger W2-2) · **Effort**: M–L · **Risk**: LOW–MED
- **Category**: bug
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

`Channel::listen`'s contract says a transport or auth fault returns `Err`, and the supervisor
resets its backoff only on `Ok`. Ten of sixteen channels return `Ok` or loop internally
instead. The consequence is not a slow reconnect — it is that **backoff never escalates**: a
revoked token becomes a two-to-five second reconnect storm against the platform, while
`health_check` still reports green because it asks a different question.

Twelve listeners also ignore the `CancellationToken`, so shutdown relies on the future being
dropped and no protocol teardown happens — no IRC `QUIT`, no IMAP `LOGOUT`, no clean WebSocket
close. The four compliant channels (DingTalk, QQ, IRC, WhatsApp Web) show the shape.

## Steps

1. **Confirm the list at HEAD.** For each channel, find where a transport or auth error is
   handled in `listen` and classify: returns `Err`, swallows to `Ok`, or loops internally.
   Put the table in the PR description. The audit named ten; verify rather than trust.
2. **Fix them in small groups, not one PR.** Suggested split by risk: the polling channels
   (Slack, Mattermost), then the WebSocket ones (Discord, Lark), then the long-lived
   connections (Signal, Email, Telegram, Matrix if it still exists after plan 304). Each group
   is its own PR.
3. **Honour cancellation** in the same pass, with the protocol teardown the contract names.
4. **One supervised-listener test per channel.** A fake transport that faults, asserting the
   listener returns `Err` and the supervisor's backoff grows. This is the test class that does
   not exist for any channel today, and its absence is why the contract drifted.
   **Verify**: mutate each fixed listener back to swallowing — its test must fail.
5. **Do not change `health_check` here.** Several health checks cannot fail or are wasteful,
   but that is a separate finding; mixing them in makes the diff unreviewable.

## Done criteria

- Every channel in the confirmed table returns `Err` on transport/auth faults.
- Every channel honours cancellation with its protocol's teardown.
- One supervised-listener test per channel, each failing when its fix is reverted.

## STOP conditions

- A channel's client library gives no way to distinguish a transient read from a fatal auth
  error → STOP for that channel and report; guessing turns a recoverable blip into a give-up.
- The supervisor's backoff turns out not to escalate even on `Err` → STOP; fix the supervisor
  first, or the whole plan buys nothing.

## Maintenance note

`traits.rs` states this contract. A new channel that swallows faults will pass CI today —
consider a test helper that every channel's suite is expected to instantiate.

## Rollback

Grouped commits; revert any group alone.
