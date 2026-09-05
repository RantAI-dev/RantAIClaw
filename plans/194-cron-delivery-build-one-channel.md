# Plan 194: Cron delivery builds one channel, and the "Slack app_token ignored" warning is stated once

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/channels/factory.rs src/channels/mod.rs src/channels/admin.rs src/cron/scheduler.rs`
> If any of those changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`deliver_if_configured` (the cron delivery path) calls
`build_configured_channels(config)`, which constructs **every** configured
channel (~15 structs + their HTTP clients), then throws all but one away via
`.find(...)`. Two costs:

1. **A recurring, misleading warning.** `build_configured_channels` emits
   `tracing::warn!("Slack: app_token is set but ignored …")` as a *construction
   side effect* (`src/channels/factory.rs:82`). Any operator who has
   `slack.app_token` set therefore gets that warning re-emitted on **every
   scheduled delivery** (and every `doctor` run) — it reads like a recurring
   fault when it is a one-time config note.
2. **Wasted allocation.** Every delivery builds ~15 channels to use one.

After this plan: the factory gains a `build_one(config, key)` entry point that
constructs exactly the one channel the delivery needs; `deliver_if_configured`
uses it; and the Slack `app_token` warning moves to the operator-facing startup
and doctor paths, so it is stated once per operator action and never on a
scheduled delivery. Constructors are otherwise pure, so this is behaviour-
preserving apart from the removed log-spam.

## Current state

- `src/channels/factory.rs`
  - `build_configured_channels(config) -> Vec<(&'static str, &'static str, Arc<dyn Channel>)>`
    (line 31) is the single construction table. It builds each configured
    channel. The 4 announce-capable channels are built at:
    - telegram: lines 36–50
    - discord: lines 52–69
    - slack: lines 71–96 — this branch contains the warning to move:
      ```rust
      if sl.app_token.as_deref().is_some_and(|t| !t.trim().is_empty()) {
          tracing::warn!(
              "Slack: `app_token` is set but ignored — this build polls conversations.history \
               and does not implement Socket Mode. Remove the key, or leave it for when it does."
          );
      }
      channels.push((
          "slack",
          "Slack",
          Arc::new(SlackChannel::new(
              sl.bot_token.clone(),
              sl.channel_id.clone(),
              sl.allowed_users.clone(),
          )),
      ));
      ```
    - mattermost: lines 98–111
  - The imports at the top already bring in `TelegramChannel`, `DiscordChannel`,
    `SlackChannel`, `MattermostChannel`, `Config`, `Arc`, and `Channel` (lines 6–14).
  - There is **no** `#[cfg(test)] mod tests` block in this file yet.

- `src/channels/mod.rs`
  - Line 43: `pub(crate) use factory::build_configured_channels;`
  - Line 65 re-exports `channel_supports_announce_delivery` from `prompt`.
  - Channel-server startup builds channels at line 760:
    ```rust
    let channels: Vec<Arc<dyn Channel>> = factory::build_configured_channels(&config)
        .into_iter()
        .map(|(_key, _display, channel)| channel)
        .collect();
    ```

- `src/channels/prompt.rs:24` — `channel_supports_announce_delivery` matches
  exactly `"telegram" | "discord" | "slack" | "mattermost"`. Cron delivery is
  gated on this (see below), so `build_one` only needs to cover those 4 keys.

- `src/channels/admin.rs:425` — `doctor_channels` builds channels:
  `let channels = factory::build_configured_channels(&config);`

- `src/cron/scheduler.rs` — `deliver_if_configured` (lines 360–394). The gate at
  383 (`channel_supports_announce_delivery(&key)`) already guarantees `key` is
  one of the 4 announce channels before the build:
  ```rust
  let key = channel.to_ascii_lowercase();
  if !crate::channels::channel_supports_announce_delivery(&key) {
      anyhow::bail!("unsupported delivery channel: {key}");
  }

  let built = crate::channels::build_configured_channels(config);
  let Some((_, _, channel_impl)) = built.into_iter().find(|(k, _, _)| *k == key) else {
      anyhow::bail!("{key} channel not configured");
  };
  channel_impl.send(&SendMessage::new(output, target)).await?;
  ```

- A compact `TelegramConfig` literal (for the new test), from
  `src/approval/permissions.rs:308`:
  ```rust
  crate::config::schema::TelegramConfig {
      bot_token: "placeholder-token".to_string(),
      allowed_users: vec![],
      stream_mode: crate::config::schema::StreamMode::default(),
      draft_update_interval_ms: 1_000,
      interrupt_on_new_message: false,
      mention_only: false,
  }
  ```

Convention: keep the delivery path free of construction warnings; keep the
factory the single construction site (plan 121). `build_one` duplicates only the
4 announce-capable constructors — acceptable because that is the exact set the
delivery gate allows (documented in Step 1).

## Commands you will need

| Purpose   | Command                                             | Expected on success       |
|-----------|-----------------------------------------------------|---------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff           |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings       |
| New tests | `cargo test --lib build_one`                        | new tests pass            |
| Cron      | `cargo test --lib cron`                             | compiles whole lib; passes|

Do **not** run a bare `cargo test` (workspace test is disk-heavy on this box).

## Scope

**In scope** (the only files you should modify):

- `src/channels/factory.rs` — add `build_one`; add `warn_unused_channel_config`;
  remove the Slack warning from `build_configured_channels`; add a test module.
- `src/channels/mod.rs` — re-export `build_one`; call
  `warn_unused_channel_config` at channel-server startup.
- `src/channels/admin.rs` — call `warn_unused_channel_config` in `doctor_channels`.
- `src/cron/scheduler.rs` — switch `deliver_if_configured` to `build_one`.

**Out of scope** (do NOT touch):

- The 11 non-announce channel constructors in `build_configured_channels`
  (imessage/signal/whatsapp/irc/linq/nextcloud_talk/lark/dingtalk/qq/email/matrix).
- `channel_supports_announce_delivery` — the delivery gate stays these 4 keys.
- The `SendMessage`/`send` call itself — behaviour unchanged.

## Git workflow

- Branch: `advisor/194-cron-delivery-build-one-channel`
- Conventional commits, e.g.
  `perf(channels): build one channel for cron delivery; state Slack warning once`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add `build_one` to the factory

In `src/channels/factory.rs`, add a function that constructs exactly one of the
announce-capable channels by key. Place it after `build_configured_channels`:

```rust
/// Build exactly one channel by its lowercase `key`, for the cron delivery path,
/// which needs a single target — not the whole fleet. Covers only the
/// announce-capable channels (the set `channel_supports_announce_delivery`
/// allows, which is the only set cron delivery selects on); returns `None` for
/// any other key or when that channel is not configured.
///
/// Unlike `build_configured_channels` this allocates one channel, not ~15, and
/// emits no construction-time warnings — the delivery path must not re-log on
/// every scheduled run. Keep the key set here a superset of
/// `channel_supports_announce_delivery`; if that gate widens, add the key here.
pub(crate) fn build_one(config: &Config, key: &str) -> Option<Arc<dyn Channel>> {
    match key {
        "telegram" => config.channels_config.telegram.as_ref().map(|tg| {
            Arc::new(
                TelegramChannel::new(
                    tg.bot_token.clone(),
                    tg.allowed_users.clone(),
                    tg.mention_only,
                )
                .with_streaming(tg.stream_mode, tg.draft_update_interval_ms)
                .with_multimodal(config.multimodal.clone()),
            ) as Arc<dyn Channel>
        }),
        "discord" => config.channels_config.discord.as_ref().map(|dc| {
            Arc::new(
                DiscordChannel::new(
                    dc.bot_token.clone(),
                    dc.guild_id.clone(),
                    dc.allowed_users.clone(),
                    dc.listen_to_bots,
                    dc.mention_only,
                )
                .with_multimodal(config.multimodal.clone()),
            ) as Arc<dyn Channel>
        }),
        "slack" => config.channels_config.slack.as_ref().map(|sl| {
            Arc::new(SlackChannel::new(
                sl.bot_token.clone(),
                sl.channel_id.clone(),
                sl.allowed_users.clone(),
            )) as Arc<dyn Channel>
        }),
        "mattermost" => config.channels_config.mattermost.as_ref().map(|mm| {
            Arc::new(MattermostChannel::new(
                mm.url.clone(),
                mm.bot_token.clone(),
                mm.channel_id.clone(),
                mm.allowed_users.clone(),
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            )) as Arc<dyn Channel>
        }),
        _ => None,
    }
}
```

Copy each constructor's argument list **verbatim** from the matching branch of
`build_configured_channels` so the two cannot drift on the fields.

**Verify**: `cargo fmt --all -- --check` → exit 0.

### Step 2: Move the Slack `app_token` warning to a dedicated operator-path helper

In `src/channels/factory.rs`:

1. Remove the `if sl.app_token.as_deref().is_some_and(...) { tracing::warn!(...) }`
   block from the slack branch of `build_configured_channels` (lines 77–86).
   Keep the `channels.push(("slack", "Slack", …))` that follows it.

2. Add a dedicated helper:

```rust
/// One-time, operator-facing warnings about channel config that is set but
/// ignored. Call from the operator paths (channel-server startup, doctor) — NOT
/// from cron delivery, which must not re-log on every scheduled run.
pub(crate) fn warn_unused_channel_config(config: &Config) {
    if let Some(ref sl) = config.channels_config.slack {
        if sl
            .app_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            tracing::warn!(
                "Slack: `app_token` is set but ignored — this build polls conversations.history \
                 and does not implement Socket Mode. Remove the key, or leave it for when it does."
            );
        }
    }
}
```

**Verify**: `cargo fmt --all -- --check` → exit 0.

### Step 3: Re-export `build_one` and call the warning helper from the startup path

In `src/channels/mod.rs`:

1. Change line 43 from
   `pub(crate) use factory::build_configured_channels;` to
   `pub(crate) use factory::{build_configured_channels, build_one};`

2. At the channel-server startup site (line 760), add a call to the warning
   helper immediately **before** building the channel list:
   ```rust
   factory::warn_unused_channel_config(&config);
   let channels: Vec<Arc<dyn Channel>> = factory::build_configured_channels(&config)
       .into_iter()
       .map(|(_key, _display, channel)| channel)
       .collect();
   ```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Call the warning helper from `doctor_channels`

In `src/channels/admin.rs`, in `doctor_channels`, add the warning call
immediately before line 425:

```rust
factory::warn_unused_channel_config(&config);
let channels = factory::build_configured_channels(&config);
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 5: Switch `deliver_if_configured` to `build_one`

In `src/cron/scheduler.rs`, replace the build-all-then-find block (lines 387–391)
with a single-channel build:

```rust
let Some(channel_impl) = crate::channels::build_one(config, &key) else {
    anyhow::bail!("{key} channel not configured");
};
channel_impl.send(&SendMessage::new(output, target)).await?;
```

(The gate at line 383 already restricts `key` to the 4 announce channels, which
`build_one` covers.)

**Verify**: `cargo test --lib cron` → compiles the whole lib and cron tests pass.

### Step 6: Add tests for `build_one`

Add a `#[cfg(test)] mod tests` block at the end of `src/channels/factory.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_telegram() -> Config {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::schema::TelegramConfig {
            bot_token: "placeholder-token".to_string(),
            allowed_users: vec![],
            stream_mode: crate::config::schema::StreamMode::default(),
            draft_update_interval_ms: 1_000,
            interrupt_on_new_message: false,
            mention_only: false,
        });
        config
    }

    #[test]
    fn build_one_returns_the_configured_announce_channel() {
        let config = config_with_telegram();
        let ch = build_one(&config, "telegram").expect("telegram is configured");
        assert_eq!(ch.name(), "telegram");
    }

    #[test]
    fn build_one_is_none_for_unconfigured_channel() {
        let config = config_with_telegram();
        assert!(build_one(&config, "discord").is_none());
    }

    #[test]
    fn build_one_is_none_for_non_announce_key() {
        let config = config_with_telegram();
        // email is not an announce-capable delivery target.
        assert!(build_one(&config, "email").is_none());
    }
}
```

If `Config::default()` is not directly constructible or `channels_config` is not
a public field in this crate's test scope, use the nearest existing test config
builder (search the crate for `Config::default()` used in tests — e.g.
`src/cron/scheduler.rs` tests build `Config { .. ..Config::default() }`). Adjust
field access to match the actual `channels_config` shape.

**Verify**: `cargo test --lib build_one` → the 3 new tests pass.

## Test plan

- New tests in `src/channels/factory.rs` `mod tests`:
  - `build_one_returns_the_configured_announce_channel` — the happy path
    (constructs exactly the requested channel).
  - `build_one_is_none_for_unconfigured_channel` — returns `None` when the key's
    channel is not configured.
  - `build_one_is_none_for_non_announce_key` — returns `None` for keys outside
    the announce set.
- Verification: `cargo test --lib build_one` (new tests) and
  `cargo test --lib cron` (delivery path compiles + cron tests pass).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib build_one` passes (3 new tests)
- [ ] `cargo test --lib cron` exits 0 (whole-lib build green)
- [ ] `grep -n "app_token" src/channels/factory.rs` shows the warning only
      inside `warn_unused_channel_config`, not in `build_configured_channels`
- [ ] `grep -n "build_configured_channels" src/cron/scheduler.rs` returns no
      matches (delivery now uses `build_one`)
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `build_configured_channels`, `deliver_if_configured`, or the two operator call
  sites don't match the "Current state" excerpts (drift since this plan).
- Any announce channel's constructor signature differs from what Step 1 copied
  (e.g. `TelegramChannel::new` took different args) — build_one must match the
  live constructor exactly; if it differs from `build_configured_channels`, that
  is itself a drift STOP.
- A verification fails twice after a reasonable fix attempt.
- `Config::default()` cannot be built in the factory test scope and no existing
  test config builder is reachable — report rather than inventing one.

## Maintenance notes

- **Drift coupling**: `build_one` duplicates the 4 announce-capable constructors
  from `build_configured_channels`. If a channel's constructor args change, both
  sites must change. Kept intentionally small (4 channels) because that is the
  exact set the cron delivery gate allows.
- **Keep the key set in sync**: `build_one` must cover every key
  `channel_supports_announce_delivery` returns true for. If that gate is ever
  widened (a capability change, per the scheduler comment at scheduler.rs:379),
  add the new key to `build_one` too.
- The Slack `app_token` warning now fires once per channel-server start and once
  per `doctor` run — both operator-initiated — and never during a scheduled
  delivery. A reviewer should confirm no other caller relied on
  `build_configured_channels` emitting that warning.
