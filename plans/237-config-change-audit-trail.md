# Plan 237: Record privileged config changes to the audit log

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/security/audit.rs src/gateway/config_api.rs src/gateway/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW (additive; the risk is logging a secret VALUE — the plan forbids that)
- **Depends on**: none (coordinates with plan 235, which also touches the mutating handlers)
- **Category**: security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

A fully-built audit subsystem exists (`AuditEventType::ConfigChange`, `AuditLogger`) with ZERO production callers. Meanwhile the config API can lower autonomy to `Full`, empty `always_ask`, and set `block_high_risk_commands=false` over HTTP, recorded nowhere. Post-incident, an operator cannot distinguish "the agent was always in Full autonomy" from "someone flipped it over the API an hour ago". After this lands, every mutating config-API write emits one audit record (field NAMES and before/after for non-secret fields only — never values of secret fields).

## Current state

- `src/security/audit.rs`:
  - `AuditEventType::ConfigChange` (`:19`), `AuditEvent::new(event_type)` builder with `.with_actor(channel, user_id, username)` (`:91`) and `.with_action(command, risk_level, approved, allowed)` (`:106`).
  - `AuditLogger { log_path, config: AuditConfig, buffer }`, constructed via `AuditLogger::new(config: AuditConfig, rantaiclaw_dir: PathBuf)` (`:167`). Read the REST of `security/audit.rs` past line 175 for the append/flush method name and signature — you will call it.
  - A repo-wide grep for `AuditLogger` / `AuditEvent` outside `security/audit.rs` and the `pub use` in `security/mod.rs` returns nothing (confirm with `grep -rn "AuditLogger" src/ | grep -v security/audit.rs`).
- `src/gateway/config_api.rs`: the mutating handlers are `set_model`, `set_autonomy`, `apply_secrets`/`set_secrets`, `add_mcp_server`/`remove_mcp_server`, telegram connect/disconnect, `set_knowledge`. All funnel through `persist_and_swap` (`:253`).
- `AuditConfig` and its `default_audit_enabled` live in `src/config/schema.rs` (`AuditConfig` ~`:3329`). The gateway already has access to the loaded `Config` in `AppState`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib security::audit` | pass |
| Test | `cargo test --lib gateway::config_api` | pass |
| grep | `grep -rn "AuditLogger" src/ \| grep -v security/audit.rs` | shows the new call site(s) after |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/gateway/config_api.rs` (emit an audit record on mutating writes — the cleanest single point is `persist_and_swap`, given the changed field names)
- `src/security/audit.rs` — only if a small helper (e.g. a `config_change(fields)` constructor) makes the call site clean
- `src/security/mod.rs` — only if a re-export is needed

**Out of scope**:
- The audit SINK design (rotation, retention) beyond what `AuditLogger` already does — reuse the existing append.
- Logging secret VALUES — forbidden. Field names + non-secret before/after only.
- The other audit event types (command execution, etc.) — this plan is config changes only.

## Git workflow

- Branch: `feat/config-change-audit-trail`
- Message e.g. `feat(security): record config-API changes to the audit log`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Determine the changed non-secret fields at the write boundary

Decide the minimal actor identity available: the authenticated principal is a hashed pairing token (not individually labelled) — record `channel: "web-console"` and, if available, a token-prefix or `None` for user_id. In each mutating handler (or in `persist_and_swap` if you thread a `&[&str] changed_fields` argument), collect the NAMES of the fields the request set (e.g. `["autonomy.level","autonomy.always_ask"]`) and, for non-secret fields, the before/after values.

**Verify**: `cargo build --lib` compiles.

### Step 2: Emit one `ConfigChange` record per successful write

After a successful `persist_and_swap`, build `AuditEvent::new(AuditEventType::ConfigChange).with_actor("web-console", …, …)` and attach the changed field names (use `with_action` or a small new helper), then append via the `AuditLogger` method you found in audit.rs. Gate on `AuditConfig::enabled` so a disabled audit config is a no-op. NEVER include values of secret-bearing fields (`api_key`, tokens, `provider_api_keys`, telegram token, knowledge keys, mcp env).

**Verify**: Test-plan `autonomy_weakening_emits_one_config_change_record` passes.

### Step 3: Confirm no secret leakage into the record

Add an assertion in the test that a write which sets a secret (e.g. `set_secrets`) produces a record naming the field but NOT its value (use a local marker for the secret and assert the marker is absent from the serialized record).

**Verify**: Test-plan `config_change_record_omits_secret_values` passes.

## Test plan

- `gateway::config_api` (or `security::audit` with a seam): `autonomy_weakening_emits_one_config_change_record` — a `PUT autonomy` lowering the level produces exactly one `ConfigChange` record naming the changed fields.
- `config_change_record_omits_secret_values` — a secret-setting write records the field name but not the value (marker-absent assertion).
- `audit_disabled_emits_nothing` — with `AuditConfig::enabled=false`, no record.
- Verification: `cargo test --lib security::audit gateway::config_api` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped audit + config_api tests pass with the 3 new tests
- [ ] `grep -rn "AuditLogger" src/ | grep -v security/audit.rs` now shows the config-API call site
- [ ] no secret value appears in any emitted record (asserted by test)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `AuditLogger`'s append method is async and the handler context can't await it cleanly — report; a `spawn_blocking` or buffered flush may be needed.
- The actor identity is genuinely unavailable (no principal threaded to the handler) — record `channel:"web-console", user_id:None` and note the limitation; do NOT fabricate an identity.
- Wiring requires touching more than the config-API handlers + audit module — report before widening.

## Maintenance notes

- Reviewer: confirm no secret value can reach the record (the marker-absent test) and that a disabled audit config is a true no-op.
- Follow-up (not this plan): wire the other `AuditEventType`s (auth success/failure, policy violation) — this plan intentionally scopes to config changes to keep review tight.
- Interacts with plan 235 (both edit the mutating handlers / `persist_and_swap`); land order matters.
