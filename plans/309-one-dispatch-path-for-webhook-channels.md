# Plan 309: Route webhook channels through the same dispatch as everything else

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/gateway/mod.rs src/channels/dispatch.rs src/channels/factory.rs`

## Status

- **Priority**: P1 (ledger W2-3) · **Effort**: L · **Risk**: MED
- **Category**: architecture / bug
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

WhatsApp Cloud, Linq and Nextcloud Talk arrive over gateway webhooks and are handled by a
second, divergent dispatch implementation inside `gateway/mod.rs` rather than by
`channels/dispatch.rs`. The two have drifted, and every difference is a defect on the webhook
side:

- History is keyed per **person**, so distinct rooms and groups merge into one conversation.
  That leak was fixed in `dispatch.rs` and never here.
- History is in-memory only; nothing persists to the session store.
- Approvals use a different dialect (`Y/A/N`) than the `/approve <tool>` the other channels
  document.
- The gateway builds its own channel instances, so WhatsApp is constructed without the
  multimodal caps the factory applies, and allowlist hot-reload never reaches these three —
  a revoked sender stays allowed until restart.

Three of sixteen channels behave differently from the documented product because of where
their bytes arrive.

## Steps

1. **Map both paths side by side** before changing anything: for one inbound message, list
   what each path does — conversation key, history store, tool assembly, approval dialect,
   media handling, allowlist source. The PR description carries that table.
2. **Reuse the factory's instances.** The gateway should hold the same channel objects the
   factory builds, not construct its own. This alone fixes the multimodal and hot-reload gaps.
3. **Hand the parsed message to `dispatch.rs`** instead of the local handler, so conversation
   keying, history, approvals and tool assembly are shared.
4. **Keep the webhook-specific parts** where they belong: signature verification, idempotency
   and the platform payload parsing stay in the gateway. Only dispatch moves.
5. **Tests that pin the differences that were bugs**: two senders in different rooms do not
   share history; an allowlist change reaches a webhook channel without restart; an approval
   over a webhook channel uses the documented dialect.
6. **Delete the second implementation** once nothing calls it, in the same PR — leaving it
   invites re-drift.

## Done criteria

- One dispatch implementation; `rg -n 'process_channel_chat' src/` finds nothing.
- The three webhook channels persist history, honour hot-reloaded allowlists, and use the
  documented approval dialect.
- `cargo test --lib channels`, `--lib gateway`, `cargo test --test channel_routing` pass.

## STOP conditions

- The webhook path must answer the platform synchronously within a timeout that shared
  dispatch cannot meet → STOP and report; the fix is then an async hand-off, a different
  design.
- Sharing dispatch would change idempotency or signature handling → STOP; those must not move.

## Maintenance note

Two implementations of one concern drift by default. If a future transport needs special
handling, it belongs as a parameter to the shared path, not as a second path.

## Rollback

Single large commit is the risk here — consider landing step 2 (shared instances) separately
from step 3 (shared dispatch) so each reverts alone.
