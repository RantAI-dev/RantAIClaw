# Plan 287: Give MCP servers a lifetime owned by the gateway, not by each chat request

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the row in `plans/280-production-readiness-handoff.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0dd4c03..HEAD -- src/mcp/ src/gateway/api_v1.rs src/gateway/mod.rs src/agent/agent.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P0 — BLOCKER (ledger W0-8)
- **Effort**: L
- **Risk**: MED — introduces process lifetime management to a path that has none
- **Depends on**: nothing, but see "sequencing" below
- **Category**: bug / architecture
- **Planned at**: commit `0dd4c03`, 2026-09-04

## Why this matters

The gateway builds a fresh `Agent` for every chat request. Agent construction discovers MCP
tools, and discovery spawns every configured server, sequentially, with a 30-second
per-request timeout. When the request's agent drops, `kill_on_drop` SIGKILLs the lot.

So each console turn pays process spawn plus package resolution plus handshake plus
`tools/list` for every server, then throws the result away. Stateful servers lose their
state every turn; one hung server stalls every turn for up to a minute per server. The
console is the gateway's primary client, so this *is* the MCP experience of the web UI.

## Current state (verified at `0dd4c03`)

```rust
// src/gateway/api_v1.rs:589 (sync chat) and :728 (SSE stream)
let mut agent = crate::agent::Agent::from_config_with_observer(&config, state.observer.clone())
```

```rust
// src/agent/agent.rs:507
let mcp_discovery = crate::mcp::discover::discover_mcp_tools(&config.mcp_servers).await;
```

```rust
// src/mcp/discover.rs:51-58 — sequential, one connect per server
pub async fn discover_mcp_tools(servers: &HashMap<String, McpServerConfig>) -> McpDiscovery {
    for (name, cfg) in servers {
        match McpClient::connect(name.clone(), &cfg.command, &cfg.args, &cfg.env).await {
```

```rust
// src/mcp/client.rs:77
.kill_on_drop(true);
```

`McpDiscovery` already holds `clients: HashMap<String, Arc<McpClient>>` "so the underlying
child processes stay alive for as long as the agent does" — the shape needed for pooling
exists; only its owner is wrong.

`AppState` (`src/gateway/mod.rs:462`) already carries shared, hot-reloadable state
(`config`, `config_fingerprint`, `provider`, `mem`, a `ToolsFactory`), so it is the right
home. Read the `ToolsFactory` doc comment there first: it explains why a *factory* was
chosen over a prebuilt registry when autonomy can change at runtime. The same reasoning
applies to config changes here.

## Sequencing

The MCP client has two defects that this plan makes *more* reachable by keeping servers
alive longer: undrained stderr deadlocks a server after ~64 KiB of logging
(`src/mcp/client.rs:76`), and the response reader discards replies whose id does not match,
so two concurrent calls to one server make the second time out (`:246-257`). Those are
ledger item W1-4. **Either land W1-4 first, or land it inside this PR.** Do not ship a
long-lived pool over a client that cannot survive being long-lived.

## Steps

1. **Decide the ownership boundary and write it down.** The pool belongs to whatever owns
   the gateway's lifetime, and must be rebuilt when `config.mcp_servers` changes (the
   gateway hot-reloads config). Sketch this in the PR description before coding: what
   creates the pool, what invalidates it, what happens to an in-flight tool call during
   invalidation.

2. **Hold discovery in `AppState`.** Add the pooled `McpDiscovery` (or a small wrapper that
   can be swapped on reload) to `AppState`. Populate it at gateway start.
   **Verify**: `cargo build -p rantaiclaw --lib` clean.

3. **Let agent construction accept pre-discovered tools.** Add a constructor seam beside
   `from_config_with_observer` that takes the already-discovered MCP tools instead of
   spawning. Leave the existing constructor's behaviour unchanged so the TUI and CLI paths
   are untouched by this PR.
   **Verify**: `rg -n 'discover_mcp_tools' src/` — the gateway request path no longer
   reaches it.

4. **Invalidate on config change.** Rebuild the pool when the reloaded config's
   `mcp_servers` differs. Old clients must be dropped only once no request holds them.
   **Verify**: a test that changes `mcp_servers` and asserts the pool was rebuilt.

5. **Prove it with a fake server.** The repo already drives a fake stdio MCP server in
   `tests/onboard_mcp_section.rs` for `validate_mcp_startup`; reuse that shape. Assert that
   two sequential chat requests spawn the server **once**, e.g. by having the fake append to
   a file on start and asserting one line.
   **Verify**: `cargo test --test onboard_mcp_section` and the new test pass.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- Scoped tests pass: `cargo test --lib mcp`, `cargo test --lib gateway`, plus the new
  spawn-once integration test.
- Two chat requests against a configured MCP server spawn one process, not two.

## STOP conditions

- W1-4 (single-reader client, drained stderr) is not landing with or before this → STOP.
- Sharing clients across requests would let one request's cancellation kill another's tool
  call → STOP and report; that needs a design decision on per-request scoping.
- The change starts touching channel or cron tool assembly → STOP. MCP reach for channels
  and cron is issue #283, a separate decision (ledger W2-1).

## Test plan

One integration test with a fake stdio server asserting single-spawn across two requests;
one unit test for config-change invalidation. Do not assert on timing.

## Maintenance note

This is the first place the gateway owns a child-process lifetime. Whatever invalidation
rule is chosen must be restated in the module doc of `src/mcp/discover.rs`, whose current
comment still says clients are pinned "for as long as the agent does".

## Rollback

Revert restores per-request spawning, which is slow but functional — no data or schema
change. Keep the W1-4 client fixes in a separate commit so they survive a rollback of the
pooling change.
