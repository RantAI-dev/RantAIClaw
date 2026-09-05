# Plan 257: Remove or honestly label dead config keys

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/config/schema.rs src/config/migrations.rs src/config/mod.rs src/config/runtime.rs docs/reference/config.md docs/operations/resource-limits.md docs/security/audit-logging.md`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: MED (removing config keys is a breaking change; needs a migration arm so existing files still parse)
- **Depends on**: none (coordinates with plan 253 on `CURRENT_VERSION`)
- **Category**: tech-debt
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

- **G7 (dead keys documented as working)**: `security.resources.*`, `audit.sign_events`, `cost.allow_override`, `cost.prices` (+ an 80-line default pricing table), `agent.parallel_tools`, `sandbox.firejail_args` serialize into every `config.toml` and every `config schema` output and are documented as functional — but nothing reads them. `security/mod.rs:11` already says the sandbox/audit blocks "have no effect today", so the code and docs already contradict each other. Per CLAUDE.md §6.4 config keys are public contract; shipping non-functional ones is the expensive kind of dead code.
- **G2 (dead module)**: `src/config/runtime.rs` (158 lines + 5 never-run tests) is not declared in `config/mod.rs`, so it compiles nowhere, and its header documents a `config.runtime.toml` overlay feature the binary does not implement.

## Current state (confirm before editing)

- Unread keys (grep each across `src/`, `tests/` — reader only in `schema.rs`): `ResourceLimitsConfig` `schema.rs:3282` + `security.resources` `:3228`; `audit.sign_events` `:3344`; `cost.allow_override` `:720` + `cost.prices` `:724` (default table `get_default_pricing` `:765-845`); `agent.parallel_tools` `:406` (only its own round-trip tests at `:5114,:5133`); `sandbox.firejail_args` `:3248` (only ever written `Vec::new()` at `security/detect.rs:129,143`).
- `src/security/mod.rs:11-14` — already states `[security.sandbox]`/`[security.audit]` "has no effect today".
- Docs: `docs/operations/resource-limits.md:89`, `docs/security/frictionless-security.md:185`, `docs/security/audit-logging.md:124`, `docs/reference/config.md:105,232`.
- `src/config/mod.rs:1-5` declares `api_url, fingerprint, migrations, schema, watcher` — NO `runtime`. `src/config/runtime.rs` implements `runtime_path`, `read_runtime_overrides`, `write_runtime_section`, `load_with_runtime_overrides`, etc., grep-confirmed unused.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config` | pass |
| Snapshot | `cargo test --test schema_drift` (+ intentional `insta accept`) | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: `src/config/schema.rs` (remove the dead keys), `src/config/migrations.rs` (drop-arms + version bump), `src/config/mod.rs`/`src/config/runtime.rs` (delete the dead module), the four doc pages.
**Out of scope**: the `[security.sandbox]`/`[security.audit]` DECISION (whether to implement them) — that is a separate spike (plans 215/218 referenced by `security/mod.rs:11`). Here, only LABEL them reserved if you don't delete them; delete the ones with no plan (`cost.allow_override`, `cost.prices`, `agent.parallel_tools`).

## Git workflow

- Branch: `chore/remove-dead-config-keys`
- Message e.g. `chore(config): remove dead config keys and the uncompiled runtime module`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Delete `src/config/runtime.rs`

Remove the file (it is declared nowhere). Confirm nothing references `config::runtime`, `load_with_runtime_overrides`, `write_runtime_section` (grep returns only that file before deletion).

**Verify**: `cargo build --lib` exit 0.

### Step 2: Delete the keys that no plan claims, with migration drop-arms

Delete `cost.allow_override`, `cost.prices` (+ `get_default_pricing`), `agent.parallel_tools`. Add a `migrate_vN` arm that DROPS these keys from an existing `config.toml` (bump `CURRENT_VERSION`; coordinate with plan 253 if it also bumps — use one bump). Regenerate the schema-drift snapshot intentionally.

**Verify**: `cargo test --lib config::migrations` → pass; `cargo test --test schema_drift` → pass after intentional accept.

### Step 3: Label the security-staged keys reserved (don't delete)

For `security.resources.*`, `audit.sign_events`, `sandbox.firejail_args` (which `security/mod.rs:11` marks as staged for future plans), do NOT delete — instead correct the docs (`resource-limits.md`, `audit-logging.md`, `frictionless-security.md`, `config.md`) to say "reserved, not yet enforced", matching `security/mod.rs`.

**Verify**: `grep -rn "config set security.resources" docs/` shows the command is no longer presented as functional (reworded to reserved).

## Test plan

- `config::migrations`: `dropped_keys_are_removed_on_migration` — an old config carrying `cost.prices`/`parallel_tools` comes out without them, stamped current, and still parses.
- Schema-drift snapshot updated intentionally.
- Verification: `cargo test --lib config` + `cargo test --test schema_drift` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `src/config/runtime.rs` deleted; no reference remains
- [ ] the three deleted keys have a migration drop-arm; scoped tests pass
- [ ] docs no longer present reserved keys as functional
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- One of the "unread" keys turns out to HAVE a reader you missed (grep more broadly incl. `benches/`, `examples/`) — do NOT delete it; report.
- Deleting `cost.prices` breaks a cost-estimation path — the finding says it's unread; if a reader exists, STOP and reclassify.

## Maintenance notes

- Reviewer: confirm each deleted key truly has no reader (broad grep) and that the migration only DROPS keys, never touches a set value elsewhere.
- The security-staged keys stay until plans 215/218 decide implement-or-remove; this plan only stops them lying to operators.
- Interacts with plan 253 and 254 (`CURRENT_VERSION`) — use a single coordinated bump.
