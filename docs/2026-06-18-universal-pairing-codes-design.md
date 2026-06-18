# Universal On-Demand Pairing Codes — Design

Status: approved (owner delegated approval via session goal; built autonomously).
Date: 2026-06-18. Target release: 0.6.82-alpha.

## Problem

1. **Only Telegram** has owner/access self-onboarding (`/bind` + `/claim`, via
   `PairingGuard`). Every other multi-user channel gates access purely by a
   config allowlist field set in `config.toml` / CLI — there's no in-chat code a
   user can send to enrol themselves.
2. The pairing code is **minted only at channel startup** (and only when the
   allowlist is empty). Once anyone is allowlisted, you cannot add another
   user/owner without **restarting the daemon** in pairing mode — impractical.
3. No **multi-owner** convenience: the startup code is single-use
   (`try_pair` consumes it).

## Goals

- Mint a fresh pairing code **on demand, without restarting** the running daemon.
- Codes are **time-windowed and multi-claim**: valid for a TTL (default 15 min),
  any number of `/claim`/`/bind` within the window, then auto-expire. Tunable
  via `--ttl` / `--max-uses`.
- `/claim`-style self-onboarding (and `/bind`) available on **all multi-user
  channels**, keyed on each channel's native identity.
- Minting from **CLI**, an **owner-only chat tool**, and the **TUI**.
- The **gateway** pairing code is also mintable on demand (same limitation today).

## Non-goals

- Not touching WhatsApp **device-linking** (`pair_code` / `pair_phone`) — that
  authenticates the bot's own WhatsApp account and is unrelated.
- Not removing config-based allowlists — pairing augments them.
- Email channel is out of scope (asynchronous; no real-time claim UX).

## Architecture

### 1. Pairing-code store (shared) — `src/security/pairing_store.rs` (new)

On-disk JSON at `<profile_root>/pairing_codes.json`, mode `0600`. Decouples the
*minting* process (CLI / chat / TUI) from the *validating* process (running
daemon): both read/write this file under an `fs2` advisory lock.

Entry:
```
{ id: String, code_hash: String (SHA-256 hex), surface: String,
  expires_at: i64 (unix), max_uses: Option<u32>, uses: u32, grant_owner: bool }
```
- `surface` = channel name (`"telegram"`, `"whatsapp"`, …) or `"gateway"`. A
  code is scoped to one surface (identity types differ; a Telegram code must not
  claim on WhatsApp).
- `grant_owner` = whether `/claim` (owner) is allowed with this code; `/bind`
  (chat-only) always allowed.

API (pure functions over the profile path; each takes/holds the file lock):
- `mint(profile, surface, ttl, max_uses, grant_owner) -> PlainCode` — generate a
  Crockford-base32 code, 8 chars rendered grouped `XXXX-XXXX` (~40 bits; brute-
  resistant within a short TTL), store its SHA-256 hash, return the plaintext.
- `try_consume(profile, surface, code, now) -> Option<ConsumeOutcome{grant_owner}>`
  — constant-time hash compare across non-expired, non-exhausted entries for the
  surface; on hit increment `uses`, persist, return the outcome; prune expired.
- `prune(profile, now)` — drop expired/exhausted entries.

Reuses existing deps: `sha2`, `fs2`, `rand`, `serde_json`.

### 2. Channel pairing integration — `src/channels/pairing.rs` (new shared helper)

A small **adapter contract** each channel implements, plus a generic handler so
the per-channel code is thin and uniform.

Adapter (per channel):
- `surface() -> &str` — store key.
- `extract_identities(inbound) -> Vec<String>` — sender identity form(s) for
  this channel (see matrix below). All forms are persisted so `can_approve`
  resolves regardless of which the runtime sees.
- `allowlist_field() -> AllowlistField` — enum naming which `Config` channel
  field to append to (`AllowedUsers | AllowedNumbers | AllowedFrom |
  AllowedSenders | AllowedContacts`).

Generic handler `try_handle_pairing(text, identities, adapter, profile, config_mutator) -> Option<Reply>`:
1. Parse `/bind <code>` or `/claim <code>` (trim, charset-validate). Return
   `None` if not a pairing command (so normal messages flow to the agent).
2. `PairingStore::try_consume(surface, code)`. On miss → reply "invalid/expired".
3. On hit → append `identities` to the channel's allowlist field (runtime list +
   persisted config); for `/claim` **and** `grant_owner` → also append to
   `channels_config.approval_owners`. Persist via `Config::save`.
4. Return a success reply ("you can chat" / "you're now an owner").

Each channel: (a) calls `try_handle_pairing` in its inbound loop **before** agent
dispatch and consumes the message if handled; (b) at startup, if its allowlist is
empty, mints a startup code into the store and prints it (generalises today's
Telegram startup behaviour). Telegram is **refactored onto this shared core**
(removing its bespoke `try_handle_bind`/`try_handle_claim` duplication) to prove
the design before fan-out.

### 3. On-demand mint surfaces

- **CLI:** `rantaiclaw channels pair [--channel <name>] [--ttl <min>=15]
  [--max-uses <n>] [--no-owner]`. Default: 15-min TTL, unlimited uses within the
  window, owner-capable. Prints the code + the channel-appropriate `/bind` and
  `/claim` instructions. Works whether or not the daemon is running; a running
  daemon picks it up on the next pairing message.
- **Chat tool:** `issue_pairing_code` (owner-only — added to
  `GuestGate::OWNER_ONLY_TOOLS`). Params: `channel`, `ttl_minutes`, `max_uses`,
  `owner` (bool). Writes to the store (same process when run as the daemon →
  immediately active) and returns the code + instructions for the agent to relay.
  The `owner-permissions` bundled skill gains a section on issuing invite codes.
- **TUI:** `/pair [channel] [--ttl N] [--no-owner]` — shows the code (mirrors the
  CLI; uses the `/autonomy` `block_in_place` config bridge pattern).

### 4. Gateway on-demand mint

`--channel gateway` mints a code into the store; the gateway's `POST /pair`
(`PairingGuard`) consults the store in addition to its in-memory startup code, so
a new API/console client can pair without a gateway restart.

### Config / schema

**No `config.toml` schema change** — the store is a separate JSON file and all
per-channel allowlist fields already exist. This deliberately avoids the
`schema_drift` + migration release gate.

### Per-channel adapter matrix (the parallelisable fan-out)

| Channel | surface | identity form(s) | allowlist field |
|---|---|---|---|
| telegram | telegram | numeric id, username | allowed_users |
| discord | discord | user id, username | allowed_users |
| slack | slack | user id | allowed_users |
| mattermost | mattermost | username / id | allowed_users |
| matrix | matrix | `@user:server` | allowed_users |
| signal | signal | phone (E.164) | allowed_from |
| whatsapp (cloud) | whatsapp | phone (E.164) | allowed_numbers |
| whatsapp_web | whatsapp | phone (E.164) | allowed_numbers |
| irc | irc | nick | allowed_users |
| lark | lark | user id | allowed_users |
| dingtalk | dingtalk | user id | allowed_users |
| qq | qq | user id | allowed_users |
| linq | linq | sender | allowed_senders |
| nextcloud_talk | nextcloud_talk | user | allowed_users |
| imessage (macOS only) | imessage | contact (phone/email) | allowed_contacts |

WhatsApp Cloud + Web share `surface = "whatsapp"` and `allowed_numbers` (same
phone identity), so a code minted for "whatsapp" works on whichever variant runs.

### Security

- Codes hashed (SHA-256) at rest; store file `0600`.
- TTL + `max_uses` bound exposure; Crockford-base32 8-char (~40 bits) is
  brute-infeasible within a 15-min window; keep per-sender attempt tracking where
  a channel already has it.
- A code is **surface-scoped** — cannot cross channels.
- `/claim` ⇒ owner only when the code's `grant_owner` is set (operator opt-out
  via `--no-owner`); guests can never mint (`issue_pairing_code` is owner-only).
- Pairing-command messages are consumed before agent dispatch (never forwarded).

### Testing

- `pairing_store`: mint / consume / expire / `max_uses` exhaustion / prune /
  concurrent-lock unit tests.
- `channels::pairing`: command parsing + identity-extraction + allowlist/owner
  mutation, per representative adapter (mocked inbound JSON).
- Telegram refactor: existing `/bind` `/claim` behaviour preserved (port the
  current tests onto the shared core).
- Lint under the CI toolchain (1.92.0): fmt + `-D clippy::correctness` +
  strict-delta. No `schema_drift` impact (no config change).

### Implementation phasing (drives the agent dispatch)

- **Phase 1 — foundation (sequential, build first; everything depends on it):**
  `pairing_store` module; `channels::pairing` shared helper + `AllowlistField`;
  CLI `channels pair`; `issue_pairing_code` chat tool + `OWNER_ONLY_TOOLS` entry
  + skill note; TUI `/pair`; **refactor Telegram onto the shared core**; gateway
  store consult + `--channel gateway`.
- **Phase 2 — channel fan-out (parallel agents, each independent once Phase 1
  lands):** wire each remaining channel onto the shared core using the adapter
  contract. Group by similarity: (a) `allowed_users` set — discord, slack,
  mattermost, matrix, irc, lark, dingtalk, qq, nextcloud_talk; (b) phone set —
  whatsapp (cloud+web), signal; (c) other fields — linq (`allowed_senders`),
  imessage (`allowed_contacts`, macOS-gated).
- **Phase 3 — integration + release (sequential):** workspace build + scoped
  tests + lint (1.92.0) + `publish=false` cross-compile verify on all 6 targets;
  version bump 0.6.82-alpha + CHANGELOG; PR → merge (Build Smoke) → tag → publish.

### Risks

- Per-channel inbound-loop shapes differ; the adapter must isolate that. Mitigate
  by porting Telegram first and codifying the exact hook point.
- Persisting config from a running channel races other writers — reuse the
  existing `Config::save` + the managed-daemon reload path; the store has its own
  file lock.
- Some channels (IRC nick, Slack id vs name) have weaker identity stability — the
  adapter persists all available forms and documents the caveat.
