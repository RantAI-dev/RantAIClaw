# Inbound media policy

**Date**: 2026-08-14
**Plan**: `plans/148-attachments-spike.md`
**Amended**: 2026-10-26 — inbound documents (PDF, DOCX, TXT) under rules
7–16. Rules 1–6 are unchanged.
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

**Accepted: PNG, JPEG, GIF, WebP.** Images only in this rule; documents have
their own rules (7–16) and never travel the image path.

The type is established by **sniffing the leading bytes**, not from the
platform's `mime_type` field or the URL's extension. Both are
attacker-influenced: a platform reports what the sender's client declared.

Where a platform *does* claim a type, the claim is used only as an early filter
(skip the download entirely for `application/pdf`), never as the accept
decision. If the claim and the bytes disagree, **the bytes win and the media is
rejected** — a mismatch is a signal in itself, not a formatting quirk.

**Amendment 2026-10-26:** the `application/pdf` early skip is scoped to the
image path. On channels that accept documents (rule 16), a claimed
`application/pdf` routes to the document rules (7–16) instead of skipping the
download.

### 3. Where the bytes land — memory only

Inbound media is converted to a `data:` URI **in memory** and embedded in the
message content. **Nothing is written to disk.**

This is the deliberate answer to "if disk, under what path, with what
permissions, and who cleans up?" — the question has no good answer yet, so the
policy avoids needing one. It costs memory proportional to the cap (5 MiB
default) for the lifetime of one message, which is acceptable; it saves a
persistent attacker-controlled file tree, a permissions decision, and a cleanup
job that would eventually not run.

If disk persistence is ever wanted (large files, audio, video, retention),
that is an operator-facing decision about where agent-received files live —
**escalate it, do not settle it inside a channel.** The 2026-10-26 amendment
keeps documents in memory too (rule 9); it does not take the disk option.

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

**The two-step channels are covered too.** Telegram (`getFile`) and WhatsApp
Cloud (the media lookup) each make one authenticated API call *before* reaching
the fetch. Both now call `media::peek` — the same refusal, without consuming a
slot — before that call, so an exhausted sender cannot make the lookup either.

Peek-then-charge is deliberately not atomic. Two attachments racing for the last
slot costs one wasted lookup, and the fetch still refuses; making the pre-check
consume would double-charge every image on those two channels, which is the
worse error.

Still bounding this alongside the budget: `max_images` per message (default 4),
the size cap per image, and the fact that a sender must already be on the
channel allowlist.

---

## Documents — amendment of 2026-10-26

These rules extend the policy to inbound **documents** (PDF, DOCX, TXT) on
Telegram and WhatsApp Web. A document is attacker-supplied bytes like an image,
and one step worse: its **parsed text is injected into the model's context**, so
both the parser and the extracted text are attack surface. The image rules above
stay as they are; a document never travels the image path.

### 7. Types — three, sniffed from bytes; the claim stays an early filter

**Accepted: PDF, DOCX, TXT.** Nothing else. The type is established by sniffing
the bytes, the discipline of rule 2:

- **PDF** — the leading bytes are `%PDF-`.
- **DOCX** — the bytes form a ZIP whose directory names `word/document.xml`.
- **TXT** — the bytes are valid UTF-8 with no NUL byte.

The platform's claimed MIME type and the file name are an early filter only;
when the claim and the bytes disagree, **the document is rejected**. For a
document the mismatch weighs more than it does for an image: the sniff decides
*which parser* touches the bytes, and feeding a ZIP to the PDF path is how
parsers get handed things they were not built to survive.

### 8. Size — a constant 10 MiB cap, checked twice

**Cap: 10 MiB per document** (`DOC_MAX_BYTES`, a constant in
`src/channels/media.rs`), checked on the platform's announced size before any
download and again on the bounded read — the rule 1 order, advisory first, the
bounded read as the guarantee. Over the cap: **reject, never truncate**. A
truncated PDF is a corrupt PDF, and a confident answer about half a document is
worse than a visible rejection.

The guarantee is the read itself, and each channel does it in its own way:

- **Telegram** streams the fetch chunk by chunk and stops one byte past the cap
  (`fetch_document_bytes` in `src/channels/media.rs`), so a body that keeps
  streaming cannot fill the memory.
- **WhatsApp Web** streams the download into a writer that refuses the write
  past the cap (`CappedDocumentWriter` in `src/channels/media.rs`, used by
  `src/channels/whatsapp_web.rs`). The bytes held are at most the cap whatever
  the sender announced, and once the cap is hit every further write fails, so
  the client's retry against the next host costs nothing.

The announced size is the sender's claim and stays advisory. A **WhatsApp Web
document that announced no size** is refused before the download: WhatsApp
announces a length of zero for a document it has not measured and sends no
length at all for some documents, and a size that was never announced cannot be
weighed.

The cap is a constant, **not a config key** — the reasoning of rule 6 applies
unchanged, and this amendment does not touch the config schema.

### 9. Where the bytes land — memory only, and the ZIP is weighed before it is opened

Rule 3 applies to documents with more force: the bytes are **parsed in a child
process over a pipe**, and **no temp file** is ever written. A document parser
is exactly the kind of code that must not be handed a writable path it can fill.

The agent process never parses a document. The parent
(`src/kb/extract/child_process.rs`) starts the same binary with a hidden
subcommand, hands the bytes over on stdin and reads one answer back, and that
answer is the only thing that crosses the boundary.

A DOCX is a ZIP, and a small ZIP can declare enormous contents — the classic
zip bomb. Before anything parses it, the **total uncompressed size recorded in
the ZIP directory is checked against a 50 MiB cap**. The directory is read by the
bounds-checked inline ZIP reader in `src/channels/media.rs`, so the check comes
from the archive's own directory and runs before anything is decompressed. A
directory that overshoots the cap rejects the document.

The check trusts the directory, so a directory that lies — small entries that
expand into gigabytes — is not stopped by it. What bounds that is the memory
ceiling of rule 10, and that ceiling exists on unix only.

### 10. Extraction — bounded in pages, characters and time, and out of process

Parsing attacker-supplied files is unbounded by default, so every axis gets a
constant limit:

- **Pages: at most 50** for a PDF, counted from the bytes **before** extraction
  starts.
- **Characters: at most 50,000** of extracted text are forwarded; a longer
  extraction is **cut with a visible note**, never silently clipped.
- **Time: at most 20 seconds** per extraction, **one extraction at a time per
  process**.

The extraction runs in a **child process**, not in a blocking task of the agent.
The parent (`src/kb/extract/child_process.rs`) starts the same binary with a
hidden subcommand, hands it the document on stdin and reads one answer back:

- The child is **killed and reaped** at the deadline, so the one-at-a-time slot
  is released **when the child dies** — not when the parser would have
  finished, had it ever finished.
- On **unix** the child also gets a **512 MiB memory ceiling** (`RLIMIT_AS`),
  with a CPU limit five seconds past the deadline as a backstop. On every other
  target the deadline kill is the only limit.
- The child gets **no environment**: the parent clears it and sets only the
  request — the document kind, the page cap and the character cap. No HOME, no
  PATH, no provider key and no configuration directory is in reach through the
  environment.
- The boundary is a **crash boundary, not a privilege boundary**. The child runs
  with the agent's own filesystem and network reach: a parser that wanted to
  read an operator file or open a socket could. What the child cannot do is
  take the agent down with it. A real jail — a namespace, a seccomp filter — is
  a separate piece of work and is not claimed here.
- The answer comes back as one JSON frame behind a sentinel, and the parent
  reads at most the frame's worth of bytes. A non-zero exit, a signal, a
  malformed frame or an oversized answer is **one rejection reason**, and the
  reason never carries the parser's internals.
- The child's stderr is discarded and never logged, because it can carry the
  document's own text.

Whatever the parser does — panic, abort, loop, runaway allocation — it is the
**child** that dies, not the agent. The sender gets a visible rejection, and the
next document can still be read.

### 11. Budget — documents are metered separately, before any download

**One document per message. Budget: 5 documents per sender per 10 minutes**
(`DOC_BUDGET_DOCUMENTS`, over the same fixed window as rule 6), constants in
`src/channels/media.rs`, not config keys. The budget is consulted **before any
download**, so an exhausted sender costs no bandwidth and no `getFile` call —
the rule 6 peek-then-charge order, applied to the document path.

### 12. How the text reaches the model — a delimited, defused, untrusted block

Extracted text enters the forwarded content as a **delimited block** naming the
file name, the page count, and whether it was truncated. The text passes through
`defuse_markers` first, so a document cannot plant `[IMAGE:…]` or
`[DOCUMENT:…]` markers of its own — extraction must not hand the sender the
injection primitive the image rules exist to contain.

The block states outright that the content **came from the sender and is
untrusted**, so the model reads it as evidence about the sender's file, not as
instruction.

In **history**, a document block becomes a one-line stub from the next user
message on, the way `redact_historic_image_markers` retires old image markers.
The text stays readable for the **whole turn it arrived in**, so every tool
call of that turn still sees the document and the model can answer the turn the
document came with. Otherwise every later turn re-carries the full text:
prompt bloat, plus a stale injection surface that lives forever.

The rule lives in `redact_historic_document_blocks` (`src/channels/media.rs`).
It works on the **structured** history (`ConversationMessage`) and is given the
index the current turn starts at, so it can tell which message belongs to the
turn in flight. It runs **before the history is flattened**
(`src/agent/loop_.rs`), because a flattened tool result is a user message, and
a flattened list cannot say which message started the current turn.

The final call of a turn — the one the soft cap and the loop detector make with
the tools off — flattens the history too, so it retires the old blocks as well.
The index is the one the turn started with, found before the loop ran, so the
document the model is still answering stays readable in that last call.

### 13. Groups and guests — the image gates, unchanged in order, before any download

A document passes the same gates an image passes, in the same order, still
before any bytes move:

- Telegram: the `mention_only` group gate reads the **caption**, so a document
  caption can address the bot like an image caption can.
- WhatsApp Web: the group gate reads the **document's own `ContextInfo`**, so a
  mention carried by the document message is visible.

A guest's document becomes the guest's text and receives exactly the treatment
a guest already gets; **it never raises what a guest can reach.**

### 14. Failure is never silent — and an allowed user is never an unauthorized user

Every rejection becomes an `[Attachment rejected: …]` note in the forwarded
content, naming the reason: type, size, pages, time, budget, or a build that
cannot read documents (rule 15). Rule 5's wording is the contract.

The note is written in **our own words**. A parser's error, a file name and a
claimed MIME type never ride into it: all three are the sender's to choose, and
the note reaches the model. A rejection therefore names the family the claim
belonged to, not what the sender typed.

One failure mode is called out because it is the one this amendment has to
forbid: a Telegram document with no caption from an **allowed** user must not be
routed to the unauthorized-user path, because the message carries no text of its
own. That would be a wrong signal to a user the operator admitted, so a document
from an allowed user is policy-checked and answered, never mistaken for an
intruder.

### 15. Build features — the parser ships behind the default `kb`

Documents need the default `kb` feature (PDF through the same `pdf-extract`
reader the `unpdf` backend uses; TXT is its own UTF-8 check on the bytes). A build without `kb` answers a
document with the note that this build cannot read documents — a visible
refusal, not a silent drop.

**DOCX — decided: option (a).** DOCX extraction lives behind `kb-office`, which
is **not** a default feature. The options the owner chose between:

- **(a)** DOCX is refused with a clear note in default builds, and works only in
  `kb-office` builds. **Chosen.**
- **(b)** `kb-office` becomes a default feature (adds `docx-rs` and `calamine`
  to every build).
- **(c)** a new feature carries only `docx-rs` and joins the defaults.

The reason: (b) and (c) both add a parser to every build for a format most
senders do not send, and (c) still needs a new feature in the tree for the same
effect. (a) keeps the default build's dependency set exactly as it is — no
`Cargo.toml` change, no `Cargo.lock` change — and answers the case honestly: a
default build refuses a DOCX with the note that this build cannot read DOCX
documents, which is a visible refusal, not a silent drop. An operator who wants
DOCX builds with `--features kb-office`.

The DOCX **sniff** still runs in a default build: a DOCX is recognised as a DOCX
(and refused as unreadable) rather than falling through to the text path, which
would treat a ZIP as UTF-8 text.

### 16. Channels — Telegram and WhatsApp Web; every other channel keeps ignoring documents

These rules bind **Telegram** and **WhatsApp Web**. Every other channel keeps
ignoring documents as it does today; this amendment does not widen their
behaviour. WhatsApp Web's media arrives decrypted in memory rather than by HTTP
fetch, so its documents take the bytes-in-hand path (the email path of the
checklist below) and the same rules apply to those bytes.

WhatsApp Web's document read is bounded by the capped writer of rule 8: the
download is streamed into a writer that refuses the write past the cap, so the
bytes held are at most the cap whatever the sender announced. A WhatsApp Web
document that announced **no size** is refused before the download, since a
size that was never announced cannot be weighed.

---

## What this policy does not cover

- **Outbound** media. Telegram sends it; nothing else does. Different threat
  model (our bytes, our choice of destination).
- **Non-image media other than documents** — audio, video. Documents were
  brought under this policy by the amendment of 2026-10-26 (rules 7–16). Audio
  and video each widen the sniff table and the size question further; images and
  documents are the cases people actually hit.
- **WhatsApp Web images** — the reverse-engineered client decrypts its own
  media through `wa-rs` and does not use an HTTP fetch at all. WhatsApp Web
  *documents* **are** covered by the amendment (rule 16); their bytes arrive
  decrypted in memory and take the bytes-in-hand path, and their read is
  bounded by the capped writer of rule 8.

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

**Documents** do not follow this checklist; rules 7–16 are their own path —
the same gates (allowlist, group gate, budget peek before the download), then
sniff, bounded download, and extraction in a child process. Rule 5 binds them
like everything else.
