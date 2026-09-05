# Plan 241: Make migration write-back atomic and route out-of-band writers through the lock

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/config/schema.rs src/gateway/config_api.rs src/gateway/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED (config write path; hoisting a lock risks deadlock if a holder re-enters a locking path)
- **Depends on**: none (but read the note re: `lock_and_load` calling `load_or_init` which itself writes — the migration atomicity half should land first)
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

1. **Migration write-back is not atomic** (C4). After migrating a config on load, the migrated TOML is persisted with a bare `fs::write(&config_path, serialized)` — the ONE write that touches every config on every upgrade is the only unprotected one. `fs::write` truncates in place: a crash / power loss / ENOSPC mid-write leaves a truncated `config.toml` with no `.bak` to restore, and the migration is one-way. The normal `save()` path already does temp + fsync + backup + rename.
2. **Out-of-band writers clobber the file** (C10). `persist_approval_owner` (Telegram `/claim`) and `persist_pairing_tokens` (pairing) do NOT take `CONFIG_WRITE_LOCK` and save the whole file from a possibly-stale in-memory snapshot, silently reverting any edit made since that snapshot.

## Current state

- `src/config/schema.rs` migration write-back (`:4018-4021`):
  ```rust
  if migrated || stripped_credential {
      let serialized = toml::to_string_pretty(&raw)...?;
      if let Err(e) = fs::write(&config_path, serialized).await { ...warn... }
  }
  ```
  The atomic path lives in `save()` (~`:4596-4663`): unique temp via `create_new`, `write_all`, `sync_all`, chmod 0600, copy-to-`.bak`, `rename`, restore-on-failure. NOTE: the temp filename `.{file_name}.tmp-{uuid}` must not match the watcher filter (`src/config/watcher.rs:70-76`) — it doesn't; keep it that way.
- `src/gateway/config_api.rs:228-235`: the `CONFIG_WRITE_LOCK` doc comment states `persist_approval_owner` and `persist_pairing_tokens` do NOT take the lock and marks it a follow-up. `lock_and_load` (`:241`) loads FRESH from disk under the lock.
- `src/gateway/mod.rs:1288-1294`: `persist_pairing_tokens` clones the in-memory `config.lock()` (not a fresh disk read), sets `paired_tokens`, and `save()`s the whole file.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config::schema` | pass |
| Test | `cargo test --lib gateway` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/config/schema.rs` (extract the temp/fsync/backup/rename block from `save()` into a reusable `atomic_write_config(path, bytes)`; call it from the migration write-back)
- `src/gateway/config_api.rs` + `src/gateway/mod.rs` (route the two out-of-band writers through a fresh-load-under-lock read-modify-write) — do this ONLY if it can be done without cross-module deadlock; see STOP conditions

**Out of scope**:
- The credential-strip logic itself (works; keep it).
- Redesigning the reloader (plan 246).

## Git workflow

- Branch: `fix/config-write-atomicity`
- Message e.g. `fix(config): write migrated config atomically and serialize out-of-band writers`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Extract `atomic_write_config` and use it for the migration write-back

Extract the temp + fsync + backup + rename sequence from `save()` into a private `async fn atomic_write_config(path: &Path, bytes: &[u8]) -> Result<()>` (Unix: mode 0600 at open, keep the `.bak`). Have both `save()` and the migration write-back (`schema.rs:4018-4021`) call it. When `migrated`, keep the `.bak` so an upgrade is reversible.

**Verify**: `cargo test --lib config::schema` → pass; Test-plan `migration_writeback_is_atomic` passes.

### Step 2: Serialize the out-of-band writers (only if deadlock-safe)

Move `CONFIG_WRITE_LOCK` into `src/config/` and expose `config::with_config_write<F>(f)` that locks → loads fresh from disk → applies `f` → saves. Convert `persist_pairing_tokens` (`gateway/mod.rs:1288`) and `persist_approval_owner` to use it (read fresh, set the one field, save) instead of cloning the stale in-memory snapshot. **First confirm** `lock_and_load` → `Config::load_or_init` does not re-enter this lock (it currently does not take the lock itself; verify) — if it can write during load (the migration write-back), sequence Step 1 before this so the load-time write is atomic.

**Verify**: `cargo test --lib gateway` → pass; Test-plan `pairing_write_preserves_concurrent_edit` passes.

## Test plan

- `config::schema`: `migration_writeback_is_atomic` — after a migrating load, assert a `.bak` exists and the target parses; assert no `.tmp` residue. (A true crash-during-write test is hard; assert the atomic sequence produces backup + valid target.)
- `gateway`: `pairing_write_preserves_concurrent_edit` — write field X to disk; then run the pairing-token persist; re-read and assert X survives (proves it read fresh, not from a stale snapshot).
- Verification: `cargo test --lib config::schema gateway` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `grep -n "fs::write(&config_path" src/config/schema.rs` returns nothing (migration write-back now atomic)
- [ ] scoped tests pass incl. the two new tests
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Hoisting `CONFIG_WRITE_LOCK` into `src/config/` creates a deadlock (a holder re-enters via `save`/`load_or_init`) — implement Step 1 only, mark Step 2 deferred, report the re-entrancy path.
- The `save()` temp/backup/rename block can't be cleanly extracted without changing its behavior — report; do NOT weaken the atomic sequence.

## Maintenance notes

- Reviewer: confirm the migration write-back now produces a `.bak` and that the pairing write reads fresh from disk (concurrent-edit test).
- Interacts with plans 236 and 246 (both touch gateway config paths). Land Step 1 independently even if Step 2 is deferred.
