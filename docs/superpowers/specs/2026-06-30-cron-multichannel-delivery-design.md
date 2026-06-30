# Cron multi-channel delivery (Hermes/OpenClaw parity)

Date: 2026-06-30
Status: Approved (scope set by owner: "core + webhook"; built autonomously in worktree)
Branch: `worktree-feat+cron-multichannel-delivery`
Risk tier: High (touches `src/cron/**`; reads live `src/channels/**` state) — see CLAUDE.md §5.

## Problem

A scheduled cron job cannot deliver its output to WhatsApp, even though the daemon
is logged into WhatsApp and can already reply to inbound messages. The cron
scheduler's `deliver_if_configured` (`src/cron/scheduler.rs`) only has hard-coded
`announce` arms for `telegram`, `discord`, `slack`, and `mattermost`; every other
channel — including `whatsapp`, `signal`, `dingtalk`, `qq`, `nextcloud_talk` —
falls through to `bail!("unsupported delivery channel")`.

Two root causes:

1. **No generic routing.** Delivery is a 4-arm `match` on channel name, each arm
   re-constructing a fresh stateless HTTP channel from config.
2. **No access to live sessions.** WhatsApp here runs in *web mode* (`wa-rs`):
   its live socket/login exists only inside the daemon's channel-supervisor task.
   A freshly constructed `WhatsAppWebChannel` has no logged-in client, so the
   "re-construct from config" pattern cannot work for stateful channels. The
   scheduler task receives only `Config` (`src/daemon/mod.rs:106` →
   `scheduler::run(cfg)`); it never sees the live channel instances.

## Goal (parity target)

Match how Hermes/OpenClaw behave: a single long-lived process owns the channel
connections, and the scheduler reuses them to deliver scheduled output to *any*
configured channel.

- **Core:** `delivery.mode = "announce"` routes to **every configured channel**
  generically (telegram, whatsapp, discord, slack, mattermost, signal, dingtalk,
  qq, nextcloud_talk, imessage, matrix), reusing the daemon's *live* channel
  session — so WhatsApp web works with the existing login, no re-pairing.
- **Webhook:** `delivery.mode = "webhook"` POSTs job output to a URL (OpenClaw
  `delivery.mode` parity: `announce | none | webhook`).
- **CLI parity:** `cron add` exposes `--announce --channel <name> --to <target>`
  and `--webhook <url>` flags (OpenClaw-shaped), in addition to the existing
  agent `cron_add` tool `delivery` JSON object (already works).

rantaiclaw already matches OpenClaw on the rest of the model: `DeliveryConfig
{mode, channel, to, best_effort}`, `Schedule::{Cron{expr,tz}, At, Every}`,
`SessionTarget::{Isolated, Main}`, `JobType::{Shell, Agent}`.

## Non-goals (YAGNI)

- Hermes "continuable / reply-into-thread" delivery (delivered brief becomes a
  thread session). Deferred until requested.
- Telegram forum-topic `-100…:topic:N` addressing sugar. The raw `to` already
  passes through; no special parsing now.
- Config schema change. `DeliveryConfig` is per-job state in `cron/jobs.db`, not
  in `config.toml`; no `schema_version` bump. `[cron]` config is unchanged.

## Design

### 1. Live channel registry (`src/channels/registry.rs`, new)

A process-global registry of the live channel instances, published by the
channel supervisor and read by the cron scheduler. The daemon runs the channel
supervisor and the scheduler in one process, so a process-global is the correct,
minimal sharing mechanism — and it matches the file's existing global-state
pattern (`runtime_config_store()`).

```rust
// keyed by Channel::name() => the SAME Arc the listener task drives.
static LIVE_CHANNELS: OnceLock<RwLock<HashMap<String, Arc<dyn Channel>>>> = ...;

pub fn publish_live_channels(map: Arc<HashMap<String, Arc<dyn Channel>>>);
pub fn live_channel(name: &str) -> Option<Arc<dyn Channel>>;
pub fn clear_live_channels(); // shutdown / test hygiene
```

Why this is safe for WhatsApp web: the registry stores the *same* `Arc<dyn
Channel>` that the supervised `listen()` task drives. `WhatsAppWebChannel::send`
locks `self.client` — which `listen()` populates on connect — so a `send()`
issued via the registry uses the one live socket. No second client, no
re-pairing.

### 2. Publish on startup (`src/channels/mod.rs`)

In `start_channels_with_cancellation`, immediately after `channels_by_name` is
built (currently `mod.rs:3434`), publish a clone:

```rust
let channels_by_name = Arc::new(/* existing collect */);
registry::publish_live_channels(Arc::clone(&channels_by_name)); // NEW (1 line)
```

`channels_by_name` already maps `Channel::name()` → live `Arc<dyn Channel>`.
On shutdown (cancellation path) call `registry::clear_live_channels()`.

### 3. Generic delivery routing (`src/cron/scheduler.rs::deliver_if_configured`)

Replace the 4-arm match with mode dispatch:

- `mode == "none"` (or empty/default) → no-op `Ok(())` (unchanged).
- `mode == "announce"`:
  - require `channel` and `to` (existing error messages).
  - `registry::live_channel(channel)` →
    - `Some(ch)` → `ch.send(&SendMessage::new(output, to)).await` — works for
      every channel, stateful or stateless, reusing the warm live session.
    - `None` → **fallback** for the known *stateless* channels only
      (telegram/discord/slack/mattermost): construct from config as today, so a
      scheduler-only deployment (no channel supervisor in-process) still works.
      For a stateful channel (whatsapp) or an unknown key with no live session:
      `bail!("channel '{channel}' is not running; cannot deliver")`.
- `mode == "webhook"`:
  - require a URL: use `delivery.to` as the URL (KISS — `to` is "destination";
    OpenClaw posts to a URL). POST JSON `{job_id, name, output, fired_at}` via
    the existing `reqwest` (0.12) client. Non-2xx → `Err`.
- unknown mode → `bail!("unsupported delivery mode: {mode}")`.

`best_effort` handling in `persist_job_result` is unchanged: a delivery `Err`
under `best_effort=true` (default) is logged as a warning and does not fail the
job; under `best_effort=false` it marks the run failed.

### 4. CLI flags (`src/main.rs` + `src/cron/mod.rs`)

Add to the `cron add` / `add-at` / `add-every` / `once` subcommands:
`--announce`, `--channel <name>`, `--to <target>`, `--webhook <url>`, mapping to
`DeliveryConfig{ mode: "announce"|"webhook", channel, to, best_effort: true }`.
Mutually-exclusive validation: `--webhook` ⇒ webhook mode (channel ignored);
`--announce` ⇒ requires `--channel` + `--to`.

## Data flow

```
cron fires (daemon process)
  → execute_job (shell stdout | agent response) => output: String
  → persist_job_result
      → deliver_if_configured(config, job, output)
          announce → registry::live_channel(job.delivery.channel)
                       → WhatsAppWebChannel.send(output → +62…)  [live wa-rs socket]
          webhook  → reqwest POST job.delivery.to {output,…}
      → best_effort gate → record run status
```

## Error handling

- Missing `channel`/`to` in announce → explicit `Err` (kept).
- Channel configured-but-not-running (no live session, stateful) → explicit
  `Err("channel '…' is not running; cannot deliver")` rather than silent success.
- Webhook non-2xx / transport error → `Err`.
- All surfaced through the existing `best_effort` policy; nothing silently
  broadens behavior (CLAUDE.md §3.5).

## Testing

Deterministic, no live phone required (CLAUDE.md §3.7):

1. `registry`: publish then `live_channel` returns the Arc; `clear` empties it.
2. `deliver_if_configured` announce → a **fake `Channel`** registered as
   `"whatsapp"` captures the `SendMessage` (content == output, recipient == to).
   Proves WhatsApp + generic routing end-to-end without the wa-rs socket.
3. announce, channel **not running**, stateful key → returns `Err(not running)`.
4. announce, stateless key not running → constructs-from-config fallback path
   selected (assert it does not hit the "not running" error; transport may fail
   in test, which is fine — assert the *routing decision*).
5. `none` mode → `Ok(())`, no send.
6. `webhook` mode → spins a one-shot local TCP listener, asserts it receives a
   POST whose body contains the output; non-2xx → `Err`.
7. unknown mode → `Err`.

Plus: `cargo fmt --all -- --check`, `cargo clippy --all-targets -D warnings`,
scoped `cargo test --lib cron` and `--lib channels::registry` (never full-crate
test — OOM), and a full `cargo build` to confirm the binary links.

Live WhatsApp e2e (deferred to owner-present): the running production daemon is
the old binary and its WhatsApp web login is a single live session; restarting it
unattended could require phone re-pairing. Documented run-steps provided; not run
unattended to honor "don't affect my other work".

## Files touched

- `src/channels/registry.rs` — new (~40 LoC + tests).
- `src/channels/mod.rs` — `mod registry;` + publish (startup) + clear (shutdown).
- `src/cron/scheduler.rs` — rewrite `deliver_if_configured`; add webhook helper + tests.
- `src/main.rs`, `src/cron/mod.rs` — CLI delivery flags.
- `docs/commands-reference.md` (cron flags), CHANGELOG — runtime-contract docs.

## Rollback

Self-contained on one branch. Revert the branch/PR. The registry is additive
(publish is one call; if absent, scheduler falls back to the old stateless arms),
so partial revert degrades gracefully to prior behavior. No data migration.
