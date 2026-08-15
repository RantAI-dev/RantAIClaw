# Inbound media policy

**Date**: 2026-08-14
**Plan**: `plans/148-attachments-spike.md`
**Applies to**: every channel that accepts an attachment from a chat platform.

Accepting an inbound image means **downloading attacker-supplied bytes onto the
operator's machine and putting them in the agent's context**. Anyone the
allowlist admits can send them, and on a group channel that is a wider set than
the operator pictures.

This policy is written **before** the first fetch, and it is deliberately one
document rather than a per-channel decision. Fifteen channels answering these
questions independently is how fifteen different answers happen.

---

## The rules

### 1. Size — hard cap, checked twice

**Cap: `[multimodal].max_image_size_mb` (default 5 MiB, clamped to 1–20).**

Checked twice, because the first check is advisory:

1. `Content-Length`, before the body is read. A missing or lying header is
   normal, so this is an early exit, not the guarantee.
2. The **actual** decoded byte count, after a **bounded** read. The read is
   bounded at the cap, so a server that streams forever cannot exhaust memory
   while we wait to find out how big the body is.

Past the cap: **reject**, do not truncate. A truncated image is a corrupt image;
handing the model half a JPEG produces a confident description of nothing. No
thumbnail fetch either — that is a second request to the same attacker-chosen
endpoint.

### 2. MIME type — sniffed, never claimed

**Accepted: PNG, JPEG, GIF, WebP.** Images only, to start.

The type is established by **sniffing the leading bytes**, not from the
platform's `mime_type` field or the URL's extension. Both are
attacker-influenced: a platform reports what the sender's client declared.

Where a platform *does* claim a type, the claim is used only as an early filter
(skip the download entirely for `application/pdf`), never as the accept
decision. If the claim and the bytes disagree, **the bytes win and the media is
rejected** — a mismatch is a signal in itself, not a formatting quirk.

### 3. Where the bytes land — memory only

Inbound media is converted to a `data:` URI **in memory** and embedded in the
message content. **Nothing is written to disk.**

This is the deliberate answer to "if disk, under what path, with what
permissions, and who cleans up?" — the question has no good answer yet, so the
policy avoids needing one. It costs memory proportional to the cap (5 MiB
default) for the lifetime of one message, which is acceptable; it saves a
persistent attacker-controlled file tree, a permissions decision, and a cleanup
job that would eventually not run.

If disk persistence is ever wanted (large files, non-image media, retention),
that is an operator-facing decision about where agent-received files live —
**escalate it, do not settle it inside a channel.**

### 4. Interaction with `[multimodal].allow_remote_fetch`

`allow_remote_fetch` gates **model-directed** fetches: an `[IMAGE:]` marker
carrying a remote URL, in text the agent is processing, which the agent can be
talked into emitting. It is off by default because it makes the agent an SSRF
proxy.

Inbound channel media is a **different case** and is not gated by it: the URL is
not chosen by anyone in the conversation, it is the platform's own CDN endpoint
reached with the channel's own credential, and it exists because a user
deliberately sent the bot a picture. Gating it on `allow_remote_fetch` would
mean images silently vanish under the default config — which is exactly the
"dropped without acknowledgement" behaviour this work exists to end.

The size and type rules above are what bound this path.

### 5. Failure is never silent

A dropped image is reported **in the forwarded content**, so the user sees it and
the model can say so:

```text
[Attachment rejected: image too large (8.4 MiB, limit 5 MiB)]
[Attachment rejected: unsupported type (sniffed application/pdf)]
[Attachment unavailable: media fetch failed]
```

The current Signal behaviour — detect an attachment, skip it, say nothing — is
the thing being generalised away. A user who gets no answer about a screenshot
they sent has no way to tell a policy rejection from a broken bot.

### 6. Rate limiting — a per-sender budget, charged before the download

Inbound media is an unmetered cost lever for anyone the allowlist admits, and on
a group channel that is a wider set than the operator pictures.

**Budget: 20 images per sender per 10 minutes.** Both numbers are constants in
`src/channels/media.rs` (`BUDGET_IMAGES`, `BUDGET_WINDOW`), not config keys — a
key means a schema version bump and a drift snapshot, which is not worth
spending until an operator asks for a different number. Change them there.

The counter lives in `media::` and is keyed by a **channel-qualified** sender
key (`discord:<id>`, `email:<address>`), so one identifier reused on two
platforms does not share an allowance. `media::charge` is consulted by
`fetch_image`/`fetch_image_bytes` **before the request goes out**, so an
exhausted sender costs no bandwidth. The alternative — metering in the dispatch
loop's existing per-sender in-flight tracking — would have been cheaper to write
but could only refuse markers whose bytes had already been downloaded.

The window is fixed rather than sliding: a sender who exhausts it waits out the
remainder, which is cheaper to reason about and errs toward the sender's benefit
at the boundary. Entries are dropped once their window closes, so the map holds
only senders active in the last 10 minutes.

Refusal is a visible note like every other rejection, and it names the wait:
`[Attachment rejected: media budget spent (20 images per 10 minutes); try again
in 4 minute(s)]`.

**Known gap.** Telegram (`getFile`) and WhatsApp Cloud (the media lookup) each
make one authenticated API call *before* reaching the fetch, so the budget saves
those channels the download but not that lookup. Bounding it would mean moving
the charge into each channel's own two-step resolver, which trades one shared
rule for five per-channel ones. The remaining exposure is one small API request
per refused attachment.

Still bounding this alongside the budget: `max_images` per message (default 4),
the size cap per image, and the fact that a sender must already be on the
channel allowlist.

---

## What this policy does not cover

- **Outbound** media. Telegram sends it; nothing else does. Different threat
  model (our bytes, our choice of destination).
- **Non-image** media — documents, audio, video. Each widens the sniff table and
  the size question; images are the case people actually hit.
- **WhatsApp Web** (the reverse-engineered client), which decrypts its own media
  through `wa-rs` and does not use an HTTP fetch at all.

## Providers that cannot see images

The gate that refuses image input to a non-vision provider reads the whole
conversation, so a stored image used to fail every later turn as well. Images
in **history** are now replaced with an explicit note before that gate runs —
only the turn the user just sent can be refused. The note is deliberately
visible in the prompt: a model should be able to say it could not see the
picture, rather than answer confidently about one it never received.

---

## Implementation checklist for a new channel

1. Filter on the platform's claimed type first, to avoid a pointless download.
2. Fetch with the channel's own credential, bounded at the size cap.
3. Sniff the bytes; reject on mismatch or unsupported type.
4. Convert to a `data:` URI and emit the `[IMAGE:…]` marker the multimodal path
   already understands (`docs/reference/channels.md` §1).
5. On any rejection, append the note to the message content. Never drop silently.
6. Pass a channel-qualified sender key (`"<channel>:<id>"`) so the per-sender
   budget in rule 6 applies. A path that does not fetch charges
   `media::charge` itself.

Steps 1 and 2 assume a transport that hands over a URL. **Email does not**: the
IMAP message already carries the decoded bytes, so it skips straight to step 3
with `media::accept_bytes`. No credential and no attacker-chosen host are
involved on that path — only the size and type rules apply.

Email is also the one inbound surface where the attachment list is not entirely
human intent: calendar invites, vCards and delivery reports arrive as parts.
Those are skipped rather than annotated. Rule 5 still binds everything that *is*
or *claims to be* an image, which is what a user who sent a screenshot needs.
