# Plan 283: A failed tunnel must not leave the gateway serving on a public bind

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the row in `plans/280-production-readiness-handoff.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0dd4c03..HEAD -- src/gateway/mod.rs src/tunnel/`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P0 — BLOCKER (ledger W0-4)
- **Effort**: S
- **Risk**: LOW — the change only *tightens*; it can turn a previously-starting gateway
  into a refusing one, which is the intended outcome
- **Depends on**: nothing
- **Category**: security (exposure boundary)
- **Planned at**: commit `0dd4c03`, 2026-09-04

## Why this matters

`CLAUDE.md` §3.6 draws a hard line: local capability may default open, but *exposure*
surfaces stay deny-by-default. The public-bind guard honours that only while a tunnel is
configured and healthy. Configure any tunnel provider, bind `0.0.0.0`, and let the tunnel
fail — a bad token, a missing binary — and the gateway prints a warning and serves the
control plane to the internet anyway, without `allow_public_bind` ever being set.

The guard's own premise is that a tunnel makes a public bind unnecessary, which is true:
every provider proxies `localhost:<port>`. So a tunnel that did not start is precisely the
case where a public bind is least acceptable.

## Current state (verified at `0dd4c03`)

```rust
// src/gateway/mod.rs:933-935
// ── Security: refuse public bind without tunnel or explicit opt-in ──
if is_public_bind(host) && config.tunnel.provider == "none" && !config.gateway.allow_public_bind
{
    // Fatal: no amount of retrying makes an exposed bind acceptable.
    return Err(anyhow::Error::new(GatewayStartupFatal(format!(
```

The escape hatch, ~60 lines later:

```rust
// src/gateway/mod.rs:996-1010
if let Some(ref tun) = tunnel {
    println!("🔗 Starting {} tunnel...", tun.name());
    match tun.start(host, actual_port).await {
        Ok(url) => { println!("🌐 Tunnel active: {url}"); tunnel_url = Some(url); }
        Err(e) => {
            println!("⚠️  Tunnel failed to start: {e}");
            println!("   Falling back to local-only mode.");   // <-- it does not
        }
    }
}
```

Execution continues to `axum::serve` on the same public listener at `:1058`. Providers
proxy loopback — see `src/tunnel/cloudflare.rs:39` — so the public bind buys nothing.

## Steps

1. **Make the guard depend only on the operator's explicit opt-in.** Drop
   `config.tunnel.provider == "none"` from the condition at `:934`, so a public bind
   requires `allow_public_bind = true` regardless of tunnel configuration.
   **Verify**: `cargo build -p rantaiclaw --lib` clean.

2. **Make a failed tunnel fatal when the bind is public.** In the `Err(e)` arm at
   `:1004-1009`, if `is_public_bind(host) && !config.gateway.allow_public_bind`, return
   `GatewayStartupFatal` with a message naming the tunnel error and the two ways out
   (bind loopback, or set `allow_public_bind`). Keep the current warn-and-continue for a
   loopback bind, where a dead tunnel is a convenience failure, not an exposure one.
   **Verify**: the daemon supervisor treats `GatewayStartupFatal` as fatal rather than
   retrying — confirm at `src/daemon/mod.rs` before relying on it.

3. **Cover both directions.** Add tests: (a) public host + tunnel configured + tunnel start
   fails + `allow_public_bind = false` → startup returns `GatewayStartupFatal`;
   (b) loopback host + same failure → gateway still starts.
   **Verify**: `cargo test --lib gateway` passes; test (a) fails if step 2 is reverted.

4. **Say so where operators read.** `docs/reference/config.md` has no `[tunnel]` section at
   all today. Add one that states the bind rule plainly. One paragraph, no roadmap prose.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib gateway` passes with both new tests.
- `rg -n 'tunnel.provider == "none"' src/gateway/mod.rs` returns nothing.

## STOP conditions

- Removing the tunnel clause makes an existing test fail in a way that suggests operators
  rely on tunnel-implies-public-bind → STOP and report; that is a product decision.
- The change would require touching `src/tunnel/*` internals → STOP; this plan is about the
  bind decision only.

## Test plan

Two tests in the `gateway` module. Drive `run_gateway`'s startup path with a tunnel stub
whose `start` returns `Err`; assert the error type, not the message text.

## Maintenance note

Any future early-return between the guard at `:934` and `axum::serve` at `:1058` must
re-check the bind decision. The guard and the serve call are 120 lines apart, which is how
this gap opened.

## Rollback

One commit, one file plus tests and a doc paragraph. Reverting restores the prior
permissive behaviour.
