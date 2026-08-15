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

## 0. Verification Status

Seventeen channels are wired. **One has been driven against a real platform.**
That is not a defect in itself, but it is the difference between "it compiles
and has tests" and "someone watched a message arrive", and an operator choosing a
channel deserves to know which they are getting.

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
| Telegram | **live-verified** — 2026-07, bot account | driven during the markdown-renderer effort; the reply rendering was read on a real client |
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
| Matrix (E2EE) | **unbuildable** | `matrix-sdk` 0.16 exceeds the type-check recursion budget; no CI job compiles it and its 1,168-line module is checked by nothing. Options are costed in [the dependency write-up](../project/2026-08-14-dependency-decisions.md) |

### What ships in a release binary

Release binaries are built with **default features only**
(`.github/workflows/pub-release.yml`). So:

- every "built in" channel above ships;
- **Lark ships in no release binary** — it needs a source build with
  `--features channel-lark`;
- **Matrix ships in no release binary** and cannot currently be built from
  source either.

The README's channel table carries the same columns; the two are meant to agree.

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

## In-Chat Runtime Model Switching (Telegram / Discord)

When running `rantaiclaw channel start` (or daemon mode), Telegram and Discord now support sender-scoped runtime switching:

- `/models` — show available providers and current selection
- `/models <provider>` — switch provider for the current sender session
- `/model` — show current model and cached model IDs (if available)
- `/model <model-id>` — switch model for the current sender session

Notes:

- Switching clears only that sender's in-memory conversation history to avoid cross-model context contamination.
- Model cache previews come from `rantaiclaw models refresh --provider <ID>`.
- These are runtime chat commands, not CLI subcommands.

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
- **Discord, Telegram, WhatsApp Cloud, Linq and Email accept inbound images.** An
  attachment is fetched, its type is sniffed from the bytes (the sender's claimed
  MIME is only an early filter), and it is embedded as a `data:` URI — nothing is
  written to disk. Over the `[multimodal].max_image_size_mb` cap, an unsupported
  type, or a failed fetch produces a **visible note** in the message rather than
  silence. Full rules:
  [inbound media policy](../security/inbound-media-policy.md).
- Email needs no fetch — the IMAP message already carries the decoded bytes — so
  only the size and type rules apply. Attachments that neither claim to be an
  image nor look like one (calendar invites, vCards, delivery reports) are left
  alone rather than annotated, since an email's attachment list carries protocol
  furniture that a chat platform's does not.

## Channel Matrix

### Build Feature Toggles (`channel-matrix`, `channel-lark`)

Matrix and Lark support are controlled at compile time.

- Default builds do **not** include Matrix or Lark. They do include WhatsApp Web:
  `default = ["tui", "whatsapp-web", "remote-install", "kb"]` (`Cargo.toml:253`).
  WhatsApp Web mode therefore ships **enabled** — read the
  [security warning](#47-whatsapp) before configuring it.
- Typical local check with only hardware support:

```bash
cargo check --features hardware
```

- Enable Matrix explicitly when needed:

```bash
cargo check --features hardware,channel-matrix
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
| Slack | polling (`conversations.history`) | No | n/a — the agent calls out |
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
| Matrix | *(not wired)* | The renderer itself shipped; what is missing is the four-line `render_target()` wiring in `matrix.rs`, which is blocked because the module does not compile ([§0](#0-verification-status)). Matrix renders GFM natively, so nothing leaks in the meantime |

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
app_token = "xapp-..."             # accepted, ignored — setup no longer asks
channel_id = "C1234567890"         # optional
allowed_users = ["*"]
```

Slack notes:

- The channel **polls** `conversations.history` every 3 seconds
  (`src/channels/slack.rs:257`, `:267`); it is not an Events API subscriber and
  needs no public inbound port. Expect up to a few seconds of reply latency, and
  budget the poll against Slack's Web API rate limits when several channels run.
- `app_token` exists in the config schema for a Socket Mode implementation that
  does not exist yet. Setting it changes nothing; the channel now logs
  `Slack: \`app_token\` is set but ignored` at startup so it is not a silent
  no-op, and neither setup path asks for it any more.

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
```

Email notes — `From:` is forgeable:

- `require_authenticated_sender = true` drops mail whose `From:` domain is not
  backed by `dmarc=pass`, or by an aligned `spf=pass`/`dkim=pass`, in the
  `Authentication-Results` header your own MTA wrote
  (`src/channels/email_channel.rs:254`). It is **off by default** because a relay
  that strips that header would otherwise silence a working mailbox.
- The **owner path does not depend on that flag**: mail claiming to come from an
  address in `approval_owners` is dropped when unauthenticated, always
  (`src/channels/email_channel.rs:315-324`). Otherwise anyone could grant
  themselves approval authority by typing a `From:` line.
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
it. Two fields, two meanings — they are not interchangeable:

- **`reply_target`** — *where* the message goes (a channel, a chat, a Telegram
  forum topic as `chat_id:thread_id`).
- **`thread_ts`** — *what* it attaches to once there (a Slack parent `ts`, a
  Discord `message_reference`, a Mattermost `root_id`, a Telegram
  `reply_to_message_id`).

| Channel | Threads today | Mechanism |
|---|---|---|
| Slack | yes | parent `ts` |
| Discord | yes | `message_reference` on the prompting message |
| Telegram | yes | `reply_parameters` (text sends; attachments are not anchored) |
| Mattermost | yes | `root_id` |
| Nextcloud Talk, QQ, Email, Lark, Matrix, Signal | not yet | see [the design note](../project/2026-08-14-threading-design.md) for each platform's mechanism and cost |
| DingTalk, Linq, IRC, iMessage | no platform primitive | replies land in the conversation |

Turn it off without turning off the channel:

```toml
[channels_config]
thread_replies = true          # shared default

[channels_config.mattermost]
thread_replies = false         # per-channel override, wins where set
```

The switch is enforced once, centrally: the dispatch loop clears the reply
anchor before the agent sees it, so every channel honours it identically.

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
