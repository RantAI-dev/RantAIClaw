# Plan 176: Stop leaking the Telegram bot token through reqwest error logs

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/channels/telegram.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The Telegram bot **token** is embedded in every request URL:
`api_url` builds `format!("{}/bot{}/{method}", self.api_base, self.bot_token)`
(`src/channels/telegram.rs:592`). When a `reqwest` transport error is logged as
`{e}`, the token leaks — because `Display for reqwest::Error` **always appends**
`" for url (<url>)"`, and that URL contains `/bot<TOKEN>/<method>`, even though
the format string no longer names `{url}`. Four live sites log an unwrapped
`reqwest::Error`:

- poll-loop transport error, WARN — `src/channels/telegram.rs:2304`
  (`tracing::warn!("Telegram poll error: {e}")`; URL built at 2288, request 2300)
- `getUpdates` decode error, WARN — `src/channels/telegram.rs:2315`
  (`tracing::warn!("Telegram parse error: {e}")`)
- `fetch_bot_username` error surfaced via `get_bot_username`, WARN —
  `src/channels/telegram.rs:627` (`tracing::warn!("Failed to fetch bot username: {e}")`;
  the underlying `?` at 596 propagates the `getMe` reqwest error)
- health-check `getMe` error, DEBUG — `src/channels/telegram.rs:2430`
  (`tracing::debug!("Telegram health check failed: {e}")`)

This is a live secret-in-logs leak (bot TOKEN credential), violating CLAUDE.md
§3.6 "never log secrets". **This re-confirms a 2026-07-22 finding that is still
live.** `src/gateway/config_api.rs:637` is genuinely safe (its `e` is an
`anyhow::Error` whose `Display` prints only the outer context, and the comment
there asserts the token is absent) — **do not touch it.** This plan routes the
four telegram sites through a formatter that strips the token from any URL the
error carries.

## Current state

Files and roles:

- `src/channels/telegram.rs:591-593` — `api_url` embeds the token:
  ```rust
  fn api_url(&self, method: &str) -> String {
      format!("{}/bot{}/{method}", self.api_base, self.bot_token)
  }
  ```
- `src/channels/telegram.rs:2300-2308` — poll request + WARN:
  ```rust
  result = self.http_client().post(&url).json(&body).send() => {
      match result {
          Ok(r) => r,
          Err(e) => {
              tracing::warn!("Telegram poll error: {e}");   // <-- 2304
  ```
- `src/channels/telegram.rs:2312-2318` — `getUpdates` decode + WARN
  (`tracing::warn!("Telegram parse error: {e}")` at 2315).
- `src/channels/telegram.rs:595-610` / `612-631` — `fetch_bot_username`
  (`?`-propagates the `getMe` reqwest error at 596) → `get_bot_username` logs it
  at 627.
- `src/channels/telegram.rs:2419-2438` — `health_check`; DEBUG at 2430.

`reqwest` version: `Cargo.lock` contains **two** reqwest entries (0.12.28 and
0.13.3). The `Display`-appends-`" for url (…)"` behavior holds for both, so the
leak and the fix apply regardless of which the telegram channel resolves to.
(This is a minor drift vs the original note that said "0.12.28"; it does not
change the fix.)

Repo conventions:
- Existing redaction helper: `src/security/mod.rs:55 redact(value)`. For this
  fix you need a **URL path** redactor, not a value redactor — the token sits in
  the URL path segment after `/bot`.
- Prefer the smallest local helper on `TelegramChannel` (or a free fn in the
  module) over pulling in a new dependency.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests   | `cargo test --lib channels` | all pass |
| Drift   | `git diff --stat 2aefb9f..HEAD -- src/channels/telegram.rs` | only your changes |

Do **not** run a bare `cargo test` (disk-constrained). Scope with `--lib`.

## Scope

**In scope**:
- `src/channels/telegram.rs` — a token-safe reqwest-error formatter and its use
  at the four sites (2304, 2315, 627, 2430)

**Out of scope**:
- `src/gateway/config_api.rs:637` — genuinely safe (anyhow outer-context only);
  changing it is churn.
- Any change to `api_url` itself / how requests are built — the token *must*
  stay in the URL for the API to work; the fix is at the **logging** boundary.
- Rotating the token in code — that is an operator action (see REMEDIATION).

## Git workflow

- Branch: `advisor/176-telegram-token-log-redaction`
- Conventional commits (e.g. `fix(telegram): redact bot token from error logs`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a token-safe reqwest-error formatter

Add a helper that formats a `reqwest::Error` without its URL secret. Two robust
options — pick one:

- (a) Use `reqwest::Error::url()` to detect an attached URL and render only its
  **host + a redacted path** (replace the `/bot<token>/<method>` segment with
  `/bot<redacted>/<method>`), plus the error's own message. Because the token is
  a path segment, take the URL's path, and if it starts with `/bot`, keep only
  up to `/bot` and append `<redacted>/…`.
- (b) Simpler and dependency-free: format the error to a `String`, then replace
  the concrete token substring `self.bot_token` with `<redacted>` before
  logging. This guarantees the token is gone regardless of how reqwest phrased
  the message. Prefer (b) for robustness — it cannot be defeated by a reqwest
  Display change, and the channel already holds `self.bot_token`.

Recommended (b), as a method on `TelegramChannel`:
```rust
/// Format an error for logging with the bot token scrubbed. reqwest's
/// `Display` appends `" for url (…)"` which contains `/bot<token>/…`, so any
/// raw `{e}` on a telegram request leaks the token. Replace the token literal.
fn scrub_token(&self, msg: impl std::fmt::Display) -> String {
    let s = msg.to_string();
    if self.bot_token.is_empty() {
        s
    } else {
        s.replace(self.bot_token.as_str(), "<redacted>")
    }
}
```

**Verify**: `cargo build` compiles; `cargo clippy --all-targets -- -D warnings`
→ exit 0.

### Step 2: Route the four sites through the formatter

Replace `{e}` with the scrubbed form at each site:

1. `src/channels/telegram.rs:2304` →
   `tracing::warn!("Telegram poll error: {}", self.scrub_token(&e));`
2. `src/channels/telegram.rs:2315` →
   `tracing::warn!("Telegram parse error: {}", self.scrub_token(&e));`
3. `src/channels/telegram.rs:627` →
   `tracing::warn!("Failed to fetch bot username: {}", self.scrub_token(&e));`
4. `src/channels/telegram.rs:2430` →
   `tracing::debug!("Telegram health check failed: {}", self.scrub_token(&e));`

For (3), `e` at 627 is the error returned by `fetch_bot_username`; it is an
`anyhow::Error` wrapping the reqwest error, whose `Display` chain can still
include the reqwest `" for url (…)"` text — scrubbing the token literal covers
it. Confirm `self` is in scope at each site (all four are methods on
`TelegramChannel` / `&self`).

**Verify**: `grep -n "poll error: {e}\|parse error: {e}\|bot username: {e}\|health check failed: {e}"
src/channels/telegram.rs` → **no matches** (all four now use `scrub_token`).
`cargo test --lib channels` → pass.

### Step 3: Regression test

Add a unit test in `src/channels/telegram.rs`'s test module asserting that
`scrub_token` removes the token: construct a `TelegramChannel` (or call
`scrub_token` on an instance) with a known non-secret placeholder token value
(e.g. `"test_token_abc123"` — a neutral, non-real placeholder, never a real
credential), format a string that embeds it plus `/bot<token>/getUpdates`, and
assert the output contains neither the token nor the literal `/bot` + token. Use
a project-neutral placeholder per CLAUDE.md §9.1 — do **not** use a
real-looking Telegram token.

**Verify**: `cargo test --lib channels` → the new test passes.

## Test plan

- New test: `scrub_token` strips the configured token from an error-shaped
  string, including from a `/bot<token>/method` URL fragment; and is a no-op when
  the token is empty.
- Verification: `cargo test --lib channels` → all pass, including the new test.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels` passes; the `scrub_token` test exists and passes
- [ ] `grep -n ": {e}" src/channels/telegram.rs` shows no remaining raw-error log
      of a reqwest error on a `/bot<token>` request (the four sites are converted)
- [ ] `src/gateway/config_api.rs:637` is unchanged
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report if:

- The "Current state" excerpts do not match live code (drift since 2aefb9f) —
  e.g. line numbers moved or the sites were already converted.
- `self.bot_token` is not accessible from a site (e.g. a helper without `&self`)
  — report so the formatter's signature can be adjusted.
- A verification fails twice after a reasonable fix attempt.

## REMEDIATION (must be in the PR description)

Any log written before this fix has already burned the token. The PR must
instruct the operator to **rotate the Telegram bot token via BotFather**
(`/revoke` then `/token`) and update the stored credential — scrubbing future
logs does not un-leak past ones.

## Maintenance notes

- A reviewer should scrutinize: that **all** telegram request-error logs go
  through `scrub_token` (grep for `{e}` in this file), and that no new
  `tracing::*` on a telegram request was added that bypasses it.
- This is a re-confirmation of a 2026-07-22 finding that was still live at
  2aefb9f — note that lineage in the PR so it is not dismissed as already fixed.
- Deferred: a broader audit of other channels that embed a secret in a URL
  (Discord/Slack webhooks) — out of scope here; telegram is the confirmed leak.
