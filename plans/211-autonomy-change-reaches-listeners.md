# Plan 211: Make a runtime autonomy change force a reload and actually reach running channel listeners

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tui/app.rs src/tui/commands/autonomy.rs src/tui/commands/permissions.rs src/tui/commands/mod.rs`

## Status

- **Priority**: P2 (security — a tightening is not enforced fleet-wide)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

Two propagation gaps mean a tightening the operator sees on-screen is not
actually enforced:

1. **No forced reload for typed autonomy/permissions changes.** The TUI
   `/autonomy <preset>` and `/permissions add|remove` handlers write config and
   return a `Message`; they rely on the best-effort config-watcher to reload the
   running agent (~500ms), and if the watcher failed to initialize (it logs a
   `warn!` and sets itself to `None`), the change never reaches the live agent
   until restart. Contrast Shift+Tab, the `/autonomy` picker, and `/model`,
   which force a `TurnRequest::Reload` directly.
2. **Autonomy changes never reach running channel listeners.** Listeners restart
   only on a `channels_config` fingerprint diff. Autonomy state lives on
   `config.autonomy`, outside that fingerprint, so `/autonomy strict` tightens
   the local TUI agent but a running Telegram/Discord/Slack listener keeps
   executing under the old, looser level until independently restarted. The
   `/allow --persist` message already concedes listeners only "pick it up on
   their next restart".

## Current state

### Typed autonomy path relies on the watcher — `src/tui/commands/autonomy.rs:104-140`

`persist_preset_to_config` loads/applies/saves config and returns; it never
sends `TurnRequest::Reload`. `/permissions` (`src/tui/commands/permissions.rs`)
is the same. The watcher is best-effort (`src/tui/app.rs:8195-8202`: on error
`config_watcher = None` with a `warn!`).

### The forced-reload precedent — `src/tui/commands/mod.rs` `CommandResult::SetModel`

`SetModel` (and `apply_preset_to_config_and_reload`) force a `TurnRequest::Reload`
directly. Shift+Tab and the picker use the latter.

### The listener-restart fingerprint — `src/tui/app.rs:2186-2212, 2303, 2509`

`reload_config` calls `restart_channels()` only when `channels_fingerprint`
(over `config.channels_config`) differs. `config.autonomy` is not in that
fingerprint, so an autonomy-only change leaves `channels_changed == false` and
the listeners are not rebuilt. `permissions add allow-command` (writes
`config.autonomy.allowed_commands`) has the same gap; `add owner/tool/command`
do restart listeners because they mutate `channels_config`.

## The fix

### Step 1 — route typed autonomy/permissions through a forced reload

Give the `/autonomy <preset>` and `/permissions add|remove` handlers a
reload-carrying result (like `CommandResult::SetModel`) so the app forces a
`TurnRequest::Reload` after the write, instead of depending on the watcher.
Reuse the `apply_preset_to_config_and_reload` path the picker/Shift+Tab use, or
add a `CommandResult` variant the dispatcher turns into a reload.

### Step 2 — surface watcher-init failure

When the config watcher fails to initialize (`app.rs:8195`), surface it to the
user (a one-time system message) instead of a silent `warn!`, so an operator
knows typed changes may lag if they somehow still depend on it.

### Step 3 — include autonomy in the listener-restart trigger

Extend the restart-trigger so an autonomy change also refreshes running
listeners. Two options:

- **A (simple):** include `config.autonomy` in the fingerprint that gates
  `restart_channels()` (compute it over `channels_config` **and** `autonomy`), so
  a tightening rebuilds listeners with the new policy.
- **B (less churn):** push a policy-only refresh to listeners without a full
  reconnect (if the listener holds an `Arc<SecurityPolicy>`/`ApprovalManager`
  that can be swapped). This avoids reconnect churn but is more work.

Prefer A unless reconnect churn is a known problem; note the choice.

Also correct the `permissions.rs` module doc that claims "a live channel session
picks up the change" — it does not, until this plan.

## Files

- **In scope**: `src/tui/commands/autonomy.rs`, `src/tui/commands/permissions.rs`,
  `src/tui/commands/mod.rs` (the result variant), `src/tui/app.rs` (fingerprint +
  watcher-failure surfacing).
- **Out of scope**: the CLI daemon-reload announce (plan 212), session-allowlist
  invalidation (plan 207 — complementary), the config-API/gateway propagation
  (the gateway rebuilds its policy per request already).

## STOP conditions

- If `/autonomy`/`/permissions` already force a reload (drift), skip Step 1.
- If including `autonomy` in the fingerprint causes listeners to restart on
  unrelated autonomy-adjacent writes (e.g. a cosmetic field), scope the
  fingerprint to the enforced fields (`level`, `always_ask`, `auto_approve`,
  `allowed_commands`, `forbidden_paths`, `workspace_only`) and report.

## Done criteria

1. `cargo fmt`/`clippy` clean; `cargo build -p rantaiclaw --bin rantaiclaw`
   (binary crate — the TUI change won't show under `--lib` alone).
2. `cargo test -p rantaiclaw --lib` clean, plus a test asserting the
   restart-trigger fingerprint changes when an autonomy enforced field changes.
   The live function is `channels_fingerprint` (`src/tui/app.rs:7927`), which
   today hashes only `channels_config` — Step 3 extends it (or adds a sibling
   `restart_fingerprint`) to include the enforced autonomy fields. Name the test
   against whatever the final trigger function is called:

```rust
#[test]
fn autonomy_change_triggers_listener_restart() {
    let a = Config::default();
    let mut b = a.clone();
    b.autonomy.level = AutonomyLevel::ReadOnly;
    // channels_fingerprint (extended per Step 3) or the new restart_fingerprint:
    assert_ne!(restart_trigger_fingerprint(&a), restart_trigger_fingerprint(&b));
}
```

3. Behavioral (drive the TUI via tmux per the repo norm): `/autonomy strict`
   with the watcher disabled still tightens the live agent (a subsequent tool
   call is gated), and a running channel listener reflects the new level.

## Test plan

- Unit: the fingerprint test above (extract the fingerprint into a testable fn
  if it is inline).
- Behavioral: the repo's tmux TUI drive; confirm a typed tighten takes effect
  without a restart and reaches a listener. Document the drive in the PR.

## Risk & rollback

- **Risk**: MED — forcing reloads and restarting listeners on autonomy changes
  adds some reconnect churn (Option A); that is the cost of correct enforcement.
- **Rollback**: revert the touched TUI files; no schema/config/migration change.

## Maintenance note

Any config change that affects the enforced policy must reach both the local
agent (forced reload) and the running listeners (restart/refresh). The
fingerprint is the single gate for the latter — keep the enforced autonomy
fields in it.
