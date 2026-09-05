# Plan 282: Stop the Telegram bot token reaching logs on send errors

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the row in `plans/280-production-readiness-handoff.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0dd4c03..HEAD -- src/channels/telegram.rs src/channels/dispatch.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P0 — BLOCKER (ledger W0-3)
- **Effort**: S
- **Risk**: LOW (adds redaction; no behaviour change on the success path)
- **Depends on**: nothing
- **Category**: security
- **Planned at**: commit `0dd4c03`, 2026-09-04

## Why this matters

Every Telegram API URL embeds the bot token, and roughly eighteen send-path calls
propagate the raw `reqwest` error with `?`. `reqwest`'s `Display` appends `" for url (…)"`,
so the token travels with the error into `tracing::error!` at the dispatch layer. A single
transient network fault while replying writes a live bot token into journald at ERROR.

Telegram is the flagship channel, the only one verified live, and the one the console can
configure — so this is the most reachable credential leak in the product.

## Current state (verified at `0dd4c03`)

The token is in the URL:

```rust
// src/channels/telegram.rs:591-593
fn api_url(&self, method: &str) -> String {
    format!("{}/bot{}/{method}", self.api_base, self.bot_token)
}
```

A correct redactor already exists and is documented for exactly this hazard:

```rust
// src/channels/telegram.rs:600-608
fn scrub_token(&self, msg: impl std::fmt::Display) -> String {
    let s = msg.to_string();
    if self.bot_token.is_empty() { s } else { s.replace(self.bot_token.as_str(), "<redacted>") }
}
```

But it is applied at only four sites — `:641` (fetch_bot_username), `:2318` and `:2329`
(poll loop), `:2444` (health check). The send path does not use it:

```rust
// src/channels/telegram.rs:1404-1408 — one of ~18 identical shapes
let resp = self
    .http_client()
    .post(self.api_url("sendMessage"))
    .json(&html_body)
    .send()
    .await?;                     // <-- raw reqwest error escapes, URL and token with it
```

And the sink logs it verbatim:

```rust
// src/channels/dispatch.rs:706-709
Err(e) => {
    tracing::error!(channel = %channel.name(), "failed to reply: {e}");
```

Other `dispatch.rs` sites with the same shape: `:486`, `:513`, `:637`, `:688`, `:743`, `:933`.

## Steps

1. **Find every escaping site.** In `src/channels/telegram.rs`:
   ```bash
   rg -n '\.send\(\)\s*$' -A2 src/channels/telegram.rs | rg -n 'await\?'
   ```
   Expect roughly eighteen: around `:1403`, `:1440`, `:1480`, `:1587`, `:1626`, `:1670`,
   `:1709`, `:1753`, `:1797`, `:1841`, `:1878`, `:1915`, `:2026`, `:2033`, `:2093`,
   `:2179`, `:2198`, `:2230`. Record the real list; do not trust these numbers blindly.

2. **Redact at the boundary, not at each call site.** Prefer one helper so a future call
   site cannot forget it — for example a private
   `async fn send_scrubbed(&self, rb: reqwest::RequestBuilder) -> anyhow::Result<Response>`
   that awaits the send and maps any error through `self.scrub_token(e)`. Route the sites
   from step 1 through it. Keep the success path byte-identical.
   **Verify**: `cargo build -p rantaiclaw --lib` is clean, and
   `rg -n 'await\?;' src/channels/telegram.rs` no longer matches a bare send.

3. **Cover the error path that produced the leak.** Add a test that drives a real send
   against a closed port and asserts the token is absent from the error text. A bound-then-
   dropped `TcpListener` gives a guaranteed-refused address without network access.
   ```rust
   // asserts the negative that matters: the token must not appear anywhere
   assert!(!err.to_string().contains(TOKEN), "token leaked: {err}");
   ```
   **Verify**: `cargo test --lib channels::telegram` passes and the new test fails if you
   temporarily revert step 2 (prove the test is not vacuous — this repo has shipped tests
   whose input could never make the assertion fail).

4. **Check the neighbours.** `rg -n 'tracing::(error|warn|info)!.*\{e\}' src/channels/telegram.rs`
   — any remaining site must pass through `scrub_token`.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib channels::telegram` passes, including the new closed-port test.
- Reverting step 2 makes the new test fail.

## STOP conditions

- `scrub_token` no longer exists or changed shape → STOP, report drift.
- More than `src/channels/telegram.rs` (plus its test module) needs editing → STOP; the
  `dispatch.rs` logging line is deliberately out of scope, the fix belongs at the source.

## Test plan

One new unit test in the `telegram.rs` test module, following the existing pattern at
`:2504-2512` which already models the scrubbed-error string shape.

## Maintenance note

Any future Telegram API call must go through the step-2 helper. If a reviewer sees a bare
`.send().await?` in this file again, that is the regression.

## Rollback

Single commit touching one file plus a test. `git revert` restores prior behaviour; there
is no data or schema change.
