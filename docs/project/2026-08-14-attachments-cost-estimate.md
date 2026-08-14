# Inbound media: what each remaining channel costs

**Date**: 2026-08-14
**Plan**: `plans/148-attachments-spike.md`
**Policy**: [`docs/security/inbound-media-policy.md`](../security/inbound-media-policy.md)
**Status**: Discord and WhatsApp Cloud implemented; the rest estimated, not built.

---

## What shipped

- The **policy**, written before any fetch code, implemented once in
  `src/channels/media.rs`: size cap from `[multimodal].max_image_size_mb`
  checked against both `Content-Length` and a bounded read; MIME **sniffed**
  from the bytes with the platform's claim used only as an early filter; bytes
  held in memory as a `data:` URI and never written to disk; every rejection
  emitted as a visible note.
- **Discord** — attachments are fetched from the CDN URL (no bot token sent:
  the host comes from the payload) and converted to `[IMAGE:data:…]`.
- **WhatsApp Cloud** — the synchronous webhook parser emits
  `[WHATSAPP_MEDIA:<id>|<claimed-mime>]` and `hydrate_media`, called by the
  gateway before dispatch, resolves it in the two authenticated round trips the
  Cloud API requires.
- The outbound seam is now `Channel::delivery_instructions()` rather than a
  central `match` on the channel name.

## Per-platform cost for the remaining twelve

| Channel | Inbound media mechanism | New API surface? | Cost | Notes |
|---|---|---|---|---|
| Slack | `files` on the event; download from `url_private` with the bot token | no | **Cheap** | the one gap is that Slack's poll (`conversations.history`) returns file metadata already |
| Telegram | already done (`getFile` → download) | — | — | the reference implementation; it predates the shared policy and should be moved onto `media::` |
| Matrix | `mxc://` URI resolved through the media repo | yes-ish | **Medium** | matrix-sdk exposes it, but the module does not compile in CI (see the dependency write-up) |
| Lark/Feishu | `im/v1/messages/{id}/resources/{key}` with the tenant token | no | **Medium** | needs the message id AND the resource key, both present in the event |
| Nextcloud Talk | the message references a file share; fetch via WebDAV with the app token | no | **Medium** | WebDAV is a different auth/path shape from the OCS API the channel already speaks |
| Signal | `signal-cli` writes attachments to disk and reports the path | **no fetch at all** | **Cheap, but** | the bytes are already local — which collides head-on with the policy's "memory only" rule. Reading them is trivial; deciding whether a path outside our control counts as "landed on disk" is the actual work |
| WhatsApp Web | `wa-rs` decrypts media itself | no HTTP | **Medium** | a different code path from Cloud entirely; belongs with plan 123's owner |
| Linq | already converts `media` parts with an `image/*` MIME to markers | — | **Done, unpoliced** | it trusts the platform's claimed type and applies no size cap — it should move onto `media::` before it is called finished |
| DingTalk | stream-mode frames carry a `downloadCode`, exchanged for a URL | no | **Medium** | one extra call, same shape as WhatsApp's two-step |
| QQ | media URLs in the message payload | no | **Cheap** | |
| Email | MIME parts are already in the message being parsed | **none** | **Cheapest** | `mail_parser` hands over the bytes; no network at all. This is the highest value-per-line remaining |
| IRC | none | — | **Not possible** | |
| iMessage | AppleScript can read the attachment path | — | **Medium** | same on-disk question as Signal |

## Recommended order

1. **Email** — no fetch, no credential, bytes already in hand.
2. **Slack, QQ** — plain authenticated fetch, the shape already implemented twice.
3. **Move Telegram and Linq onto `media::`** — both predate the policy. Linq in
   particular accepts a platform-claimed `image/*` with no size cap, which is
   the exact combination the policy exists to prevent.
4. **Lark, DingTalk, Nextcloud Talk** — one extra resolution step each.
5. **Signal, iMessage** — only after the "bytes already on disk" question has an
   answer. Do not settle it channel by channel.
6. **Matrix** — behind the matrix-sdk build decision.

## Outbound media beyond Telegram

**Not worth it yet, and the reason is not effort.** The outbound path needs a
per-platform upload API, a size/type policy of its own, and a decision about
what the agent is allowed to send — and the demand is much lower than inbound:
users send screenshots constantly and receive generated files rarely. The seam
is now a trait method, so a channel can opt in without touching a central
`match` when that changes.

## What the drive would have shown, and did not

Step 5 asks for a screenshot sent from a real Discord client and a real WhatsApp
account, then an oversized file and a rejected type, confirming the user is told
rather than ignored.

**That drive was not performed.** This environment has no Discord bot token or
guild and no WhatsApp Business account. The tests assert the policy decisions
and the request shapes against a local server; they cannot show that Discord's
CDN URL shape, or Meta's two-step media lookup, matches what the real platforms
send today.

Before this reaches users someone with real accounts should run step 5 exactly
as written. The specific risks a drive would catch: Discord attachment URLs now
carry expiring signed query parameters (`ex`/`is`/`hm`) — the code passes the
URL through unmodified, which should be right, but only a live attachment
proves it; and Meta's media lookup response shape is version-dependent.

## Gaps recorded rather than fixed

- **No per-sender media rate limit.** The policy explains why (it belongs in the
  dispatch loop's per-sender tracking, not a per-channel counter) and calls for
  a follow-up plan.
- **Non-image media is still skipped silently on WhatsApp** (audio, video,
  documents, stickers, reactions, locations). Turning every one of those into a
  rejection note would put "[Attachment rejected]" under every reaction a user
  sends; widening the accepted set is a separate decision per type.
- **Telegram's own photo path still has its own 25 MiB cap** and does not use
  `media::`. It works; it is simply not yet under the shared policy.
