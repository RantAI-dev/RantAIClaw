# Plan 249: Recover from a stale auth lock and harden the OAuth loopback

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/auth/profiles.rs src/auth/openai_oauth.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (auth path; stale-lock detection must not break a genuinely concurrent holder; OAuth state verification is a security property)
- **Depends on**: none
- **Category**: bug / security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

1. **Orphaned auth lock bricks all auth** (D7). `acquire_lock` creates `auth-profiles.lock` with `create_new(true)` and, on `AlreadyExists`, sleeps up to 10s then bails. The only removal is a `Drop` guard, which does NOT run on SIGKILL/OOM/`panic=abort`. So one hard kill during any auth write orphans the lock forever — afterwards every `load()`, `auth login`, `auth list`, and every provider using an auth profile blocks 10s then fails, with no recovery hint. The PID is written into the file but never read back.
2. **OAuth loopback accepts one connection and can return the path as the code** (E9). `receive_loopback_code` accepts exactly one connection; a path with no `?` makes `parse_code_from_redirect` skip state verification entirely and return the raw path string as the "code". So a stray request (browser preconnect, favicon fetch, scanner) before the real callback consumes the single `accept()`, the flow "succeeds" into a token exchange using e.g. `/favicon.ico` as the code, and the CSRF `state` is silently unverified.
3. **PKCE test doesn't bind the challenge** (H4). The existing test asserts only lengths, not `challenge == BASE64URL(SHA256(verifier))` — the entire security property.

## Current state

- `src/auth/profiles.rs:454-521` — `acquire_lock` writes the PID into the lock file (`:471`) but never reads it back; on `AlreadyExists` it sleeps in 50ms steps to `LOCK_TIMEOUT_MS` (10s, `:17`) then `bail!`. `AuthProfileLockGuard::drop` (`:517-521`) is the only removal.
- `src/auth/openai_oauth.rs`:
  ```rust
  pub async fn receive_loopback_code(expected_state: &str, timeout: Duration) -> Result<String> {
      let listener = TcpListener::bind("127.0.0.1:1455").await?;        // :243
      let accepted = timeout(timeout, listener.accept()).await??;       // :247 — ONE accept
      let (mut stream, _) = accepted;
      let bytes_read = stream.read(&mut buffer).await?;                 // :254 — ONE read
      let path = first_line.split_whitespace().nth(1)...;              // :265
      let code = parse_code_from_redirect(path, Some(expected_state))?; // :270
      ...
  }
  pub fn parse_code_from_redirect(input, expected_state) -> Result<String> {
      let is_callback_payload = trimmed.contains('?') || params has code/state/error;  // :297
      // state check is gated on is_callback_payload (:310-318)
      if let Some(code) = params.get("code") { return Ok(code); }       // :320
      if !is_callback_payload { return Ok(trimmed.to_string()); }       // :324 — raw path as code!
      bail!("Missing OAuth code in callback")
  }
  ```
- PKCE: `generate_pkce_state` (`:73-84`, S256), `pkce_generation_is_valid` test (`:459-464`) asserts only lengths.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib auth` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/auth/profiles.rs` (stale-lock detection)
- `src/auth/openai_oauth.rs` (loopback accept-loop + require code+state; PKCE test)

**Out of scope**:
- The token exchange/refresh network calls (a separate coverage plan could mock them; not here).
- The auth store file mode (plan 238).

## Git workflow

- Branch: `fix/auth-lock-and-oauth-loopback`
- Message e.g. `fix(auth): recover a stale auth lock and require code+state on the OAuth loopback`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Detect and reclaim a stale auth lock

In `acquire_lock`, on `AlreadyExists`, read the `pid=` line from the lock file; if the PID is not alive (or the file's mtime exceeds a generous bound, e.g. > 60s), remove the lock and retry once. Include the lock path and "delete it if no other rantaiclaw is running" in the timeout error message.

**Verify**: Test-plan `stale_lock_is_reclaimed` passes.

### Step 2: Loop the loopback accept until a real callback arrives

In `receive_loopback_code`, wrap `accept()`+read in a loop under the overall `timeout`: for each connection, if the request path contains `code=` OR `error=`, process it; otherwise serve `204 No Content` and continue waiting. Loop the `read()` if the request line is split across segments. Only the callback request drives `parse_code_from_redirect`.

**Verify**: Test-plan `stray_request_does_not_consume_the_callback` passes.

### Step 3: Require `code=` and a matching `state=` on the loopback path

Remove the raw-path escape hatch for the loopback path: `receive_loopback_code` must require a `code=` param AND a `state=` that matches `expected_state` (reject otherwise). Keep the raw-code escape hatch (`parse_code_from_redirect:324`) ONLY for the interactive paste-redirect entry point (verify which caller that is; if only the loopback uses it, delete the escape hatch, else gate it by an argument).

**Verify**: Test-plan `loopback_requires_code_and_matching_state` passes.

### Step 4: Bind the PKCE challenge in the test

Rewrite `pkce_generation_is_valid` to assert `code_challenge == base64url_nopad(SHA256(code_verifier))` and that two `generate_pkce_state()` calls differ in both verifier and state, and that `build_authorize_url` contains `code_challenge_method=S256`.

**Verify**: `cargo test --lib auth::openai_oauth` → pass.

## Test plan

- `auth::profiles`: `stale_lock_is_reclaimed` — write a lock file with a dead PID and old mtime; assert `acquire_lock` reclaims it and succeeds.
- `auth::openai_oauth`: `stray_request_does_not_consume_the_callback` — simulate a no-`?` request then a real `?code=…&state=…`; assert the real one wins. (Use `parse_code_from_redirect` directly if the socket loop is hard to test, plus a smaller test of the loop's filter.)
- `loopback_requires_code_and_matching_state` — a path with no `code`, or a mismatched `state`, → Err; the raw path is never returned as a code.
- `pkce_generation_is_valid` (rewritten) — the SHA-256/base64url binding + distinctness + `S256`.
- Verification: `cargo test --lib auth` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped auth tests pass incl. the 4 new/rewritten tests
- [ ] `parse_code_from_redirect` no longer returns a raw path as a code on the loopback path (asserted by test)
- [ ] `git status` shows only the two in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- The raw-code escape hatch (`:324`) has a legitimate non-loopback caller you can't identify — keep it gated by an explicit argument rather than deleting; report which caller.
- Stale-lock reclamation could race a genuinely concurrent holder — require BOTH pid-not-alive AND an mtime bound before reclaiming; report if either signal is unavailable on the target.

## Maintenance notes

- Reviewer: confirm a stray loopback request cannot derail login and that state is always verified on the callback (the CSRF property).
- Rotation: if any login on an affected build completed with an unverified state, treat that session as suspect.
- The auth store mode (plan 238) and this lock fix both touch `auth/profiles.rs`; coordinate land order.
