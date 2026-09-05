# Plan 115: Channel allowlist edits reach the running runtime without a daemon restart

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/traits.rs src/channels/mod.rs src/channels/telegram.rs src/gateway/config_api.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

An operator edits the Telegram allowlist in the web console. The allowlist saves,
and then the gateway dies and has to be restarted by hand. This is the single
most-reported symptom in this subsystem and it was reproduced and root-caused
against a live daemon.

It is not a crash. `POST /api/v1/config/channels/telegram` persists the config and
then deliberately restarts the whole managed service 750 ms later — and the
gateway is hosted **inside** that service, so the request handler kills the
process that served it. There is no debounce, so N saves schedule N restarts;
systemd's start rate limiter (`StartLimitBurst=5` per `StartLimitIntervalUSec=10s`)
trips at five saves in ten seconds and leaves the unit `failed` with no automatic
recovery.

The restart exists because the gateway has no handle to the Telegram channel — it
polls from a separate runtime component. But the channels runtime **does** hold
every live channel handle, it **already** hot-reloads other config per message,
the Telegram allowlist is **already** a runtime-mutable lock with a mutator, and
the Linq pairing path **already** does exactly this in-place update. Every piece
of the fix exists; they are simply not connected.

After this plan: an allowlist-only edit applies to the running channel within one
message, with no restart. Connect, token change and disconnect still restart,
because those genuinely create or destroy a channel.

## Current state

### The restart chain

`src/gateway/config_api.rs:616-632` — the allowlist-only save path:

```rust
    let (_guard, mut cfg) = lock_and_load().await?;
    let tg = apply_telegram_update(
        cfg.channels_config.telegram.clone(),
        new_token.as_deref(),
        body.allowed_users.clone(),
    )?;
    cfg.channels_config.telegram = Some(tg);
    persist_and_swap(&state, cfg).await?;

    // The running channels runtime doesn't hot-reload channel config from disk,
    // so ask a managed daemon to restart and pick up the change (detached, after
    // the response flushes).
    schedule_daemon_reload();
```

`src/gateway/config_api.rs:549-569` — what that schedules:

```rust
fn schedule_daemon_reload() {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        match tokio::task::spawn_blocking(crate::channels::reload_managed_daemon).await {
```

`src/channels/mod.rs:216-217` — what that runs:

```rust
const SYSTEMD_STATUS_ARGS: [&str; 3] = ["--user", "is-active", "rantaiclaw.service"];
const SYSTEMD_RESTART_ARGS: [&str; 3] = ["--user", "restart", "rantaiclaw.service"];
```

`src/daemon/mod.rs:74-87` and `:146` — the daemon hosts the gateway in-process:

```rust
    let mut gateway_handle = {
        …
        async move { crate::gateway::run_gateway(&host, port, cfg, sd).await }
    };
    …
    println!("   Components: gateway, channels, heartbeat, scheduler");
```

### Why the gateway cannot reach the channel today

`src/gateway/mod.rs:429-466` — `AppState` holds handles only for webhook-driven
channels. There is no `telegram` field:

```rust
pub struct AppState {
    …
    pub whatsapp: Option<Arc<WhatsAppChannel>>,
    pub linq: Option<Arc<LinqChannel>>,
    pub nextcloud_talk: Option<Arc<NextcloudTalkChannel>>,
    …
}
```

### The seam that already exists

`src/channels/mod.rs:222-223` — the channels runtime holds every live handle:

```rust
struct ChannelRuntimeContext {
    channels_by_name: Arc<HashMap<String, Arc<dyn Channel>>>,
```

`src/channels/mod.rs:648-669` — the per-message hot-reload, keyed on a config-file
stamp, already re-applies autonomy, owners and allowed-commands:

```rust
async fn maybe_apply_runtime_config_update(ctx: &ChannelRuntimeContext) -> Result<()> {
    let Some(config_path) = runtime_config_path(ctx) else {
        return Ok(());
    };
    let Some(stamp) = config_file_stamp(&config_path).await else {
        return Ok(());
    };
    {
        let store = runtime_config_store()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(state) = store.get(&config_path) {
            if state.last_applied_stamp == Some(stamp) {
                return Ok(());
            }
        }
    }
    let (next_defaults, next_autonomy) =
        load_runtime_defaults_from_config_file(&config_path).await?;
```

`src/channels/telegram.rs:310` — the allowlist is already runtime-mutable:

```rust
    allowed_users: Arc<RwLock<Vec<String>>>,
```

`src/channels/telegram.rs:492-502` — the mutator already exists:

```rust
    fn add_allowed_identity_runtime(&self, identity: &str) {
        let normalized = Self::normalize_identity(identity);
        if normalized.is_empty() {
            return;
        }
        if let Ok(mut users) = self.allowed_users.write() {
            if !users.iter().any(|u| u == &normalized) {
                users.push(normalized);
            }
        }
    }
```

`src/gateway/mod.rs:2120-2126` — and the precedent, with a comment naming the goal:

```rust
    // Intercept `/bind` / `/claim` self-onboarding BEFORE the allowlist gate in
    // `parse_webhook_payload` drops unknown senders. A successful pairing appends
    // the sender to `allowed_senders` (+ `approval_owners` for an owner-capable
    // `/claim`) and persists config; mirror it into the runtime allowlist so the
    // next message is accepted without a gateway restart.
```

### Conventions this plan must follow

- New capability goes on the trait with a defaulted method, matching
  `render_target` at `src/channels/traits.rs:83-91` — read that method and its
  doc comment and copy the shape, including documenting the default.
- Error handling in `maybe_apply_runtime_config_update` is "apply what you can,
  log what you cannot, advance the stamp" — see the failure branch at
  `src/channels/mod.rs:698-731`. Match it: a channel that cannot apply an
  allowlist must not abort the whole reload.
- `tracing`, never `println!`/`eprintln!`, anywhere on the runtime path — see the
  comment at `src/channels/mod.rs:1659-1666` explaining why.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Scoped tests | `cargo test --lib channels::` | all pass |
| Gateway tests | `cargo test --lib gateway::` | all pass |
| Config API tests | `cargo test --test config_api` | all pass |

**Do not run a bare `cargo test`** — the workspace suite writes ~27 GB and this
machine is disk-constrained. Use the scoped commands above.

## Scope

**In scope** (the only files you may modify):
- `src/channels/traits.rs` — add the defaulted trait method
- `src/channels/telegram.rs` — implement it
- `src/channels/mod.rs` — apply allowlists in the hot-reload seam
- `src/gateway/config_api.rs` — stop restarting for an allowlist-only update

**Out of scope** (do NOT touch, even though they look related):
- `src/channels/{matrix,lark,dingtalk,qq,nextcloud_talk}.rs` — these five hold
  their allowlist as a plain `Vec<String>` and need the same treatment, but each
  is owned by its own platform plan (124, 128, 129). Adding the trait method with
  a **no-op default** is exactly what lets them land separately. Do not convert
  them here.
- `schedule_daemon_reload` itself, and the connect/token-change/disconnect paths —
  a restart is correct for those. Only the allowlist-only branch changes.
- The systemd start-limit behaviour. Once allowlist saves stop restarting, the
  repeated-restart trip is unreachable from this endpoint; do not add debounce
  logic in this plan.
- `src/daemon/mod.rs` — the daemon's in-process hosting is by design.

## Git workflow

- Branch: `fix/channel-allowlist-reaches-runtime`
- Conventional-commit messages, matching `git log` style, e.g.
  `fix(channels): apply allowlist edits to the live channel without a restart`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the defaulted trait method

In `src/channels/traits.rs`, next to `render_target`, add:

```rust
    /// Replace this channel's runtime sender allowlist.
    ///
    /// Called by the channels runtime when `config.toml` changes, so an
    /// allowlist edit from the console or the CLI reaches a running listener
    /// without a restart. Channels that hold their allowlist behind a lock
    /// override this; the default is a no-op, which means the channel keeps
    /// its boot-time list until it is restarted.
    fn apply_allowed_senders(&self, _allowed: &[String]) {}
```

Match the surrounding doc-comment style. The default must be a no-op so the five
`Vec<String>` channels compile unchanged.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Implement it for Telegram

In `src/channels/telegram.rs`, inside the `impl Channel for TelegramChannel`
block, add an override that normalizes each entry through the existing
`Self::normalize_identity` and replaces the list wholesale:

```rust
    fn apply_allowed_senders(&self, allowed: &[String]) {
        let normalized = Self::normalize_allowed_users(allowed.to_vec());
        if let Ok(mut users) = self.allowed_users.write() {
            if *users != normalized {
                tracing::info!(
                    target: "channels",
                    channel = "telegram",
                    count = normalized.len(),
                    "applied updated allowlist from config"
                );
                *users = normalized;
            }
        }
    }
```

Reuse `normalize_allowed_users` (it exists at `src/channels/telegram.rs:405`) so
the runtime list is normalized identically to the constructor's.

**Verify**: `cargo test --lib channels::telegram` → all pass.

### Step 3: Carry per-channel allowlists through the reload

`load_runtime_defaults_from_config_file` currently returns
`(ChannelRuntimeDefaults, AutonomyConfig)`. Extend the loaded data with the
per-channel allowlists — a `HashMap<String, Vec<String>>` keyed by the same
lowercase channel name used in `channels_by_name` (`"telegram"`, `"discord"`, …).

Populate it from `config.channels_config` for every channel section that is
`Some`, using that section's allowlist field. Note the field is spelled
differently per channel (`allowed_users`, `allowed_numbers`, `allowed_from`,
`allowed_senders`, `allowed_contacts`) — read
`src/channels/pairing.rs:117-133`, which already enumerates that mapping, and
mirror it exactly rather than inventing a second mapping.

**Verify**: `cargo test --lib channels::` → all pass (no behaviour change yet).

### Step 4: Apply the allowlists in the reload seam

In `maybe_apply_runtime_config_update`, after `ctx.security.apply_config(&next_autonomy)`
at `src/channels/mod.rs:688` and **before** the provider rebuild, iterate the map
and call the new trait method on each live handle:

```rust
    for (name, allowed) in &next_allowlists {
        if let Some(channel) = ctx.channels_by_name.get(name.as_str()) {
            channel.apply_allowed_senders(allowed);
        }
    }
```

Placing it before the provider rebuild matters: the existing comment at
`src/channels/mod.rs:674-680` establishes that safety-relevant settings must apply
even when the provider fails to build. An allowlist tightening is safety-relevant,
so it must not be skipped on the provider-failure path.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 5: Stop restarting the daemon for an allowlist-only update

In `src/gateway/config_api.rs`, `connect_telegram` already distinguishes the two
cases via `TokenPlan` (`src/gateway/config_api.rs:478-513`): `TokenPlan::Validate`
means a new token, `TokenPlan::KeepExisting` means an allowlist-only edit.

Call `schedule_daemon_reload()` only when a token was supplied — i.e. when
`new_token.is_some()`. On the allowlist-only path, skip it.

Update the response `note` so the two cases say different, true things:
- token change / fresh connect: keep the existing "Saved. Reloading the runtime to
  apply…" wording.
- allowlist-only: something like `"Saved. The running channel picks this up on its
  next message — no restart."`

Leave `disconnect_telegram`'s call at `src/gateway/config_api.rs:680` unchanged.

**Verify**: `cargo test --test config_api` → all pass.

## Test plan

New tests, modelled on the existing reload tests in `src/channels/mod.rs`'s test
module — use `maybe_apply_runtime_config_update_hot_reloads_owners_guest_gate_and_allowed_commands`
(`src/channels/mod.rs:5071`) as the structural pattern; it already builds a temp
config, rewrites it, and asserts the runtime picked the change up.

1. `allowlist_edit_reaches_the_live_channel_without_restart` — build a context
   containing a recording channel whose `apply_allowed_senders` captures its
   argument; write a config with `allowed_users = ["a"]`; run the reload; rewrite
   the config with `["a", "b"]`; run the reload again; assert the channel received
   `["a", "b"]`.
2. `allowlist_applies_even_when_the_provider_fails_to_build` — same shape, but with
   a config whose provider cannot be constructed (mirror the existing
   `maybe_apply_runtime_config_update_applies_autonomy_when_provider_build_fails`
   test at `src/channels/mod.rs:5201`). Assert the allowlist still applied.
3. `telegram_apply_allowed_senders_normalizes_like_the_constructor` — in
   `src/channels/telegram.rs`, assert `apply_allowed_senders(&["@Alice".into()])`
   yields the same stored form as `TelegramChannel::new(.., vec!["@Alice".into()], ..)`.
4. In `tests/config_api.rs`: `allowlist_only_update_does_not_schedule_a_restart`
   and `token_change_still_schedules_a_restart`. If `schedule_daemon_reload` is not
   observable from a test, make it observable by extracting the decision into a
   small pure function (`fn needs_runtime_restart(plan: &TokenPlan) -> bool`) and
   test that instead — do not add a global.

**Mutation check (required).** For test 1, invert the guard you added in step 4
(skip the loop entirely) and confirm the test **fails**. A test that passes with
the fix removed is worthless; this repo has a documented history of exactly that.
Restore the code afterwards.

**Verify**: `cargo test --lib channels::` and `cargo test --test config_api` →
all pass, including the four new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` passes, including the three new channel tests
- [ ] `cargo test --test config_api` passes, including the two new API tests
- [ ] The mutation check in the test plan was performed and the test failed as
      expected with the fix removed
- [ ] `grep -n 'schedule_daemon_reload' src/gateway/config_api.rs` shows it is no
      longer called unconditionally in `connect_telegram`
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 115 updated

## STOP conditions

Stop and report back (do not improvise) if:

- The code at the locations in "Current state" does not match the excerpts.
- `ChannelRuntimeContext.channels_by_name` turns out not to be keyed by the same
  lowercase names as the config sections — the mapping is load-bearing for step 4
  and guessing at it would silently apply an allowlist to the wrong channel, or to
  none.
- Adding a defaulted trait method breaks a channel that is behind a feature flag
  you cannot compile (`channel-matrix` cannot be built at all — that is expected
  and is exactly why the default is a no-op; if it breaks anyway, stop).
- You find that a channel other than Telegram already overrides an equivalent
  method under a different name — that would mean two mechanisms, and the plan
  needs revising before you add a third.
- Test 1 still passes after you invert the step-4 guard.

## Maintenance notes

- **What interacts with this**: plans 124, 128 and 129 convert the five
  `Vec<String>` channels and will override `apply_allowed_senders`. When they do,
  the no-op default should end up with few or no implementers left — if it still
  has many, the trait method is not being adopted and that is worth asking about.
- **What a reviewer should scrutinise**: that the allowlist application sits
  *before* the provider rebuild (step 4), and that the allowlist-only branch is
  the only one that stopped restarting (step 5). Getting either backwards
  reintroduces the bug in a subtler form.
- **Deliberately deferred**: debouncing `schedule_daemon_reload`. Once allowlist
  saves stop calling it, the systemd start-limit trip is unreachable from this
  endpoint. If a future endpoint calls it in a loop, add the debounce then — not
  speculatively now.
- The console half of this bug is plan 137, in the claw-ui repo: it reports
  success optimistically, suppresses the gateway's restart notice, and has no
  reconnect state. Fixing only this side leaves the operator's experience
  improved but still confusing on the connect/token paths that legitimately
  restart.
