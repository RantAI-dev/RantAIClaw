# Plan 233: Stop `doctor models` / model probes from leaking provider credentials

> **Executor instructions**: Follow step by step; run every verification command
> and confirm before moving on. On any "STOP condition", stop and report. Update
> this plan's row in `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/onboard/wizard.rs src/doctor/legacy.rs src/doctor/checks/channels.rs`
> On any change to these files, compare the excerpts below before proceeding; mismatch = STOP.

## Status

- **Priority**: P1
- **Effort**: S–M
- **Risk**: HIGH (credential-handling; a wrong change could still leak or could break the probe)
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Two credential leaks on the diagnostics path, both triggered by a normal `rantaiclaw doctor models` or `models refresh --all`:

1. **Wrong-key broadcast.** `run_models_refresh` takes the ACTIVE provider's `api_key` (`config.api_key`) and sends it to EVERY provider in the sweep (`doctor models` with no `--provider` iterates all ~34 registered providers). The active key is transmitted as `Authorization: Bearer`, `x-api-key`, and — for Gemini — as a URL query parameter `?key=…`. So one command hands the operator's single configured secret to every third-party host, and lands it in Google's request logs. The URL is already gated per-provider (`active_provider_api_url`); the key never got the same gate.
2. **Key echoed in error text.** `fetch_gemini_models` puts the key in the query string; on failure the retained `reqwest::Error` Display appends ` for url (…)` with the key, and `format_error_chain` prints the whole chain to the terminal / CI logs. The Telegram doctor probe has the same shape (bot token in the URL path).

After this lands: each provider is probed only with ITS OWN key (resolved via `resolve_key_for_provider`, else the provider-specific env var), and no probe error text can contain a credential.

## Current state

- `src/onboard/wizard.rs`:
  - `run_models_refresh` (~`:2143`): `let api_key = config.api_key.clone().unwrap_or_default();` then `fetch_live_models_for_provider(&provider_name, &api_key, provider_api_url)` at `:2146`. This is the wrong-key source: it uses the active key for whatever `provider_name` is being probed.
  - `fetch_live_models_for_provider` (`:1721`): only falls back to the provider env var when the passed `api_key` is empty (`:1729 if api_key.trim().is_empty()`), so a non-empty active key always wins.
  - `active_provider_api_url` (`:1712`): the exemplar gate — returns the stored `api_url` only when the probed provider IS the active provider. The key needs the same treatment.
  - `fetch_gemini_models` (`:1608-1623`): `.query(&[("key", api_key), ("pageSize","200")])` then `.and_then(error_for_status).context("model fetch failed: GET Gemini models")` — the source error keeps the URL (with key).
- `src/config/schema.rs`: `resolve_key_for_provider(&self, provider: &str) -> Option<String>` (`:3893-3916`) is the correct per-provider resolver: checks `provider_api_keys[provider]`, then the top-level `api_key` only when `provider` IS the default provider, else `None`.
- `src/doctor/legacy.rs`: `format_error_chain` (~`:872-885`) walks `error.chain()` and joins every cause — so a safe outer `.context` does NOT hide an inner URL-bearing cause. Reachable from `doctor models` via `run_models_refresh`.
- `src/doctor/checks/channels.rs` (~`:235-244`): Telegram probe builds `format!("https://api.telegram.org/bot{token}/getMe")` and maps errors with `format!("network: {e}")` — token in the URL leaks into the message. Existing helper `reqwest::Error::without_url()` + a `safe_target` pattern is used at `src/onboard/provision/validate/http.rs:11-63` — reuse it.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib onboard::wizard` | pass |
| Test | `cargo test --lib doctor` | pass |

**Disk constraint**: never run bare `cargo test`. Use the scoped filters above.

## Scope

**In scope**:
- `src/onboard/wizard.rs` (per-provider key resolution + gemini error scrub)
- `src/doctor/checks/channels.rs` (telegram probe error scrub)
- `src/doctor/legacy.rs` — only if you add a scrubbing backstop to `format_error_chain` (optional Step 4)

**Out of scope**:
- `src/config/schema.rs` — read `resolve_key_for_provider` only; do not change it.
- The Discord/Slack probes (they pass tokens as headers, already safe).
- `src/providers/gemini.rs:500` — a sibling of the same class, out of this cluster; note it in Maintenance for a follow-up, don't fix here.

## Git workflow

- Branch: `fix/doctor-probe-credential-leak`
- Message e.g. `fix(security): resolve model-probe keys per provider and scrub credentials from probe errors`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Resolve the key per probed provider, not from the active provider

In `run_models_refresh` (`wizard.rs:2143`), replace `let api_key = config.api_key.clone().unwrap_or_default();` with a per-provider resolution: `let api_key = config.resolve_key_for_provider(&provider_name).unwrap_or_default();`. Leave `fetch_live_models_for_provider`'s empty-key env fallback intact (`:1729`) so a provider with no stored key still tries its own env var. Confirm `provider_name` is in scope at that line.

**Verify**: `cargo test --lib onboard::wizard` → pass. Add the test in the Test plan and re-run.

### Step 2: Keep the Gemini key out of the error chain

In `fetch_gemini_models` (`wizard.rs:1608`), build the request URL but convert transport/status errors with `.map_err(|e| e.without_url())` (or the `safe_target` helper at `src/onboard/provision/validate/http.rs`) BEFORE `.context(...)`, so no cause in the chain carries the query string. Keep the `?key=` query on the actual request; only the ERROR must be scrubbed.

**Verify**: the Test-plan test `gemini_model_fetch_error_omits_the_key` passes.

### Step 3: Scrub the Telegram doctor probe error

In `src/doctor/checks/channels.rs` (~`:235`), route the Telegram probe through the same scrub — apply `without_url()` to the `reqwest::Error` before formatting, or move to the hardened `probe_get` helper used by the Discord/Slack branches. The rendered `channel auth failures: …` string must never contain the bot token.

**Verify**: the Test-plan test `telegram_probe_error_omits_the_token` passes.

### Step 4 (optional backstop): scrub `format_error_chain`

If low-cost, add a final scrub in `format_error_chain` (`doctor/legacy.rs:872`) that redacts `key=<...>` query params and `user:pass@` userinfo from the joined string — a defense-in-depth net for every provider probe. Skip if it risks the existing tests; the primary fixes are Steps 1-3.

**Verify**: `cargo test --lib doctor` → pass.

## Test plan

- `onboard::wizard`: `probing_a_non_default_provider_sends_no_active_key` — build a `Config` with `default_provider="anthropic"`, `api_key=Some("...")`, no `provider_api_keys["openai"]`; assert `run_models_refresh(..., Some("openai"), ...)` resolves `None`/empty for the key (i.e. does not reuse the anthropic key). Use a distinctive local marker for the key value; assert it is NOT sent.
- `onboard::wizard`: `gemini_model_fetch_error_omits_the_key` — force an error (unreachable host / 401 via a mock or a bad base) and assert the returned error's full chain string does not contain the key marker. Model after any existing `model_fetch_error_never_echoes_the_endpoint_value` test if present.
- `doctor::checks::channels`: `telegram_probe_error_omits_the_token` — assert the rendered failure string excludes the token marker.
- Verification: `cargo test --lib onboard::wizard doctor` → all pass incl. new tests.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `cargo test --lib onboard::wizard` and `cargo test --lib doctor` pass with the 3 new tests
- [ ] `grep -n "config.api_key.clone().unwrap_or_default()" src/onboard/wizard.rs` returns nothing (the wrong-key line is gone)
- [ ] `git status` shows only the in-scope files modified
- [ ] `plans/README.md` row updated

## STOP conditions

- `resolve_key_for_provider` no longer exists or changed signature (drift) — STOP.
- A new test can't force a scrubbable error without real network — use a mock (`mockito` is used at `tests/doctor_checks.rs`) or an unroutable host; if neither works, report rather than shipping an untested scrub.
- The change appears to require editing `src/providers/` — out of scope; report.

## Maintenance notes

- Reviewer: confirm each probe now uses the probed provider's own key, and that at least one test proves an error string is credential-free (not just that the happy path works).
- Rotation: any key that has passed through a `doctor models`/`models refresh --all` sweep on an affected build should be treated as exposed and rotated with the provider.
- Follow-up (separate): `src/providers/gemini.rs:500` propagates a raw URL-bearing `reqwest::Error` — same class, out of this plan's scope.
