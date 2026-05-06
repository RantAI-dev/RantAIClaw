# Pillar 5 — Multi-Channel Runtime

> **ClickUp:** [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) → Setup: Channels + Resilience: Channels · **Maturity:** Core stable, niche mixed · **Modules:** `src/channels/`

Run one agent across many transports simultaneously. Each channel is independent — add, remove, or hot-swap at runtime via the gateway API without restarting.

## What this pillar covers

- 13+ channel adapters (chat, email, IRC, terminal)
- Per-channel allowlist + auth probes
- Live config: PATCH `/config/channels` adds or removes at runtime
- Conversation-history management per channel
- Supervised listener with auto-restart
- Pairing / QR / token-based onboarding

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Channel count (core stable) | 9 (Telegram, Discord, Slack, WhatsApp Cloud, Mattermost, Signal, Email, IRC, DingTalk, CLI) | TBD | TBD |
| Channel count (feature-gated) | 4 (WhatsApp Web, Matrix E2EE, Lark, iMessage) | TBD | TBD |
| Per-channel allowlist (deny-by-default) | ✅ | TBD | TBD |
| Hot add/remove via API | ✅ PATCH `/config/channels` | TBD | TBD |
| Doctor probes (`getMe`, `auth.test`, etc.) | ✅ all major | TBD | TBD |
| QR pairing (WhatsApp Web) | ✅ Unicode block-character render | n/a | n/a |

## Channel matrix

| Channel | Status | Feature | Doctor probe |
|---|---|---|---|
| Telegram | Stable | default | `getMe` |
| Discord | Stable | default | `users/@me` |
| Slack | Stable | default | `auth.test` |
| WhatsApp Cloud | Stable | default | `/v18.0/<phone_id>` |
| Mattermost | Stable | default | n/a |
| Signal | Stable | default | n/a |
| Email (IMAP/SMTP) | Stable | default | n/a |
| IRC | Stable | default | n/a |
| DingTalk | Stable | default | n/a |
| CLI | Built-in | always | n/a |
| WhatsApp Web | Stable | `--features whatsapp-web` | session-DB exists |
| Matrix (E2EE) | Feature-gated | `--features channel-matrix` | n/a |
| Lark / Feishu | Feature-gated | `--features channel-lark` | n/a |
| iMessage | Stable (macOS only) | default | n/a |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Channel trait + factory | Stable |
| Supervised listener loop | Stable |
| Conversation history per channel | Stable |
| Hot-add via gateway API | Stable |
| Doctor probes | Stable (added v0.5.2) |
| WhatsApp Web QR (Unicode block) | Stable (added v0.5.2) |
| Matrix E2EE on `--all-features` | Blocked on matrix-sdk recursion-limit |
| Mattermost maturation | Implemented · needs validation |
| Nextcloud Talk | Implemented · needs validation |

## Architecture

```
src/channels/
├── traits.rs       ← Channel trait
├── mod.rs          ← orchestration loop + factory wiring (planned split)
├── runtime.rs      ← supervised listener (planned)
├── history.rs      ← per-channel conversation memory (planned)
├── telegram.rs / discord.rs / slack.rs / ...
├── whatsapp.rs / whatsapp_web.rs / whatsapp_storage.rs
└── qr_terminal.rs  ← Unicode QR for pairing
```

## Trait extension point

- `Channel` — `src/channels/traits.rs`
- Required: `send`, `listen`, `health_check`, typing semantics
- Test: auth + allowlist + health behavior
- Default allowlist policy: empty = deny all, `*` = allow all (explicit opt-in)

## CLI / config

```bash
rantaiclaw channel list
rantaiclaw channel add <type> '<json>'
rantaiclaw channel remove <name>
rantaiclaw channel doctor              # health check all
rantaiclaw channel start
```

```toml
[channels.telegram]
bot_token = "..."
allowed_chats = [-1001234567890, "*"]   # specific or wildcard

[channels.discord]
bot_token = "..."
guild_id = "..."
```

Hot-swap via API:

```bash
curl -X PATCH http://localhost:8080/config/channels \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"telegram": {"bot_token": "...", "name": "my-bot"}}'
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Setup: Channels validates the picker across all 13 channels (auth probes + allowlist + WhatsApp Web QR). Resilience: Channels produces the **per-channel resumption guarantee matrix** (at-least-once / at-most-once / lossy) that lands in this pillar's "Channel matrix" section.
