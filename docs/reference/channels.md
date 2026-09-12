# Channels Reference

This document is the canonical reference for channel configuration in RantaiClaw.

For encrypted Matrix rooms, also read the dedicated runbook:
- [Matrix E2EE Guide](matrix-e2ee-guide.md)

## Quick Paths

- Need a full config reference by channel: jump to [Per-Channel Config Examples](#4-per-channel-config-examples).
- Need a no-response diagnosis flow: jump to [Troubleshooting Checklist](#6-troubleshooting-checklist).
- Need Matrix encrypted-room help: use [Matrix E2EE Guide](matrix-e2ee-guide.md).
- Need Nextcloud Talk bot setup: use [Nextcloud Talk Setup](nextcloud-talk-setup.md).
- Need deployment/network assumptions (polling vs webhook): use [Network Deployment](../operations/network-deployment.md).

## FAQ: Matrix setup passes but no reply

This is the most common symptom (same class as issue #499). Check these in order:

1. **Allowlist mismatch**: `allowed_users` does not include the sender (or is empty).
2. **Wrong room target**: bot is not joined to the configured `room_id` / alias target room.
3. **Token/account mismatch**: token is valid but belongs to another Matrix account.
4. **E2EE device identity gap**: `whoami` does not return `device_id` and config does not provide one.
5. **Key sharing/trust gap**: room keys were not shared to the bot device, so encrypted events cannot be decrypted.
6. **Stale runtime state**: config changed but `rantaiclaw daemon` was not restarted.

---

## 0. Two axes: support and verification

Every channel carries two labels, because there are two different questions and
one word could not answer both.

The single label this replaced defined *supported* as "someone has driven it".
Three of the four supported channels had never been driven, so the definition and
the catalog contradicted each other from the day the tier was set. Splitting the
label fixes the contradiction without demoting anything.

| Axis | Values | What it says | Who moves it |
|---|---|---|---|
| **support** | `supported`, `under development` | What the project undertakes. A bug in a supported channel is a bug we own, and `channel doctor` probes it | The owner. It is a product commitment |
| **verification** | `verified`, `not yet verified` | Whether anyone has driven a round trip against the real platform | The checklist below, once the evidence exists |

Both live in `CHANNEL_CATALOG` (`src/channels/mod.rs`), one row per channel, and
every surface renders them from there. `scripts/ci/check_channel_maturity.sh`
fails the build when this table drifts from that catalog, on either axis.

### The three states, and why the middle one is not a bug

| State | Reads as | Means |
|---|---|---|
| supported + verified | `supported · verified` | We commit to it and somebody has watched a message arrive |
| **supported + not yet verified** | `supported · not yet verified` | **We commit to it and nobody has driven it yet. This is a legitimate state** |
| under development + not yet verified | `under development · not yet verified` | Ships, outside what an alpha claims, and undriven |

The middle state describes Discord, Slack and WhatsApp Cloud today. It is not an
oversight and it is not something to tidy away. The owner committed to those
three on 2026-09-04; nobody has driven them because no credential for them exists
on any machine this project has run on. Both halves of that sentence are true at
once, and the two labels say so.

**Do not "fix" it by demoting them.** Moving a channel off the support axis
discards an owner decision so that a label looks consistent, which is the
opposite of what this split is for. `committed_but_undriven_is_a_legitimate_state`
in `src/channels/mod_tests.rs` fails if that set changes, so the demotion cannot
happen quietly.

"Under development" is not "broken" either. Twelve of these channels have tests
and several are in use.

### How a channel becomes verified

This checklist governs the **verification** axis only. Every item is about
evidence, and none of them is about commitment. Read together, they answer one
question: has somebody actually watched this work.

**No checklist can move the support axis.** That is the owner deciding what the
project undertakes to support, and evidence is not a substitute for that decision
in either direction. A channel can be committed to before anyone drives it, and
driving a channel does not by itself commit the project to it.

`not yet verified` becomes `verified` when all four hold, in this order:

1. **A real account exists** for the platform, and its credential is available to
   whoever runs the check. Without this the rest is theatre.
2. **A round trip was driven**: a message sent from the platform reached the
   agent, the agent's reply arrived back, and it was *read on a real client*,
   not asserted against a recorded fixture.
3. **What broke was written down.** A verification with no observations is a
   claim, not a verification. If nothing broke, say that.
4. **`channel doctor` probes it** — the key is in `PROBED_KEYS`
   (`src/doctor/checks/channels.rs`).

Failing any of the four leaves the channel `not yet verified`, with the reason
recorded. That is the checklist working, not the channel failing.

Item 4 needs one clarification, because it is the one place the two axes touch.
`probed_keys_cover_the_supported_tier` requires every **supported** channel to
have a doctor probe, whether or not it has been driven. Doctor coverage follows
the commitment, not the evidence: narrowing it to the driven set would stop
probing three channels the owner committed to.

| Channel (catalog key) | Support | Verification |
|---|---|---|
| `telegram` | supported | verified |
| `discord` | supported | not yet verified |
| `slack` | supported | not yet verified |
| `mattermost` | under development | not yet verified |
| `webhook` | under development | not yet verified |
| `imessage` | under development | not yet verified |
| `matrix` | under development | not yet verified |
| `signal` | under development | not yet verified |
| `whatsapp` | supported | not yet verified |
| `whatsapp_web` | supported | not yet verified |
| `linq` | under development | not yet verified |
| `nextcloud_talk` | under development | not yet verified |
| `email` | under development | not yet verified |
| `irc` | under development | not yet verified |
| `lark` | under development | not yet verified |
| `dingtalk` | under development | not yet verified |
| `qq` | under development | not yet verified |

`webhook` is in the catalog because operators think of it as a channel, but it is
served by the gateway and is not a `Channel` implementer, so the checklist above
cannot be run against it at all. It is under development by that fact rather than
by a default, and it can never become verified while that stays true.

---

## 0.1 Build and test evidence, per channel

**Seventeen channels are wired. One has been driven against a real platform.**

That count is the length of `CHANNEL_CATALOG`, and
`scripts/ci/check_channel_maturity.sh` fails the build if this sentence and the
catalog disagree. It said "seventeen" until 2026-09-08, against a sixteen-row
catalog, because it was typed rather than derived.

This section refines the **verification** axis of §0; it does not compete with
it. §0 answers one binary question, has anyone driven this. The table here says
what evidence a channel does have short of that, which is the useful thing to
know about the fifteen that answer "no". A channel is `verified` in §0 exactly
when it is **live-verified** here.

The vocabulary is borrowed from
[`kb-providers.md`](kb-providers.md), which already publishes this distinction.

| Value | Means |
|---|---|
| **live-verified** | A scripted round trip — send a message, receive it, assert the echo — was run against a real account. The row records **when** and **what kind of account** |
| **built and unit-tested** | Compiles, has unit tests, and is exercised by CI. Never driven against the live platform |
| **built, not tested by CI** | Compiles only under a feature flag no CI job builds |
| **unbuildable** | Does not compile at all today |

A row may **not** be marked live-verified on the strength of a unit test. That
conflation is the thing this table exists to prevent.

| Channel | Status | Evidence |
|---|---|---|
| Telegram | **live-verified** — 2026-07, bot account; re-driven 2026-09-08 | driven during the markdown-renderer effort; the reply rendering was read on a real client. Re-driven 2026-09-08 against the same bot: `channel doctor` reports healthy, the listener long-polls clean for 45 s, and a revoked token produces a rising 2→4→8→16→32→60 s backoff with the token absent from every log line. **The inbound round trip was not re-driven** — a bot cannot message itself, so that half needs a person |
| Discord | built and unit-tested | default feature; `cargo test --lib channels::discord` runs in CI |
| Slack | built and unit-tested | default feature; CI |
| Mattermost | built and unit-tested | default feature; CI |
| DingTalk | built and unit-tested | default feature; CI |
| WhatsApp Cloud | built and unit-tested | default feature; CI |
| WhatsApp Web | built and unit-tested | `whatsapp-web` is in `default`; CI |
| Signal | built and unit-tested | default feature; CI |
| Email (IMAP/SMTP) | built and unit-tested | default feature; CI |
| IRC | built and unit-tested | default feature; CI |
| QQ | built and unit-tested | default feature; CI |
| Linq | built and unit-tested | default feature; CI |
| Nextcloud Talk | built and unit-tested | default feature; CI |
| iMessage | built and unit-tested | default feature; CI. Note it needs macOS to run at all |
| Webhook (generic) | built and unit-tested | gateway endpoint; handler-level auth tests |
| Lark/Feishu | built and unit-tested | `channel-lark` is not a default feature, but it has its own CI job that builds **and tests** it |
| Matrix (E2EE) | built and unit-tested | `channel-matrix` is not a default feature, but it has its own CI job that builds **and tests** it — same shape as Lark. It was unbuildable until the pin moved to `matrix-sdk` 0.18: 0.16 exceeded the type-check recursion budget, so no job compiled it and its tests had never run. Re-confirmed 2026-09-08: `cargo test --locked --features channel-matrix --lib channels::matrix` builds and runs **33 tests, all passing**. Not live-verified against a real homeserver, so it is `not yet verified` on the second axis. It is also `under development` on the first, and the two are separate reasons: the owner did not name Matrix in the supported tier, and being undriven would not by itself put it outside one (Discord is supported and undriven) |

### What ships in a release binary

Release binaries are built with **default features only**
(`.github/workflows/pub-release.yml`). So:

- every "built in" channel above ships;
- **Lark ships in no release binary** — it needs a source build with
  `--features channel-lark`;
- **Matrix ships in no release binary** — it needs a source build with
  `--features channel-matrix`, on **rustc 1.93 or newer** (`matrix-sdk` declares
  that MSRV; the rest of RantaiClaw builds on 1.91). It does build there now.

The README's channel table carries the same columns; the two are meant to agree.

### What the supported tier has actually been driven against

The supported tier is the owner's 2026-09-04 decision (§0). It is a statement of
intent about what this project stands behind; it is **not**, by itself, a record
that anything was run. As of 2026-09-08:

| Supported channel | Credential available here | Driven |
|---|---|---|
| Telegram | yes | 2026-07, re-driven 2026-09-08 (see the row above) |
| Discord | **no** | never |
| Slack | **no** | driven before #778 landed, so void as evidence — re-drive owed |
| WhatsApp Web | **no** | never |
| WhatsApp Cloud | **no** | never |

Three of the four have no credential in any profile or environment on the
machine this was checked from, so nobody can drive them without an account being
provisioned first. They stay in the supported tier because the tier is the
owner's call, not the executor's — but the gap between "supported" and "driven"
is written down here rather than left for a reader to assume away.

### How a row becomes live-verified

Run a scripted round trip against a real account — send a message through the
channel, receive it back, assert the echo — then record the **date** and the
**account type** in the row. Anything less specific is decoration: without a
definition, "verified" becomes whatever the last person to edit the row thought
it meant.

A `channel verify <name>` subcommand extending the shape `channel doctor`
already establishes is the obvious way to make that repeatable. It is
**deliberately not built here** — it needs real platform accounts, and a per-PR
job depending on seventeen third-party services would be red more often than
green, at which point it gets ignored. If it is ever built it belongs in a
manually-dispatched workflow.

---

## 1. Configuration Namespace

All channel settings live under `channels_config` in `~/.rantaiclaw/config.toml`.

```toml
[channels_config]
cli = true
```

Each channel is enabled by creating its sub-table (for example, `[channels_config.telegram]`).

## In-Chat Runtime Model Switching

When running `rantaiclaw channel start` (or daemon mode), the four tier channels (Telegram, Discord, Slack and WhatsApp Web) support runtime switching, scoped to the conversation:

- `/models` — show available providers and current selection
- `/models <provider>` — switch provider for the current conversation. In a group that applies to everyone in it
- `/model` — show current model and cached model IDs (if available)
- `/model <model-id>` — switch model for the current conversation. In a group that applies to everyone in it

**Slack takes the same commands without the slash:** `models`, `models <provider>`, `model` and
`model <model-id>`. Slack's client treats a message that starts with `/` as one of its own slash
commands and never delivers it, which is also why approval replies on Slack are bare verbs. A bare
verb can also start an ordinary sentence, so Slack treats the message as a command only when it is
the verb alone or the verb and one more word; `model apa yang kamu pakai?` goes to the model. The
runtime's replies on Slack name the commands without the slash too.

Notes:

- Switching clears that conversation's history to avoid cross-model context contamination, in memory and in the persisted store. In a group that clears the history everyone there shares. On Slack the conversation is the thread.
- Model cache previews come from `rantaiclaw models refresh --provider <ID>`.
- These are runtime chat commands, not CLI subcommands.
- WhatsApp Cloud registers under the same channel name as WhatsApp Web, so it answers the slash form as well.
- Other channels do not answer these commands; the text reaches the model as ordinary chat.

Telegram, Discord and WhatsApp Web answer three more kinds of slash command:

- `/new` or `/clear` clears this conversation's history. Long-term memory stays; see
  [Clearing a Conversation](#clearing-a-conversation).
- `/start` or `/help` returns a short welcome with the command list. Telegram sends `/start` by
  itself when someone first opens a bot, and the group form `/start@<botname>` works too.
- Any other `/command` is answered by the runtime with the command list instead of reaching the
  model, which used to invent a result. In a Telegram group, an unknown command carrying an
  `@<name>` gets no reply at all, since it may be addressed to another bot; the runtime only knows
  its own bot name when `mention_only` is on, so this applies to its own name as well.

Approval replies (`/approve`, `/deny`) and pairing (`/bind`, `/claim`) are handled before any of
these and behave as before. Slack has no reset command: a new top-level message already starts a new
conversation.

## Clearing a Conversation

A conversation keeps its history, in memory and in `brain.db`, so the bot can follow what was said
earlier. To start fresh:

| Channel | How |
|---|---|
| Telegram, Discord, WhatsApp Web | send `/new` or `/clear` |
| Slack | start a new top-level message instead of replying inside the thread. With threading on, the default, each top-level message is already its own conversation. With `thread_replies = false` a Slack channel is one conversation, and no chat command resets it |

A `/model` or `/models <provider>` switch also clears that conversation's history, as the section
above describes. A reset keeps the model chosen with `/model`, and that choice lasts until the daemon
restarts: route overrides are held in memory and never written to disk, so a restart returns every
conversation to the configured model.

**A reset does not change what the chat app shows.** Telegram, Discord and WhatsApp Web still display
every earlier message, and the reply now says so in each app's own name: Telegram and WhatsApp point
at the chat's own menu, while Discord says the messages stay, because a DM has no clear-chat. A
channel whose behaviour has not been checked gets the reply unchanged rather than an invented
instruction, and no hint quotes a menu label.

### What a reset does not clear

`/new` and `/clear` remove the conversation's history and nothing else. Long-term memory stays:
facts the model saved with `memory_store`, such as a name, remain available in every conversation.
To see and remove those, run on the host:

- `rantaiclaw memory list` shows stored entries; `--category core|daily|conversation` narrows it.
- `rantaiclaw memory clear --key <key>` removes entries whose key starts with `<key>`. It is a
  prefix match, so list first.
- `rantaiclaw memory clear --category <core|daily|conversation>` removes a whole category.

Editing `brain.db` by hand does not reach a running daemon, which loads conversation history into
memory when it starts. Use `/new`, or restart the daemon after a manual edit.

## Draft streaming

A long reply can arrive all at once when the turn finishes, or grow in place while
the agent writes. The second is what makes a channel feel responsive, and it is
one feature rather than a maturity difference.

**Telegram is the only channel that streams drafts.** `supports_draft_updates()`
and the four methods behind it (`send_draft`, `update_draft`, `finalize_draft`,
`cancel_draft`) are implemented only in `telegram.rs`; every other channel takes
the `false` default from `channels/traits.rs` and replies once.

The blocker is configuration, not platform support. Discord and Slack both edit
messages perfectly well. What gates the feature is `stream_mode`, and that key
exists exactly once in the schema — on `TelegramConfig`. Turning streaming on for
another channel means giving it its own `stream_mode` and
`draft_update_interval_ms`, which are new config keys, so it needs a schema
version bump and belongs in a change that carries one.

Shipping it without the gate is not the shortcut it looks like: `update_draft` is
called on **every** streamed delta (`dispatch.rs:495`), and the per-recipient
throttle inside the channel is the only thing turning that into one edit per
`draft_update_interval_ms`. A channel with no setting has no off switch and no
configurable cadence.

### Why WhatsApp Web is a separate question

Even after a schema bump, WhatsApp Web should be measured before it is wired.
`wa-rs` does expose `Client::edit_message`, so the API is there. The cost is what
the numbers say:

- The throttle default is one edit per second (`draft_update_interval_ms` =
  `1000`).
- A channel turn may run to `message_timeout_secs` = `600`, and that scales up to
  4× with tool depth, so **600 edits for a default-length turn and up to 2,400 at
  the ceiling** — for a single reply.
- WhatsApp marks an edited message "Edited" on the recipient's screen and allows
  edits only within 15 minutes of sending, which is shorter than that scaled
  ceiling.

Telegram absorbs this because an edit there is quiet. On WhatsApp the same
cadence rewrites a message hundreds of times in a conversation the recipient is
watching. If it is ever built, it wants a much longer interval than the Telegram
default, chosen deliberately rather than inherited.

## Outbound Media Markers

A channel that can deliver attachments tells the model the marker syntax through
`Channel::delivery_instructions(workspace)`. The model then writes one marker
per attachment and the channel splits them out of the reply before sending:

```text
Here is the chart. [IMAGE:/workspace/chart.png]
```

`[IMAGE:…]`, `[DOCUMENT:…]`, `[VIDEO:…]`, `[AUDIO:…]` and `[VOICE:…]` all take a
local path or an `http(s)` URL. One vocabulary for every channel, built by
`media::delivery_instructions_for`, so a reply written on one channel does not
leak literal markers on another.

The instruction says what the runtime actually does, not only the syntax. It
states that attaching a file needs no tool call and no approval, that a local
path must be absolute and inside the workspace and names that path, that the
file has to exist before the marker is sent, and that markers go at the end of
the reply and never inside code fences. Syntax alone was not enough: on
2026-09-12 the same request produced a file on WhatsApp while Telegram answered
that it could not send attachments at all and emitted no marker, and Slack
guessed a `~/` path. Naming the workspace is why the method takes it.

A marker that never closes is not silent either. The closing bracket is looked
for on the marker's own line, so a `]` further down the reply no longer swallows
every line in between. A known marker kind that opens without one is logged at
WARN naming the kind and the target, and when the rest of the line is an
absolute path to a file that exists it is delivered anyway, so the reader gets
the file the model meant. Otherwise the text is left exactly as written and
nothing is lost. Workspace confinement is unchanged and is still decided by
`media::resolve_attachment_path_in_workspace` on the send path, which fails
closed.

When an attachment cannot be delivered, the conversation gets one line naming the
file, in the same thread as the reply. The text of a reply is sent before its
attachments on every channel, so that line says the attachment did not arrive
rather than that the answer was lost, and a reply that was only markers gets the
same line instead of silence. History then keeps the text the person read plus
`(the attachment was not delivered)`, and all four internal history notes are
stripped from outgoing replies, so a model that parrots its own history cannot
deliver the runtime's bookkeeping to a reader.

A local path goes through `media::resolve_attachment_path_in_workspace`, the same
way on every channel: a leading `~/` expands against `$HOME`, a relative path is
taken from the workspace root rather than the daemon's working directory, and an
absolute path is used as written. The file must then exist and sit inside the
workspace, or the attachment is refused and the error names the path the model
wrote. Until 2026-09-12 each channel checked the string as written, so only the
absolute form ever arrived.

**All four tier channels can deliver attachments**: Telegram, Discord, Slack and
WhatsApp Web. Every other channel returns `None` and is never told the syntax,
because telling a channel that cannot deliver them leaks `[IMAGE:…]` to the
reader as literal text.

**Slack needs the `files:write` scope**, granted on the owner's decision of
2026-09-10. A workspace whose app predates that has to add the scope and
reinstall; no migration can grant it. The upload is the modern three-call flow —
`files.getUploadURLExternal` to reserve a URL, a `POST` of the bytes to it, then
`files.completeUploadExternal` naming the channel, which is what actually shares
the file. `files.upload` is retired.

Only the first and third calls carry the bot token, and both go to the hardcoded
`slack.com` API host. The bytes `POST` deliberately carries **no** credential,
which is why it needs no host check: the URL came back from an authenticated
call, and pinning a host there would break uploads the day Slack moves external
storage off `files.slack.com`. That is the opposite of the *inbound* path, where
`url_private` arrives inside an event payload and does carry the token, so the
host is pinned there.

Rules that apply wherever a marker names a **local path**:

- The file must exist and must resolve **inside the workspace**. Both sides are
  canonicalised, so `../` cannot walk out, and an unresolvable path fails closed.
- This is not tidiness. A reply is influenced by whoever is chatting, so a prompt
  injection naming `~/.rantaiclaw/config.toml` would otherwise post provider keys
  and the bot token into the chat. `media::path_within_workspace` is the one
  implementation, and a class test fails if any channel's upload path stops
  calling it.
- An `http(s)` URL is passed to the platform instead of being re-uploaded.

WhatsApp Web builds a different message type per marker kind, because the
recipient's client renders on that: `[VIDEO:…]` is a video message, not an image
one, and `[VOICE:…]` is an audio message with `ptt` set, which is the only thing
separating a voice note from an audio file. The upload bucket follows the kind
too — encrypting under the wrong one produces a file the recipient cannot open,
and nothing on the sending side sees that fail.

The per-sender media budget in `media.rs` is **inbound only**. Nothing counts
outbound attachments today, on any channel.

## Inbound Image Marker Protocol

RantaiClaw supports multimodal input through inline message markers:

- Syntax: ``[IMAGE:<source>]``
- `<source>` can be:
  - Local file path
  - Data URI (`data:image/...;base64,...`)
  - Remote URL only when `[multimodal].allow_remote_fetch = true`

Operational notes:

- Marker parsing applies to user-role messages before provider calls.
- Provider capability is enforced at runtime: if the selected provider does not support vision, the request fails with a structured capability error (`capability=vision`).
- Linq webhook `media` parts with `image/*` MIME type are automatically converted to this marker format.
- **Discord, Telegram, Slack, both WhatsApp transports, Linq and Email accept inbound images.** An
  attachment is fetched, its type is sniffed from the bytes (the sender's claimed
  MIME is only an early filter), and it is embedded as a `data:` URI — nothing is
  written to disk. Over the `[multimodal].max_image_size_mb` cap, an unsupported
  type, or a failed fetch produces a **visible note** in the message rather than
  silence. Full rules:
  [inbound media policy](../security/inbound-media-policy.md).
- **Slack needs the `files:read` scope to see an upload at all.** `url_private` is
  authenticated, so without that scope the fetch is refused and the attachment
  becomes a note rather than an image. It is not in the scopes the first-run
  wizard used to list, so a workspace set up before 2026-09-10 has to add it and
  reinstall the app.
- **Slack sends its bot token to fetch an upload**, because `url_private` is
  authenticated. The URL comes out of the event payload, so the token goes only
  to `slack.com` and its subdomains over HTTPS; a file named on any other host
  becomes a visible note and no credential is sent. Discord needs no equivalent
  because its CDN links are pre-authorised and carry no token.
- **WhatsApp Web downloads through the library, not a URL.** `wa-rs` decrypts the
  media and hands back bytes, which then go through the same size cap, byte
  sniffing and per-sender budget as every other channel. Outbound media is still
  missing on this channel.
- **Inbound images are budgeted per sender**: 20 images per 10 minutes, counted
  per channel-qualified sender and charged *before* the download. Past it, the
  attachment becomes a note naming the wait. Not a config key — the constants
  live in `src/channels/media.rs`.
- Email needs no fetch — the IMAP message already carries the decoded bytes — so
  only the size and type rules apply. Attachments that neither claim to be an
  image nor look like one (calendar invites, vCards, delivery reports) are left
  alone rather than annotated, since an email's attachment list carries protocol
  furniture that a chat platform's does not.

## Channel Matrix

### Build Feature Toggles (`channel-matrix`, `channel-lark`)

Matrix and Lark support are controlled at compile time.

- Default builds do **not** include Matrix or Lark. They do include WhatsApp Web:
  `default = ["tui", "whatsapp-web", "remote-install", "kb"]` (`Cargo.toml:268`).
  WhatsApp Web mode therefore ships **enabled** — read the
  [security warning](#47-whatsapp) before configuring it.
- **No release binary carries Matrix or Lark.** `pub-release.yml` builds with default
  features only, so an operator who installs a release and configures
  `[channels_config.matrix]` gets a channel that reports as not configured. Running
  either one means building from source.
- Typical local check with only hardware support:

```bash
cargo check --features hardware
```

- Enable Matrix explicitly when needed:

```bash
cargo check --features hardware,channel-matrix
```

  To actually **run** it, build the binary rather than checking it, and note the
  toolchain: `matrix-sdk` declares `rust-version = "1.93"`, while the rest of
  RantaiClaw builds on 1.91. Matrix is the only part of this repository that
  needs a newer compiler than the crate's own declared MSRV.

```bash
rustup toolchain install 1.93.0
cargo +1.93.0 build --release --features channel-matrix
```

- Enable Lark explicitly when needed:

```bash
cargo check --features hardware,channel-lark
```

If `[channels_config.matrix]` or `[channels_config.lark]` is present but the corresponding feature is not compiled in, `rantaiclaw channel list`, `rantaiclaw channel doctor`, and `rantaiclaw channel start` will report that the channel is intentionally skipped for this build.

---

## 2. Delivery Modes at a Glance

| Channel | Receive mode | Public inbound port required? | Inbound authenticity |
|---|---|---|---|
| CLI | local stdin/stdout | No | n/a — local |
| Telegram | polling | No | n/a — the agent calls out |
| Discord | gateway/websocket | No | n/a — the agent calls out |
| Slack | Socket Mode (websocket) with `app_token`, else polling (`conversations.history`) | No | n/a — the agent calls out |
| Mattermost | polling | No | n/a — the agent calls out |
| Matrix | sync API (supports E2EE) | No | n/a — the agent calls out |
| Signal | signal-cli HTTP bridge | No (local bridge endpoint) | n/a — local bridge |
| WhatsApp (Cloud API) | webhook (`POST /whatsapp`) | Yes (public HTTPS callback) | **HMAC-verified** — `X-Hub-Signature-256`, required |
| WhatsApp (Web mode) | websocket | No | n/a — the agent calls out |
| Linq | webhook (`POST /linq`) | Yes (public HTTPS callback) | **HMAC-verified** — `X-Webhook-Signature` + `X-Webhook-Timestamp` (300s window), required |
| Nextcloud Talk | webhook (`POST /nextcloud-talk`) | Yes (public HTTPS callback) | **HMAC-verified** — `X-Nextcloud-Talk-Signature`, required |
| Webhook | gateway endpoint (`/webhook`) | Usually yes | Shared secret — pairing bearer token or `X-Webhook-Secret` |
| Email | IMAP polling + SMTP send | No | n/a — the agent calls out (see §4.9 on `From:` spoofing) |
| IRC | IRC socket | No | n/a — the agent calls out |
| Lark/Feishu (websocket) | websocket | No | n/a — the agent calls out |
| Lark/Feishu (webhook) | webhook (`POST /lark`, its own port) | Yes | Shared secret — `verification_token` in the body; **no signature check** |
| DingTalk | stream mode | No | n/a — the agent calls out |
| QQ | bot gateway | No | n/a — the agent calls out |
| iMessage | local integration | No | n/a — local |

"Inbound authenticity" is what proves a request actually came from the platform.
The three HMAC-verified endpoints refuse to serve at all without their secret —
see each channel's subsection. "n/a — the agent calls out" means there is no
inbound port: the process opens the connection, so there is nothing for an
attacker to POST to.


---

## 2a. Reply Formatting

The agent replies in GitHub-Flavored Markdown. Because each platform renders a
different markup dialect (or none), RantaiClaw parses the reply once and renders
it per platform before sending, so `##` headings, `**bold**`, and tables no
longer leak as literal text. Rendering is pure and deterministic; each channel
picks its dialect and the output is split into platform-sized chunks without
cutting a code fence.

| Channel | Render dialect | What happens to the markup |
|---|---|---|
| Telegram | HTML (`parse_mode=HTML`) | headings → **bold**, rules → a line, code → `<pre>`, tables → `<pre>` ASCII; each chunk carries a plain-text twin sent if Telegram rejects the HTML |
| Discord | StdMarkdown | keeps CommonMark; tables → aligned ASCII in a ``` fence (Discord renders no tables); `\*literal\*` escaped |
| DingTalk | StdMarkdown | its `markdown` message type renders CommonMark; tables → ASCII fence |
| Mattermost | StdMarkdown (native tables) | full GFM including pipe tables stays native |
| Slack | LightMarkup (`<url\|text>`) | `**bold**` → `*bold*`, links → `<url\|text>`, tables → ASCII fence, `&`/`<`/`>` escaped per Slack's text field |
| WhatsApp (Cloud + Web) | LightMarkup (`text (url)`) | `**bold**` → `*bold*`, links → `text (url)`, tables → ASCII fence |
| Signal, QQ, Linq, IRC, iMessage, Nextcloud Talk, Lark, Email, CLI | Plain | all markup stripped to readable text: headings uppercased, emphasis removed, links → `text (url)`, tables → aligned ASCII |
| Matrix | *(not wired)* | `matrix.rs` declares no `render_target()`, so it takes the trait default. It never calls the renderer at all: `send()` passes the model's text straight to `RoomMessageEventContent::text_markdown`, which Matrix renders natively as GFM — so nothing leaks, and the wiring buys formatting control rather than fixing a defect. **This row used to say the wiring was "blocked because the module does not compile"; that has not been true since the pin moved to `matrix-sdk` 0.18** ([§0.1](#01-build-and-test-evidence-per-channel)) |

Notes:

- **Tables** become an aligned ASCII grid on every platform that has no native
  table (all but Mattermost). Wide tables can scroll horizontally on narrow
  screens — inherent to ASCII-in-monospace.
- **Telegram streaming**: mid-response draft edits are rendered as plain text (a
  half-open HTML tag would be rejected); the final message is sent as HTML.
- **Lark, Email, Nextcloud Talk, Linq** are on the Plain baseline. Lark sends its
  `text` message type (plain); richer Lark `post`/`interactive` rendering, and an
  HTML email part, are deferred upgrades.

---

## 3. Allowlist Semantics

For channels with inbound sender allowlists:

- Empty allowlist: deny all inbound messages.
- `"*"`: allow all inbound senders (use for temporary verification only).
- Explicit list: allow only listed senders.

Field names differ by channel:

- `allowed_users` (Telegram/Discord/Slack/Mattermost/Matrix/IRC/Lark/DingTalk/QQ/Nextcloud Talk)
- `allowed_from` (Signal)
- `allowed_numbers` (WhatsApp)
- `allowed_senders` (Email, Linq)
- `allowed_contacts` (iMessage)

### 3.1 Per-channel matching rules

Matching is **exact and case-sensitive** unless listed below. An allowlist that
matches more than you expect is a security-boundary problem, so these are worth
reading before you tighten one:

| Channel | Rule | Source |
|---|---|---|
| Email | An entry containing `@` is a full address (case-insensitive). An entry starting with `@` matches a **domain suffix** (`@example.com` allows everyone at that domain). A bare entry with no `@` is also a domain (`example.com` ≡ `@example.com`). | `src/channels/email_channel.rs:215-238` |
| Matrix | Case-**insensitive** full-string match on the sender MXID. | `src/channels/matrix.rs:191-197` |
| Telegram | Entries are normalised on the way in — trimmed, and a leading `@` stripped — so `@user` and `user` are the same entry. A numeric user ID and a username are both accepted; the numeric ID is the stable one. | `src/channels/telegram.rs:428-430` |
| WhatsApp | Numbers are compared in normalised `+E.164` form. | `src/channels/whatsapp_web.rs` |
| Everything else | Exact, case-sensitive. | per-channel `is_*_allowed` |

Email's domain matching is the one to watch: `allowed_senders = ["example.com"]`
admits **every** sender at that domain, and `From:` is trivially forged unless
the sender-authentication gate is on (§4.9).

---

## 4. Per-Channel Config Examples

### 4.1 Telegram

```toml
[channels_config.telegram]
bot_token = "123456:telegram-token"
allowed_users = ["*"]
stream_mode = "off"               # optional: off | partial
draft_update_interval_ms = 1000   # optional: edit throttle for partial streaming
mention_only = false              # optional: require @mention in groups
interrupt_on_new_message = false  # optional: cancel in-flight same-sender same-chat request
```

Telegram notes:

- `interrupt_on_new_message = true` preserves interrupted user turns in conversation history, then restarts generation on the newest message.
- Interruption scope is strict: same sender in the same chat. Messages from different chats are processed independently.

### 4.2 Discord

```toml
[channels_config.discord]
bot_token = "discord-bot-token"
guild_id = "123456789012345678"   # optional
allowed_users = ["*"]
listen_to_bots = false
mention_only = false
```

### 4.3 Slack

```toml
[channels_config.slack]
bot_token = "xoxb-..."
app_token = "xapp-..."             # optional; selects Socket Mode. Needs `connections:write`.
                                   # Without it the channel polls one conversation: no DMs,
                                   # and no replies inside a thread.
channel_id = "C1234567890"         # required for polling, optional filter under Socket Mode
allowed_users = ["*"]
```

Slack notes:

- **Two receive transports, chosen by `app_token`.** `SlackChannel::listen` calls
  `listen_socket_mode` when `app_token` is set and non-blank, and `listen_polling`
  otherwise. This is a documented split, not a silent fallback: the two differ in
  what they can see. (Function names rather than line numbers on purpose — the
  line numbers this file used to cite went stale after one refactor.)
- **Socket Mode** holds one outbound WebSocket carrying events for every
  conversation the bot is in: channels, threads and DMs alike. `channel_id` stops
  being a requirement here and becomes what the schema always called it, an
  optional filter. No public inbound port either way.
- **Polling** reads one `conversations.history` page every 3 seconds. It
  **requires** `channel_id` — `listen_polling` returns `Err` without one — and it
  cannot see direct messages, nor replies inside a thread. That last one includes
  replies to the approval prompt this channel itself posts into a thread, so an
  operator on polling can be asked to approve a tool call in a place where their
  answer is never read. Budget the poll against Slack's Web API rate limits when
  several channels run.
- `doctor` reports the gap rather than leaving it to be discovered. With
  `app_token` it says nothing; with only `channel_id` it names polling as the
  narrower transport; with neither it says Slack cannot listen at all. See
  `slack_listen_gap` in `src/doctor/checks/channels.rs`.
- Both setup paths ask for `app_token` as an optional prompt. Skipping it is
  valid and yields polling.
- **Slack shows a short "working…" message** while the agent thinks, posted when
  the turn starts and deleted when the answer is ready. Slack has no typing
  indicator a bot can drive, and the two other routes were both closed:
  `agents.sessions.setStatus` needs the app declared as an agent plus a
  reinstall, and draft streaming needs a per-channel `stream_mode` only Telegram
  has (see [Draft streaming](#draft-streaming)). A placeholder needs neither —
  `chat:write` already covers posting and deleting the bot's own message.
  - It goes in the **same thread** as the reply, so a threaded conversation does
    not put a notice in the main channel where everyone else sees it.
  - It is removed on **every** exit path. The runtime cancels the typing task
    and awaits it before it looks at the turn's result, so a turn that errored,
    timed out or was cancelled still cleans up. A stuck "working…" is a lie that
    persists, and this is the shape that cannot leave one behind.
  - The placeholder is deleted **before** the answer is posted, so the two never
    race and nothing edits a message into something else.
  - **Not yet measured:** whether posting and quickly deleting leaves a push
    notification behind on mobile, which can differ between a channel post, a
    thread reply and a DM. That is a drive question; the code cannot answer it.

### 4.4 Mattermost

```toml
[channels_config.mattermost]
url = "https://mm.example.com"
bot_token = "mattermost-token"
channel_id = "channel-id"          # required for listening
allowed_users = ["*"]
```

### 4.5 Matrix

```toml
[channels_config.matrix]
homeserver = "https://matrix.example.com"
access_token = "syt_..."
user_id = "@rantaiclaw:matrix.example.com"   # optional, recommended for E2EE
device_id = "DEVICEID123"                  # optional, recommended for E2EE
room_id = "!room:matrix.example.com"       # or room alias (#ops:matrix.example.com)
allowed_users = ["*"]
```

See [Matrix E2EE Guide](matrix-e2ee-guide.md) for encrypted-room troubleshooting.

### 4.6 Signal

```toml
[channels_config.signal]
http_url = "http://127.0.0.1:8686"
account = "+1234567890"
group_id = "dm"                    # optional: "dm" / group id / omitted
allowed_from = ["*"]
ignore_attachments = false
ignore_stories = true
```

### 4.7 WhatsApp

RantaiClaw supports two WhatsApp backends:

- **Cloud API mode** (`phone_number_id` + `access_token` + `verify_token`) — stable, uses the official Meta Cloud API.
- **WhatsApp Web mode** (`session_path`) — always available in the default build (since v0.6.49-alpha). **Experimental, see security warning below.**

Cloud API mode:

```toml
[channels_config.whatsapp]
access_token = "EAAB..."
phone_number_id = "123456789012345"
verify_token = "your-verify-token"
app_secret = "your-app-secret"     # REQUIRED — see below
allowed_numbers = ["*"]
```

> **`app_secret` is required in Cloud API mode.** The gateway refuses the
> webhook outright when it is absent — `POST /whatsapp` returns **401
> Unauthorized** before the body is parsed, because the `X-Hub-Signature-256`
> HMAC is the only thing that can prove a request came from Meta
> (`src/gateway/mod.rs:2003-2011`). The channel still connects and still passes
> `rantaiclaw channel doctor`, so the symptom is a channel that looks healthy
> and answers nothing. Grep the log for:
>
> ```text
> WhatsApp webhook rejected: no app secret configured. Set RANTAICLAW_WHATSAPP_APP_SECRET to authenticate this endpoint.
> ```
>
> Set it in `[channels_config.whatsapp].app_secret` or as
> `RANTAICLAW_WHATSAPP_APP_SECRET` (`src/gateway/mod.rs:621`).

WhatsApp Web mode:

```toml
[channels_config.whatsapp]
session_path = "~/.rantaiclaw/state/whatsapp-web/session.db"
pair_phone = "15551234567"         # optional; omit to use QR flow
pair_code = ""                     # optional custom pair code
allowed_numbers = ["*"]
```

> **Security warning — WhatsApp Web mode**
>
> WhatsApp Web mode uses the `wa-rs` library, which reverse-engineers the
> WhatsApp Web protocol. This approach carries risks that do not apply to the
> Cloud API mode:
>
> - **Unofficial API:** The protocol is not documented or supported by Meta.
>   It can break without notice on any WhatsApp update.
> - **Account risk:** Meta may suspend accounts detected using unofficial clients.
> - **Unpredictable attack surface:** Protocol changes or undocumented behaviors
>   can introduce security regressions that are hard to audit or anticipate.
> - **Recommended only for:** controlled, non-production environments where the
>   Cloud API is unavailable and account suspension is acceptable.
>
> For production deployments, the Cloud API mode is strongly preferred.

Notes:

- Always compiled into the default `rantaiclaw` binary since v0.6.49-alpha.
  Prefer Cloud API mode in production — the WhatsApp Web protocol is
  unofficial and can break on any WhatsApp update.
- Keep `session_path` on persistent storage to avoid relinking after restart.
- Reply routing uses the originating chat JID, so direct and group replies work correctly.

### 4.7a Linq

```toml
[channels_config.linq]
api_token = "linq-partner-api-token"   # Bearer auth for the Partner API
from_phone = "+15551234567"            # E.164 sending number
signing_secret = "webhook-signing-secret"   # REQUIRED — see below
allowed_senders = ["*"]                # phone numbers, or "*"
```

Linq notes:

- Inbound endpoint: `POST /linq` on the gateway. Requires a reachable HTTPS
  callback.
- **`signing_secret` is required.** Without it the gateway returns **401** before
  parsing, the same fail-closed rule WhatsApp and Nextcloud Talk follow
  (`src/gateway/mod.rs:2160-2168`). Log line to grep:

  ```text
  Linq webhook rejected: no signing secret configured. Set RANTAICLAW_LINQ_SIGNING_SECRET to authenticate this endpoint.
  ```

- Verification is HMAC-SHA256 over `"{timestamp}."` followed by the raw body,
  read from `X-Webhook-Signature` and `X-Webhook-Timestamp`; timestamps older
  than 300 seconds are rejected (`src/channels/linq.rs:495-516`).
- `RANTAICLAW_LINQ_SIGNING_SECRET` overrides the config value
  (`src/gateway/mod.rs:649`).
- Inbound `media` parts with an `image/*` MIME type are converted to the image
  marker format described in §1, so Linq is the one webhook channel that carries
  images into the agent.

### 4.8 Webhook Channel Config (Gateway)

`channels_config.webhook` enables webhook-specific gateway behavior.

```toml
[channels_config.webhook]
secret = "optional-shared-secret"
```

Run with gateway/daemon and verify `/health`.

Notes:

- There is **no `port` key**. The endpoint is `POST /webhook` on the gateway's
  own listener (`[gateway].port`, default 9393). A `port` key existed until
  schema v21 and was read by nothing — it told operators to open a firewall port
  nothing binds. A config still carrying it loads; the key is ignored.
- Authentication is the pairing bearer token, or `X-Webhook-Secret` when
  `secret` is set.

### 4.9 Email

```toml
[channels_config.email]
imap_host = "imap.example.com"
imap_port = 993
imap_folder = "INBOX"
smtp_host = "smtp.example.com"
smtp_port = 465
smtp_tls = true
username = "bot@example.com"
password = "email-password"
from_address = "bot@example.com"
poll_interval_secs = 60
allowed_senders = ["*"]
require_authenticated_sender = false   # optional; see below
trusted_authserv_id = "mx.example.com" # required for owner recognition; see below
```

Email notes — `From:` is forgeable:

- **`trusted_authserv_id` is what makes the authentication headers mean
  anything.** `Authentication-Results` (RFC 8601) is written by the receiving
  infrastructure, but anything a sender puts in a message arrives as a header
  too, and the two are indistinguishable once parsed. Set this to the
  **authserv-id your own mail server writes**: the first token of the header,
  before the first `;`. To find it, open a message that reached the mailbox and
  read its headers —

  ```
  Authentication-Results: mx.example.com; dmarc=pass header.from=example.com
                          ^^^^^^^^^^^^^^ this
  ```

  Verdicts from any other authserv-id are ignored, so a header the sender
  supplied grants nothing.
- **Unset means email owner recognition is off.** Mail from an `approval_owners`
  address is dropped rather than granted owner authority, and the channel logs
  that at startup. This is deliberate: there is no safe way to accept a
  header-derived identity without knowing who wrote the header.
- `require_authenticated_sender = true` drops mail whose `From:` domain is not
  backed by `dmarc=pass`, or by an aligned `spf=pass`/`dkim=pass`, from that
  trusted verifier. It is **off by default** because a relay that strips the
  header would otherwise silence a working mailbox.
- Alignment is exact: an identifier authenticates `example.com` when it *is*
  `example.com` or a subdomain of it (`bounces.example.com`).
  `example.com.attacker.test` does not.
- The **owner path does not depend on that flag**: mail claiming to come from an
  address in `approval_owners` is dropped when unauthenticated, always.
  Otherwise anyone could grant themselves approval authority by typing a `From:`
  line.
- Turn the flag on for any mailbox that is reachable from the public internet.

### 4.10 IRC

```toml
[channels_config.irc]
server = "irc.libera.chat"
port = 6697
nickname = "rantaiclaw-bot"
username = "rantaiclaw"              # optional
channels = ["#rantaiclaw"]
allowed_users = ["*"]
server_password = ""                # optional
nickserv_password = ""              # optional
sasl_password = ""                  # optional
verify_tls = true
```

### 4.11 Lark / Feishu

```toml
[channels_config.lark]
app_id = "cli_xxx"
app_secret = "xxx"
encrypt_key = ""                    # must stay empty — see below
verification_token = ""             # REQUIRED in webhook mode
allowed_users = ["*"]
use_feishu = false
receive_mode = "websocket"          # or "webhook"
port = 8081                          # required for webhook mode
```

Interactive onboarding support:

```bash
rantaiclaw onboard --interactive
```

The wizard now includes a dedicated **Lark/Feishu** step with:

- region selection (`Feishu (CN)` vs `Lark (International)`)
- credential verification against official Open Platform auth endpoint
- receive mode selection (`websocket` or `webhook`)
- webhook verification token prompt — **required** when `receive_mode = "webhook"`

Webhook-mode authenticity (accurate as of plan 124, merged):

- The event endpoint authenticates with the **`token` field in the callback
  body**, compared in constant time against `verification_token`
  (`src/channels/lark.rs:1434-1448`). An absent token is a rejection.
- `receive_mode = "webhook"` **refuses to start** without `verification_token`
  (`src/channels/lark.rs:1299-1307`) — an endpoint that authenticates nothing
  would let anyone who can reach the port drive the agent.
- **`X-Lark-Signature` is not checked.** Its digest construction could not be
  confirmed against a live tenant, and a subtly wrong implementation rejects
  every legitimate callback while presenting as a working gate
  (`src/channels/lark.rs:1427-1433`). A shared body token is weaker than an
  HMAC: it is replayable and it is in the request body rather than bound to it.
  If the endpoint is internet-facing, front it with an authenticating reverse
  proxy.
- `encrypt_key` is **rejected at startup** if set (`src/channels/lark.rs:1312-1323`), and setup no longer asks for it:
  this build does not decrypt event bodies, so enabling encryption in the
  developer console makes every callback unreadable. Leave it empty, or use
  `receive_mode = "websocket"`, which needs no inbound endpoint at all.

Runtime token behavior:

- `tenant_access_token` is cached with a refresh deadline based on `expire`/`expires_in` from the auth response.
- send requests automatically retry once after token invalidation when Feishu/Lark returns either HTTP `401` or business error code `99991663` (`Invalid access token`).
- if the retry still returns token-invalid responses, the send call fails with the upstream status/body for easier troubleshooting.

### 4.12 DingTalk

```toml
[channels_config.dingtalk]
client_id = "ding-app-key"
client_secret = "ding-app-secret"
allowed_users = ["*"]
```

### 4.13 QQ

```toml
[channels_config.qq]
app_id = "qq-app-id"
app_secret = "qq-app-secret"
allowed_users = ["*"]
```

### 4.14 Nextcloud Talk

```toml
[channels_config.nextcloud_talk]
base_url = "https://cloud.example.com"
app_token = "nextcloud-talk-app-token"
webhook_secret = "webhook-secret"           # REQUIRED — 401 without it
allowed_users = ["*"]
```

Notes:

- Inbound webhook endpoint: `POST /nextcloud-talk`.
- Signature verification uses `X-Nextcloud-Talk-Random` and `X-Nextcloud-Talk-Signature`.
- The secret is **required**: with none configured the endpoint returns `401`
  before parsing (`src/gateway/mod.rs:2338-2346`), and invalid signatures are
  rejected with `401` as well.
- `RANTAICLAW_NEXTCLOUD_TALK_WEBHOOK_SECRET` overrides config secret.
- See [nextcloud-talk-setup.md](nextcloud-talk-setup.md) for a full runbook.

### 4.15 iMessage

```toml
[channels_config.imessage]
allowed_contacts = ["*"]
```

---

## 4a. Threading

Where the platform supports it, a reply attaches to the message that prompted
it. Three fields carry three different things, and they are not interchangeable:

- **`reply_target`**: *where* the message goes. A channel, a chat, a Telegram
  forum topic as `chat_id:thread_id`, or a Discord thread, which is a channel.
- **`thread_ts`**: the platform *thread* the message belongs to, a Slack parent
  `ts` or a Mattermost `root_id`. It is part of the conversation key, so every
  message in one thread shares history, memory scope and a `/model` choice.
- **`reply_anchor`**: the message a reply *quotes*, a Telegram
  `reply_parameters` or a Discord `message_reference`. It is different on every
  message, so it is never part of the conversation key.

| Channel | Threads today | Mechanism | Field |
|---|---|---|---|
| Slack | yes | parent `ts` | `thread_ts` |
| Discord | yes | `message_reference` on the prompting message | `reply_anchor` |
| Telegram | yes | `reply_parameters` (text sends; attachments are not anchored) | `reply_anchor` |
| Mattermost | yes | `root_id` | `thread_ts` |
| Nextcloud Talk, QQ, Email, Lark, Matrix, Signal | not yet | see [the design note](../project/2026-08-14-threading-design.md) for each platform's mechanism and cost | none yet |
| DingTalk, Linq, IRC, iMessage | no platform primitive | replies land in the conversation | none |

Turn it off without turning off the channel:

```toml
[channels_config]
thread_replies = true          # shared default

[channels_config.mattermost]
thread_replies = false         # per-channel override, wins where set
```

The switch is enforced once, centrally. The dispatch loop clears both
`thread_ts` and `reply_anchor` before the agent sees the message, so every
channel honours it identically. With threading off, Telegram and Discord replies
stop quoting, and a Slack or Mattermost channel becomes one conversation instead
of one per thread.

---

## 4b. Approval and Roles

Every channel shares one authorization model, and its **secure default surprises
people**: with no owners configured, nobody can approve anything, so
approval-required tools auto-deny and the agent looks broken.

```toml
[channels_config]
approval_owners = ["rantaiclaw_user"]   # who may approve privileged tool calls
guest_allowed_tools = []                # capability ceiling for everyone else
guest_allowed_commands = []             # shell globs guests may run (hard ceiling)
autonomous_tools = false                # true = skip the approval gate entirely
```

- **Owners** approve privileged tool calls and always get the full toolset.
  `approval_owners = []` (the default) means **nobody** can approve —
  privileged tools stay auto-denied. `"*"` lets any allowed sender approve;
  it is accepted, insecure, and opt-in only (`src/config/schema.rs:2739-2743`).
- **Guests** are senders on the channel allowlist who are not owners. They get
  read-only file and memory tools plus skills; `guest_allowed_tools` widens
  that, and `guest_allowed_commands` is a hard ceiling on shell — a command
  outside it is denied outright, never escalated to an owner.
- **If privileged tools are being denied, add an owner.** Do **not** reach for
  `autonomous_tools = true`: that skips the approval gate for everyone on the
  channel, which is a different and much larger decision.

Full model, including the enforcement point:
[Per-role channel permissions](../security/per-role-permissions.md).

---

## 5. Validation Workflow

1. Configure one channel with permissive allowlist (`"*"`) for initial verification.
2. Run:

```bash
rantaiclaw onboard --channels-only
rantaiclaw daemon
```

3. Send a message from an expected sender.
4. Confirm a reply arrives.
5. Tighten allowlist from `"*"` to explicit IDs.

---

## 6. Troubleshooting Checklist

If a channel appears connected but does not respond:

1. Confirm the sender identity is allowed by the correct allowlist field.
2. Confirm bot account membership/permissions in target room/channel.
3. Confirm tokens/secrets are valid (and not expired/revoked).
3a. For the three webhook channels, confirm the **inbound secret is set at all**:
   WhatsApp Cloud API `app_secret`, Linq `signing_secret`, Nextcloud Talk
   `webhook_secret`. Each returns `401` before parsing when it is missing, so the
   channel connects, passes `channel doctor`, and answers nothing. Lark in
   webhook mode refuses to start without `verification_token`.
3b. If tool calls are being denied rather than the channel being silent, this is
   the approval model, not the transport — see [§4b](#4b-approval-and-roles).
3c. A `503` from `/whatsapp`, `/linq` or `/nextcloud-talk` means the message was
   verified but could not be queued: either the channel dispatch loop is not
   running (the daemon is starting or the channels component is restarting) or its
   queue of 100 is full. The response carries a `Retry-After` and the message is
   **not** marked as seen, so the platform's retry is processed normally. Watch
   `rantaiclaw_channel_enqueue_rejected_total` on `/metrics` to tell a saturated
   queue (`reason="full"`) from a stopped one (`reason="closed"`).
4. Confirm transport mode assumptions:
   - polling/websocket channels do not need public inbound HTTP
   - webhook channels do need reachable HTTPS callback
5. Restart `rantaiclaw daemon` after config changes.

For Matrix encrypted rooms specifically, use:
- [Matrix E2EE Guide](matrix-e2ee-guide.md)

---

## 7. Operations Appendix: Log Keywords Matrix

Use this appendix for fast triage. Match log keywords first, then follow the troubleshooting steps above.

### 7.1 Recommended capture command

```bash
RUST_LOG=info rantaiclaw daemon 2>&1 | tee /tmp/rantaiclaw.log
```

Then filter channel/gateway events:

```bash
rg -n "Matrix|Telegram|Discord|Slack|Mattermost|Signal|WhatsApp|Email|IRC|Lark|DingTalk|QQ|iMessage|Webhook|Channel" /tmp/rantaiclaw.log
```

### 7.2 Keyword table

| Component | Startup / healthy signal | Authorization / policy signal | Transport / failure signal |
|---|---|---|---|
| Telegram | `Telegram channel listening for messages...` | `Telegram: ignoring message from unauthorized user:` | `Telegram poll error:` / `Telegram parse error:` / `Telegram polling conflict (409):` |
| Discord | `Discord: connected and identified` | `Discord: ignoring message from unauthorized user:` | `Discord: received Reconnect (op 7)` / `Discord: received Invalid Session (op 9)` |
| Slack | `Slack channel listening on #` | `Slack: ignoring message from unauthorized user:` | `Slack poll error:` / `Slack parse error:` |
| Mattermost | `Mattermost channel listening on` | `Mattermost: ignoring message from unauthorized user:` | `Mattermost poll error:` / `Mattermost parse error:` |
| Matrix | `Matrix channel listening on room` / `Matrix room ... is encrypted; E2EE decryption is enabled via matrix-sdk.` | `Matrix whoami failed; falling back to configured session hints for E2EE session restore:` / `Matrix whoami failed while resolving listener user_id; using configured user_id hint:` | `Matrix sync error: ... retrying...` |
| Signal | `Signal channel listening via SSE on` | (allowlist checks are enforced by `allowed_from`) | `Signal SSE returned ...` / `Signal SSE connect error:` |
| WhatsApp (channel) | `WhatsApp channel active (webhook mode).` / `WhatsApp Web connected successfully` | `WhatsApp webhook rejected: no app secret configured.` (401, endpoint disabled) / `WhatsApp webhook signature verification failed` / `WhatsApp: ignoring message from unauthorized number:` / `WhatsApp Web: message from ... not in allowed list` | `WhatsApp send failed:` / `WhatsApp Web stream error:` |
| Linq (gateway) | `POST /linq` | `Linq webhook rejected: no signing secret configured.` (401, endpoint disabled) / `Linq webhook signature verification failed` / `Linq: rejecting stale webhook timestamp` | `Linq send failed:` |
| Webhook / WhatsApp (gateway) | `WhatsApp webhook verified successfully` | `Webhook: rejected — not paired / invalid bearer token` / `Webhook: rejected request — invalid or missing X-Webhook-Secret` / `WhatsApp webhook verification failed — token mismatch` | `Webhook JSON parse error:` |
| Email | `Email polling every ...` / `Email sent to ...` | `Blocked email from ...` | `Email poll failed:` / `Email poll task panicked:` |
| IRC | `IRC channel connecting to ...` / `IRC registered as ...` | (allowlist checks are enforced by `allowed_users`) | `IRC SASL authentication failed (...)` / `IRC server does not support SASL...` / `IRC nickname ... is in use, trying ...` |
| Lark / Feishu | `Lark: WS connected` / `Lark event callback server listening on` | `Lark WS: ignoring ... (not in allowed_users)` / `Lark: ignoring message from unauthorized user:` | `Lark: ping failed, reconnecting` / `Lark: heartbeat timeout, reconnecting` / `Lark: WS read error:` |
| DingTalk | `DingTalk: connected and listening for messages...` | `DingTalk: ignoring message from unauthorized user:` | `DingTalk WebSocket error:` / `DingTalk: message channel closed` |
| QQ | `QQ: connected and identified` | `QQ: ignoring C2C message from unauthorized user:` / `QQ: ignoring group message from unauthorized user:` | `QQ: received Reconnect (op 7)` / `QQ: received Invalid Session (op 9)` / `QQ: message channel closed` |
| Nextcloud Talk (gateway) | `POST /nextcloud-talk — Nextcloud Talk bot webhook` | `Nextcloud Talk webhook rejected: no webhook secret configured.` (401, endpoint disabled) / `Nextcloud Talk webhook signature verification failed` / `Nextcloud Talk: ignoring message from unauthorized actor:` | `Nextcloud Talk send failed:` / `LLM error for Nextcloud Talk message:` |
| iMessage | `iMessage channel listening (AppleScript bridge)...` | (contact allowlist enforced by `allowed_contacts`) | `iMessage poll error:` |

### 7.3 Runtime supervisor keywords

If a specific channel task crashes or exits, the channel supervisor in `channels/mod.rs` emits:

- `Channel <name> exited unexpectedly; restarting`
- `Channel <name> error: ...; restarting`
- `Channel message worker crashed:`

These messages indicate automatic restart behavior is active, and you should inspect preceding logs for root cause.

### 7.4 Shutdown drain keywords

When the daemon stops, the dispatch loop drains before the process exits:

- `Cancelled an in-flight channel request`: a turn was stopped, by a newer message from the same sender or by the drain deadline 12 seconds into shutdown
- `sent a restart notice` / `could not send a restart notice:` / `a restart notice timed out`: a conversation was told to send its message again, logged with `channel` and `message_id`
- `WhatsApp Web stopped listening; its connection stays open until dispatch finishes`: WhatsApp Web forwards nothing new and keeps its connection only for the replies and notices still going out
