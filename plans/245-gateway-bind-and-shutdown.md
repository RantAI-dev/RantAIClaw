# Plan 245: Accept loopback host spellings and give the gateway command a clean shutdown

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/gateway/mod.rs src/main.rs src/security/pairing.rs src/daemon/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: S–M
- **Risk**: LOW (host resolution + wiring an existing shutdown token)
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

1. **`--host localhost` crashes the gateway** (D2). The security gate `is_public_bind` whitelists `127.0.0.1`, `localhost`, `::1`, `[::1]`, `0:0:0:0:0:0:0:1` as loopback (permitted without `allow_public_bind`), but the bind then does `format!("{host}:{port}").parse::<SocketAddr>()`, which only accepts a numeric IPv4 or bracketed IPv6. So `localhost:8080`, `::1:8080`, and the long IPv6 form crash at startup — `rantaiclaw gateway --host localhost`, the most natural "loopback only", never starts.
2. **The standalone `gateway` command has no shutdown path** (D6). It passes a `CancellationToken::new()` that nothing cancels, so `with_graceful_shutdown` waits on a future that never resolves — SIGINT/SIGTERM severs in-flight requests. The daemon path already installs a SIGINT+SIGTERM handler driving the same token.

## Current state

- `src/gateway/mod.rs` (`run_gateway`, `:900-916`):
  ```rust
  if is_public_bind(host) && config.tunnel.provider == "none" && !config.gateway.allow_public_bind {
      anyhow::bail!("🛑 Refusing to bind to {host} ...");   // :904
  }
  let addr: SocketAddr = format!("{host}:{port}").parse()?;  // :911 — only numeric IP parses
  let listener = tokio::net::TcpListener::bind(addr).await?;
  ```
- `src/security/pairing.rs` `is_public_bind` (`:367-372`) — the loopback whitelist to honor: `"127.0.0.1" | "localhost" | "::1" | "[::1]" | "0:0:0:0:0:0:0:1"`.
- `src/main.rs` Gateway handler (`:1858-1876`):
  ```rust
  gateway::run_gateway(&host, port, config,
      tokio_util::sync::CancellationToken::new()).await   // :1869-1874 — never cancelled
  ```
  Comment at `:1866-1868` acknowledges the drain is a follow-up.
- `src/daemon/mod.rs` installs a SIGINT+SIGTERM handler driving the token (~`:149-222`); its `shutdown_signal` async fn has no daemon-specific state and can be shared.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib gateway` | pass |
| Test | `cargo test --lib security::pairing` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/gateway/mod.rs` (host resolution)
- `src/main.rs` (Gateway handler drives a real token)
- `src/daemon/mod.rs` — only to make `shutdown_signal` shareable (`pub(crate)`)

**Out of scope**:
- The daemon supervisor lifecycle (plan 246).
- `allow_public_bind` semantics — unchanged; the fix keeps `is_public_bind` on the ORIGINAL host string.

## Git workflow

- Branch: `fix/gateway-bind-and-shutdown`
- Message e.g. `fix(gateway): accept loopback host spellings and drain on signal for the gateway command`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Resolve the host instead of parsing it as a numeric IP

In `run_gateway` (`gateway/mod.rs:911`), replace `format!("{host}:{port}").parse::<SocketAddr>()?` with a resolver that handles hostnames and unbracketed IPv6: normalize the known loopback aliases (`localhost`→`127.0.0.1`, `::1`/`0:0:...:1`→`[::1]`) before parsing, or use `(host, port).to_socket_addrs()` and take the first. Keep the `is_public_bind(host)` check on the ORIGINAL string (`:902`) so the security gate is unchanged.

**Verify**: Test-plan `every_loopback_spelling_resolves` passes.

### Step 2: Drive a real shutdown token from the `gateway` command

Make `daemon::shutdown_signal` (or the equivalent SIGINT+SIGTERM waiter) `pub(crate)`. In `main.rs`'s Gateway handler (`:1869`), create a `CancellationToken`, spawn a task that cancels it when the signal fires, and pass that token to `run_gateway` — mirroring `daemon::run`.

**Verify**: `cargo test --lib gateway` compiles; manual note: `rantaiclaw gateway` then Ctrl-C should drain (not tested in unit tests).

## Test plan

- `gateway` (or `security::pairing` for the resolver): `every_loopback_spelling_resolves` — for each spelling in `is_public_bind`'s loopback list, assert the resolver yields a valid `SocketAddr` (table-driven).
- `public_host_still_gated` — a public IP without `allow_public_bind`/tunnel still bails (regression that the security gate is intact).
- Verification: `cargo test --lib gateway security::pairing` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `grep -n 'parse()?' src/gateway/mod.rs` near the bind shows the resolver, not a bare `SocketAddr` parse of `{host}:{port}`
- [ ] scoped tests pass incl. `every_loopback_spelling_resolves`
- [ ] `grep -n "CancellationToken::new()" src/main.rs` no longer shows an un-cancelled token in the Gateway arm
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `is_public_bind`'s loopback list changed (drift) — re-derive the table from the live code.
- Making `shutdown_signal` shareable requires a larger daemon refactor — implement Step 1 alone, defer Step 2, report.
- `to_socket_addrs` does a DNS lookup that blocks — for loopback names that's fine, but if a non-loopback hostname would block the async runtime, wrap in `spawn_blocking` or restrict resolution to the known aliases.

## Maintenance notes

- Reviewer: confirm the security gate still fires on a public bind (the resolver must not accidentally widen what binds publicly) and that every loopback spelling now resolves.
- Interacts with plan 246 (supervisor) — a fatal bind error there should propagate; this plan makes `--host localhost` no longer BE a fatal error.
