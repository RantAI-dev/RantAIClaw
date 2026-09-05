# Plan 239: Stop setup provisioners overwriting config they did not prompt for

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/onboard/section/channels.rs src/onboard/wizard.rs src/onboard/provision/runtime_surfaces/tunnel.rs src/onboard/provision/runtime_surfaces/browser.rs src/onboard/provision/runtime_surfaces/gateway.rs src/onboard/provision/channels/lark.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH (re-running setup currently destroys credentials; the fix changes "Done" semantics from "full set" to "edits")
- **Depends on**: none
- **Category**: bug (data loss)
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Several setup surfaces replace a whole config struct from defaults, silently wiping fields they never prompt for. Re-running `setup channels` to add Discord deletes the Telegram token, every `approval_owners` entry, and the guest capability ceiling — after which every approval-gated tool call auto-denies. The gateway provisioner was already fixed to mutate field-by-field (see the exemplar below); its siblings were not. This plan propagates that fix.

Findings covered: C1 (`setup channels` wipe), C6 (gateway `0.0.0.0` doesn't set `allow_public_bind`; "disable" writes nothing), C7 (tunnel whole-replace + provider-without-cred), C8 (browser discards answers + wipes allowlist), A10 (Lark token prompt not masked), H2 (runtime_surfaces provisioners untested).

## Current state

- **Exemplar (already correct)** — `src/onboard/provision/runtime_surfaces/gateway.rs:160-187` mutates only prompted fields (`config.gateway.port/host/require_pairing`) and its comment enumerates exactly the wipe hazards. Its test seam `run_with` + `setup_gateway_preserves_everything_it_does_not_prompt_for` (`gateway.rs:196-261`) is the pattern to copy for every other surface.
- `src/onboard/section/channels.rs:39`: `ctx.config.channels_config = wizard::setup_channels()?;` — whole-struct replace. `approval_owners`, `guest_allowed_tools`, `guest_allowed_commands` live INSIDE `ChannelsConfig` (`schema.rs:2821-2841`), so they are destroyed. `wizard::setup_channels()` (`wizard.rs:3424`) starts from `ChannelsConfig::default()` and never sees the existing config. `run_channels_repair_wizard` (`wizard.rs:485`) has the same shape.
- `src/onboard/provision/runtime_surfaces/tunnel.rs:91-104,284`: builds a fresh `TunnelConfig` with all providers `None` and assigns `config.tunnel = tunnel_cfg;` unconditionally; selecting a provider with an empty token persists `provider="cloudflare", cloudflare:None`.
- `src/onboard/provision/runtime_surfaces/browser.rs:159-188`: prompts viewport w/h/quality then discards (`let _w`/`_h`/`_q`), hardcodes `allowed_domains: vec![]`, assigns `config.browser = browser_cfg` wholesale. `src/tools/browser.rs:420` treats an empty `allowed_domains` on an enabled browser as a hard error.
- `src/onboard/provision/runtime_surfaces/gateway.rs:174-176`: writes `port/host/require_pairing` but NOT `allow_public_bind`; the `0.0.0.0` case leaves a config the gateway refuses to start from (`src/gateway/mod.rs:902`). The enable/disable prompt writes nothing (no `enabled` field exists on `GatewayConfig`).
- `src/onboard/provision/channels/lark.rs:204`: `ProvisionEvent::Prompt { id:"verification_token", …, secret:false }` — the only credential prompt with `secret:false`; siblings use `secret:true`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib onboard::provision` | pass |
| Test | `cargo test --lib onboard::section` | pass |
| Test | `cargo test --lib onboard::wizard` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/onboard/section/channels.rs`, `src/onboard/wizard.rs` (`setup_channels`, `run_channels_repair_wizard` — take the existing config, mutate in place)
- `src/onboard/provision/runtime_surfaces/tunnel.rs`, `browser.rs`, `gateway.rs`
- `src/onboard/provision/channels/lark.rs` (flip `secret:true`)
- New `#[cfg(test)]` tests in each modified provisioner file

**Out of scope**:
- The other `runtime_surfaces` provisioners (memory, runtime, proxy, …) — their whole-replace bug is real but the FIX pattern is identical; note them in Maintenance for a follow-up plan, do not fix all 12 here (keeps this PR reviewable). This plan fixes the ones with confirmed credential/security loss: channels, tunnel, browser, gateway.
- The TUI channel path (`tui/app.rs:4160`) — already mutates per-field correctly.

## Git workflow

- Branch: `fix/setup-preserve-unprompted-config`
- Message e.g. `fix(onboard): preserve unprompted config across setup provisioner re-runs`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: `setup_channels` mutates the existing config

Change `setup_channels()` to `setup_channels(existing: ChannelsConfig) -> Result<ChannelsConfig>` seeded from the caller's `ctx.config.channels_config`, mutating only the channel the operator touches. Add an explicit "Disconnect <platform>" menu action so removal is still possible. Update `section/channels.rs:39` and `run_channels_repair_wizard` to pass the existing config. The "✅ connected" menu markers must read the seeded config, not a fresh default.

**Verify**: `cargo test --lib onboard::section onboard::wizard` → pass; Test-plan `channels_preserves_approval_owners` passes.

### Step 2: Tunnel provisioner mutates field-by-field

In `tunnel.rs`, replace the fresh-`TunnelConfig` + `config.tunnel = ...` with per-field mutation (mirror `gateway.rs:160-187`). An empty credential keeps the existing one, else falls back to `provider="none"` (as the CLI wizard `wizard.rs:4970` already does). Never persist `provider="cloudflare"` with `cloudflare:None`.

**Verify**: Test-plan `tunnel_preserves_existing_token` passes.

### Step 3: Browser provisioner applies its answers and preserves the allowlist

In `browser.rs`, parse `_w`/`_h`/`_q` into `computer_use` (use the `numeric::prompt_number` helper referenced by `gateway.rs`), and mutate `config.browser` field-by-field, preserving `allowed_domains` and `session_name`.

**Verify**: Test-plan `browser_preserves_allowed_domains` passes.

### Step 4: Gateway `0.0.0.0` opt-in + drop the meaningless disable prompt

In `gateway.rs`, after the `0.0.0.0` warning add an explicit "Bind publicly anyway?" confirm that sets `config.gateway.allow_public_bind = true` when confirmed. Remove the enable/disable prompt (it writes nothing) OR wire it to a real field if one is added — prefer removal. Extend the existing test to assert a `0.0.0.0` + confirm yields a startup-viable config.

**Verify**: Test-plan `gateway_public_bind_opt_in_is_startup_viable` passes.

### Step 5: Mask the Lark token prompt

In `lark.rs:204`, set `secret: true` on the `verification_token` prompt.

**Verify**: `grep -n "verification_token" src/onboard/provision/channels/lark.rs` shows `secret: true`.

## Test plan

Copy the `run_with` seam + preservation-test pattern from `gateway.rs:196-261` into each modified file:
- `channels_preserves_approval_owners` — pre-seed `approval_owners`; run a channels edit touching only Discord; assert `approval_owners` survives and the Telegram token survives.
- `tunnel_preserves_existing_token` — pre-seed a Cloudflare token; run with an empty token answer; assert the token survives (or provider falls to "none", never a provider-with-None-cred).
- `browser_preserves_allowed_domains` — pre-seed `allowed_domains`; run; assert it survives and viewport answers are applied.
- `gateway_public_bind_opt_in_is_startup_viable` — `0.0.0.0` + confirm → `allow_public_bind=true`.
- Verification: `cargo test --lib onboard::provision onboard::section onboard::wizard` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped onboard tests pass with the new preservation tests
- [ ] `grep -n "config.tunnel = " src/onboard/provision/runtime_surfaces/tunnel.rs` shows per-field mutation, not a whole-struct assign
- [ ] `grep -n "let _w\|let _h\|let _q" src/onboard/provision/runtime_surfaces/browser.rs` returns nothing (answers are used)
- [ ] `grep -n "verification_token" src/onboard/provision/channels/lark.rs` shows `secret: true`
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `setup_channels` is called from more than the two known sites (drift) — enumerate callers first; report if a third exists.
- Changing `setup_channels`'s signature cascades into unrelated modules — report.
- The `numeric::prompt_number` helper isn't found for Step 3 — parse the strings inline with range validation instead, note it.

## Maintenance notes

- Reviewer: each modified provisioner must have a preservation test proving a pre-seeded unprompted field survives a re-run.
- Deferred follow-up (own plan): the remaining `runtime_surfaces` provisioners (memory, runtime, proxy, model_routes, embedding_routes, hardware, web_search, multimodal, composio, secrets, agents) have the same whole-replace shape — `memory.rs` and `runtime.rs` already discard tuned values. Apply the same pattern + tests there.
- This is the source of finding H2 (12 untested provisioners); the tests added here start closing it.
