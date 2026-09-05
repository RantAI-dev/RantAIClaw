# Plan 232: Redact every secret-bearing field in the config API response

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving on. If anything in "STOP
> conditions" occurs, stop and report — do not improvise. When done, update this
> plan's row in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/gateway/config_api.rs src/config/schema.rs`
> If either file changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH (touches the config API response boundary; a wrong redaction could hide a field the console needs, or fail to hide a secret)
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

`GET /api/v1/config` returns the whole config (redacted) to any paired console client. Three secret-bearing surfaces are NOT redacted and leak in cleartext into the browser and into `config show`:

1. `cfg.api_url` — the sibling endpoint `GET /api/v1/config/secrets` deliberately withholds it when it looks like a credential (`secrets_view`), but `redact_config_secrets` does not, so the two endpoints disagree. A credential parked in `api_url` (e.g. `https://user:pass@host/v1` or `?key=…`) is returned verbatim.
2. `mcp_servers.<name>.args` and `.command` — MCP servers are commonly launched as `npx -y <server> --api-key <token>`; the suffix-based `redact_secrets_in_json` recurses into the `args` array (whose items have no keys) and returns every arg verbatim.
3. `mcp_servers.<name>.env` values keyed by an operator-chosen name outside the secret-suffix list (`DATABASE_URL`, `PGPASSWORD`, `SENTRY_DSN`), and `proxy.http_proxy`/`https_proxy`/`all_proxy` whose `user:pass@` userinfo is a credential.

After this lands, no secret-bearing config value leaves the gateway in cleartext, and a completeness test walks the real `Config` struct so a newly-added secret field fails the test instead of leaking.

## Current state

- `src/gateway/config_api.rs` — the config API. Redaction lives in two functions:
  - `redact_secrets_in_json` (`config_api.rs:133-175`): a recursive JSON walk that blanks values whose KEY matches a secret suffix. Its `is_secret_key` (`:134-149`) matches `_token`/`_secret`/`_password`/`_key`/`credential`/`db_url` and a few exact names. The array branch (`:168-172`) recurses into items with no keys — so `args` values pass through.
  - `redact_config_secrets` (`config_api.rs:179-207`): typed field clearing — sets `cfg.api_key=None`, clears `provider_api_keys`, `composio.api_key`, `browser.computer_use.api_key`, `web_search.brave_api_key`, `storage.provider.config.db_url`, per-agent keys, telegram `bot_token`, knowledge keys, skill literal keys. It does NOT touch `cfg.api_url`, `cfg.proxy`, or `cfg.mcp_servers`.
- `get_config` calls `redact_config_secrets` then serializes and runs `redact_secrets_in_json` over the JSON as a backstop (confirm this ordering at the `get_config` handler near `config_api.rs:103-119`).
- `secrets_view` (`config_api.rs:781-792`) is the exemplar for how `api_url` should be treated:
  ```rust
  let api_url = cfg.api_url.as_deref().filter(|value| !looks_like_api_key(value));
  ```
  `looks_like_api_key` already exists in this file — reuse it. Note it only catches the 6 known key PREFIXES; a `user:pass@` or `?key=` URL is not caught, so extend the sanitizer (Step 2).
- `ProxyConfig` fields are `Option<String>` at `src/config/schema.rs:1409-1421` (`http_proxy`, `https_proxy`, `all_proxy`, `no_proxy`).
- Repo error style: functions return `Result`, redaction fns are pure `pub(crate) fn` mutating `&mut`. Match the existing `redact_config_secrets` shape.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0 |
| Clippy (scoped) | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test (scoped) | `cargo test --lib gateway::config_api` | all pass |
| Integration test | `cargo test --test config_api` | all pass |

**Disk constraint**: this box is disk-limited; NEVER run bare `cargo test` (writes ~27G). Use the scoped `--lib`/`--test` filters above only.

## Scope

**In scope**:
- `src/gateway/config_api.rs` (redaction functions + one new test)

**Out of scope** (do NOT touch):
- `src/config/schema.rs` — read only, for the `ProxyConfig` field names. Do not change the encrypt/decrypt lists here.
- The `secrets_view` endpoint — it already withholds `api_url` correctly; don't change its contract.
- The claw-ui client mask (`console.ts`) — kept as defense-in-depth in a separate claw-ui PR; this plan moves the authoritative boundary to the server, so do not remove the client mask here.
- MCP write path validation — that is plan 234; here you only redact the READ response.

## Git workflow

- Branch: `fix/config-api-secret-redaction`
- Conventional-commit message, e.g. `fix(security): redact api_url, proxy userinfo, and MCP args/env in the config response`
- Do NOT push or open a PR unless the operator instructs it.

## Steps

### Step 1: Redact `mcp_servers.*.env`, `.args`, `.command` in `redact_config_secrets`

In `redact_config_secrets` (`config_api.rs:179`), after the existing field clears, add a loop over `cfg.mcp_servers.values_mut()` that blanks every `env` value (keep keys), and blanks arg values that look secret. Conservative rule for args: blank an arg equal to a value that `looks_like_api_key`, and blank the token FOLLOWING a credential-shaped flag (an arg starting with `--` and containing `key`/`token`/`secret`/`password` case-insensitively). Leave `command` as-is unless it itself `looks_like_api_key`.

**Verify**: `cargo test --lib gateway::config_api` → all pass (no behavior asserted yet; just compiles clean).

### Step 2: Add a shared `api_url` sanitizer and apply it in `redact_config_secrets`

Add a helper `fn sanitize_api_url(value: &str) -> Option<String>` that returns `None` when `looks_like_api_key(value)` is true, and otherwise strips URL userinfo (`user:pass@`) and a `key=`/`api_key=`/`access_token=` query parameter, returning the cleaned URL. In `redact_config_secrets`, set `cfg.api_url = cfg.api_url.as_deref().and_then(sanitize_api_url)`. Then update `secrets_view` (`config_api.rs:782-785`) to call the SAME helper so both endpoints share one policy.

**Verify**: `cargo test --lib gateway::config_api` → all pass.

### Step 3: Redact proxy userinfo

In `redact_config_secrets`, for each of `cfg.proxy.http_proxy`, `https_proxy`, `all_proxy`: if set, strip the `user:pass@` userinfo component (reuse the userinfo-strip logic from Step 2), leaving host/port visible so the operator can still see which proxy is configured.

**Verify**: `cargo fmt --all -- --check` → exit 0; `cargo clippy --lib -- -D warnings` → exit 0.

### Step 4: Replace the hand-fixture completeness test with a real-`Config` walk

Find the existing test `redact_secrets_in_json_nulls_all_channel_and_gateway_secrets` (~`config_api.rs:1146`). Add a NEW test `redact_config_secrets_leaves_no_marker_in_real_config` that: builds a `Config` (start from `Config::default()`), writes a distinctive marker string (e.g. `"MARKER_SECRET_a1b2c3"`) into EVERY secret-bearing field including `api_url`, one `mcp_servers` entry's `command`/`args`/`env`, and `proxy.http_proxy = "http://u:MARKER_SECRET_a1b2c3@host:8080"`; runs `serde_json::to_value` → `redact_config_secrets` → `redact_secrets_in_json`; then asserts the serialized string contains no `"MARKER_SECRET"`. Keep the marker OUT of the plan's committed value — use a local `const`.

**Verify**: `cargo test --lib gateway::config_api::redact_config_secrets_leaves_no_marker_in_real_config` → passes. Then temporarily delete your Step 1 mcp loop and re-run — it MUST fail (proves the test catches the leak). Restore the loop.

## Test plan

- New test `redact_config_secrets_leaves_no_marker_in_real_config` (Step 4) — the completeness guard.
- Extend the existing redaction test to assert `api_url` with a `user:pass@` URL comes back with userinfo stripped, and an MCP `env` value under a non-suffix key (`DATABASE_URL`) comes back empty.
- Model new tests after the existing `#[cfg(test)]` block in `config_api.rs` (starts ~`:1058`).
- Verification: `cargo test --lib gateway::config_api` and `cargo test --test config_api` → all pass including the new tests.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --lib -- -D warnings` exits 0
- [ ] `cargo test --lib gateway::config_api` and `cargo test --test config_api` pass, with the new completeness test present
- [ ] The completeness test fails when the Step-1 mcp loop is removed (verified in Step 4)
- [ ] `git status` shows only `src/gateway/config_api.rs` modified
- [ ] `plans/README.md` row updated

## STOP conditions

- The excerpts in "Current state" don't match the live code (drift).
- `looks_like_api_key` no longer exists in `config_api.rs` — report; the plan assumes it.
- Removing the mcp loop does NOT fail the completeness test — the test isn't actually walking the struct; report instead of shipping a vacuous test.
- Any change appears to require editing `src/config/schema.rs` beyond reading field names.

## Maintenance notes

- Reviewer: confirm the completeness test serializes the REAL `Config` (not a hand JSON literal) and that removing a redaction line makes it fail.
- Future: when a new secret field is added to `Config`, this test fails until the field is redacted — that is the intended tripwire. Keep `redact_config_secrets`'s comment "Keep in sync with the encrypt/decrypt lists in config::schema".
- Deferred: masking args by position could hide a legitimate flag; the conservative flag-follows rule may miss an exotic credential arg. The completeness test covers the `looks_like_api_key` case regardless.
