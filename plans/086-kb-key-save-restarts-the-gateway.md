# Plan 086: Stop a Knowledge Base key save from restarting the gateway

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/gateway/config_api.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P1
- **Effort**: XS
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Saving a Knowledge Base API key through the web console takes the whole agent
runtime down for several seconds. Operators see the console throw an error and
the gateway go offline; the natural reaction is to retry, which bounces the
service again.

The chain is fully traced:

1. `PUT /api/v1/config/knowledge` → `set_knowledge` (`src/gateway/config_api.rs:871`)
2. `set_knowledge` calls `schedule_daemon_reload()` at `config_api.rs:897`
3. `schedule_daemon_reload` (`config_api.rs:549`) spawns a detached task that
   sleeps 750 ms then calls `crate::channels::reload_managed_daemon`
4. `reload_managed_daemon` (`src/channels/mod.rs:2522`) runs
   `systemctl --user restart rantaiclaw.service` (`mod.rs:2649`)
5. `rantaiclaw daemon` is what hosts the gateway (`src/daemon/mod.rs:87`
   spawns `run_gateway`)

So the gateway restarts itself, taking channels, the scheduler and any
in-flight chat turn with it. The 750 ms delay is enough for the PUT response to
flush, which is why the write appears to succeed before everything drops.

This is not the intended use of that helper. Its own doc comment
(`config_api.rs:538-548`) says "After a **channel** config change" — a KB
credential is not a channel. And `set_secrets`, which writes the *main*
provider API key, deliberately does not call it (`config_api.rs:804-815`).

The reload is also redundant: `set_knowledge` already calls
`crate::kb::axi::clear_kb_ctx()` at `config_api.rs:896`, which drops the cached
`KbContext` so the next KB request rebuilds with the new credential in-process.

## Current state (verified at 2ca7e59)

`src/gateway/config_api.rs:888-903` (inside `set_knowledge`):

```rust
    persist_and_swap(&state, cfg).await?;
    // New credentials invalidate any cached KB embedding/extraction context.
    crate::kb::axi::clear_kb_ctx().await;
    schedule_daemon_reload();
    let cfg = state.config.lock().clone();
```

`schedule_daemon_reload` has three callers (`config_api.rs:632`, `:680`,
`:897`). The first two are `connect_telegram` / `disconnect_telegram`, where a
runtime bounce IS the point — those must keep it.

## Scope

**In scope**: remove the single call at `config_api.rs:897` and explain why in
a comment.

**Out of scope**: the channels-runtime question (a KB key change does not reach
a separately-running `rantaiclaw channels` process until it restarts). That is
pre-existing, applies equally to `set_secrets`, and is not worth a service
bounce. Note it in the PR body, do not fix it here.

## Git workflow

```bash
git switch -c fix/kb-key-save-restarts-gateway
```

## Steps

### Step 1: Drop the reload call

In `set_knowledge` (`src/gateway/config_api.rs`), delete the
`schedule_daemon_reload();` line and replace it with a comment recording the
decision:

```rust
    persist_and_swap(&state, cfg).await?;
    // New credentials invalidate any cached KB embedding/extraction context.
    // `clear_kb_ctx` is sufficient: the next KB request rebuilds the context
    // in-process with the new key. Do NOT call `schedule_daemon_reload()` here
    // — the managed service hosts this gateway (`daemon::run` spawns
    // `run_gateway`), so a restart would take the console offline mid-save.
    // Channel connects/disconnects still reload, because the channels
    // supervisor captures its channel set at startup.
    crate::kb::axi::clear_kb_ctx().await;
```

**Verify**: `cargo build --features kb` succeeds and
`grep -n 'schedule_daemon_reload' src/gateway/config_api.rs` shows exactly
three hits — the `fn` at 549 and the two telegram call sites.

### Step 2: Verification is manual, deliberately

**Do not add a unit test for this.** The behaviour being removed is "spawns a
task that runs `systemctl restart`". A unit test cannot observe that without a
process-control seam, and a test that greps this file's own source for the
helper name is brittle theatre — it breaks on a rename and passes on a
re-introduction through a wrapper.

The honest guard is the `NRestarts` check in the Test plan below: it observes
the actual system effect. Run it and paste the two values into the PR body.

If a real seam is wanted later (injecting `DaemonControl` into the gateway the
way `src/daemon/handoff.rs:26-34` already does for the daemon), that is a
separate refactor — not a prerequisite for deleting one line.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb config_api
```

Manual confirmation on a host with the managed service installed:

```bash
systemctl --user show rantaiclaw.service -p NRestarts   # note the value
# save a KB key through the console, or:
curl -X PUT localhost:9393/api/v1/config/knowledge \
  -H 'content-type: application/json' \
  -d '{"embedding_api_key":"sk-test-not-a-real-key"}'
sleep 3
systemctl --user show rantaiclaw.service -p NRestarts   # must be UNCHANGED
```

## Done criteria

- `schedule_daemon_reload` is called only from the two telegram handlers.
- `NRestarts` does not move across a KB key save, and both readings are in the
  PR body.
- The comment at the call site explains why the reload is absent, so the next
  reader does not "restore" it.

## STOP conditions

- The telegram handlers no longer call `schedule_daemon_reload` — someone else
  changed this file; re-read before editing.
- `clear_kb_ctx` is gone from `set_knowledge` — the in-process invalidation is
  load-bearing for this fix; stop and report.
