# Plan 179: Add WAL + `busy_timeout` to the cron store, make `record_run` IMMEDIATE, and run column migration once

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/sessions/store.rs`
> If `src/cron/store.rs` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (interacts with `plans/167` — see Maintenance notes)
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`jobs.db` (the cron store) is written by four concurrent actors: the in-process
scheduler poll loop, the gateway HTTP handlers, the TUI, and separate CLI
processes. Every store call opens a **fresh** connection whose only PRAGMA is
`foreign_keys = ON` — no `busy_timeout`, no WAL. So the moment two of those
writers overlap, the loser gets `SQLITE_BUSY` **immediately** ("database is
locked") instead of retrying: a failed `cron add`, a lost run record, a dropped
reschedule. The sibling `src/sessions/store.rs` documents `busy_timeout` as
**required** for exactly this reason and sets `WAL` + `busy_timeout=5000` +
`foreign_keys=ON`. This plan brings the cron store to the same standard.

Two smaller fixes ride along in the same file:

1. `record_run` uses a `DEFERRED` transaction (`unchecked_transaction`) that
   **reads then writes**; `src/sessions/store.rs` documents that pattern as
   deadlock-prone under contention and mandates `IMMEDIATE`.
2. The 9 `add_column_if_missing` calls run `PRAGMA table_info` (and maybe
   `ALTER`) on **every** store call — including the scheduler's `due_jobs`
   every poll tick (~15s). They only exist to migrate legacy on-disk databases;
   for a database this process has already opened they are pure overhead. Gate
   them so migration runs once per database path per process.

## Current state

### `src/cron/store.rs` — `with_connection` (lines 513–573)

```rust
fn with_connection<T>(config: &Config, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let db_path = config.workspace_dir.join("cron").join("jobs.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cron directory: {}", parent.display()))?;
    }

    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open cron DB: {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS cron_jobs (
            ...
        );
        ...indexes and cron_runs table...",
    )
    .context("Failed to initialize cron schema")?;

    add_column_if_missing(&conn, "schedule", "TEXT")?;
    add_column_if_missing(&conn, "job_type", "TEXT NOT NULL DEFAULT 'shell'")?;
    add_column_if_missing(&conn, "prompt", "TEXT")?;
    add_column_if_missing(&conn, "name", "TEXT")?;
    add_column_if_missing(&conn, "session_target", "TEXT NOT NULL DEFAULT 'isolated'")?;
    add_column_if_missing(&conn, "model", "TEXT")?;
    add_column_if_missing(&conn, "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(&conn, "delivery", "TEXT")?;
    add_column_if_missing(&conn, "delete_after_run", "INTEGER NOT NULL DEFAULT 0")?;

    f(&conn)
}
```

The `execute_batch` string starts with `PRAGMA foreign_keys = ON;` (line 524)
and the 9 `add_column_if_missing` calls are lines 562–570.

### `src/cron/store.rs` — `record_run` transaction (lines 314–351)

```rust
with_connection(config, |conn| {
    // Wrap INSERT + pruning DELETE in an explicit transaction so that
    // if the DELETE fails, the INSERT is rolled back and the run table
    // cannot grow unboundedly.
    let tx = conn.unchecked_transaction()?;   // <-- line 318, DEFERRED

    tx.execute( /* INSERT INTO cron_runs ... */ )?;
    let keep = i64::from(config.cron.max_run_history.max(1));
    tx.execute( /* DELETE ... pruning ... */ )?;
    tx.commit().context("Failed to commit cron run transaction")?;
    Ok(())
})
```

### `src/sessions/store.rs` — the pattern to mirror (READ AND QUOTE BOTH SIDES)

The pragma batch that this plan copies (lines 124–137):

```rust
pub fn open(path: &Path) -> Result<Self> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open session db at {}", path.display()))?;

    // busy_timeout is REQUIRED: every /api/v1 handler opens its own connection
    // to this file, so concurrent writers must retry instead of failing
    // immediately with "database is locked". Matches channels/history_store.rs.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )?;
    run_migrations(&conn)?;

    Ok(Self { conn })
}
```

The `IMMEDIATE` rationale (lines 415–418, on `record_api_turn`):

```rust
/// Uses `IMMEDIATE` (not the default `DEFERRED`): this reads the session row
/// then writes, and two concurrent DEFERRED read→write transactions deadlock
/// with a `SQLITE_BUSY` that `busy_timeout` cannot resolve. `IMMEDIATE` takes
/// the write lock up front so contenders serialize and retry cleanly.
```

`record_api_turn` acquires IMMEDIATE via
`self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?`
(line 430). The cron store's closure only has a shared `&Connection`, so use
`rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?`
instead (same RAII transaction, chosen behavior; rusqlite `0.37`, Cargo.toml
line 123).

Conventions:

- `execute_batch` ignores the row that `PRAGMA journal_mode=WAL` returns — the
  sessions store uses it exactly this way, so it is safe here too.
- `add_column_if_missing` (lines 483–511) already tolerates the concurrent
  "duplicate column name" race, so running migration on two threads at once
  (first open) is safe.

## Commands you will need

| Purpose   | Command                                             | Expected on success        |
|-----------|-----------------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`                        | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings`         | exit 0, no warnings        |
| Tests     | `cargo test --lib cron`                             | all pass                   |

Do NOT run bare `cargo test` (disk-constrained). Scope to `cron`.

## Scope

**In scope** (the only file you should modify):

- `src/cron/store.rs` — `with_connection` (add WAL/busy_timeout pragmas + gate
  migration), `record_run` (switch to IMMEDIATE), and a new private helper +
  file-scoped static for the one-shot migration gate. Optionally one small test.

**Out of scope** (do NOT touch):

- `src/sessions/store.rs` — read-only reference; do not edit.
- `update_job` (lines 185–252) — belongs to `plans/167`.
- `add_column_if_missing` itself (lines 483–511) — keep its body; only change
  *how often* it is called.
- Changing `with_connection`'s `FnOnce(&Connection)` signature.

## Git workflow

- Branch: `advisor/179-cron-store-busy-timeout-wal`
- Conventional commits, e.g. `fix(cron): WAL + busy_timeout on the cron store, IMMEDIATE record_run, one-shot column migration`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add WAL + busy_timeout to the cron connection pragmas

In `with_connection`, change the first line of the `execute_batch` string from:

```
"PRAGMA foreign_keys = ON;
```

to:

```
"PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;
```

(Leave the rest of the batch — the `CREATE TABLE`/index statements — unchanged.)

**Verify**: `cargo test --lib cron` → all pass (the store still initializes).

### Step 2: Gate the 9 `add_column_if_missing` calls behind a per-path one-shot

Add a file-scoped static and a helper near the other private `fn`s in
`src/cron/store.rs` (fully-qualified paths avoid touching the import block):

```rust
/// Tracks which cron DB paths this process has already migrated, so the 9
/// `add_column_if_missing` PRAGMA scans run once per DB — not on every store
/// call (the scheduler calls `due_jobs` every poll tick).
static MIGRATED_CRON_DBS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = std::sync::OnceLock::new();

fn migrate_cron_columns_once(conn: &Connection, db_path: &std::path::Path) -> Result<()> {
    let set = MIGRATED_CRON_DBS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = set.lock().expect("cron migration lock poisoned");
    if guard.contains(db_path) {
        return Ok(());
    }
    add_column_if_missing(conn, "schedule", "TEXT")?;
    add_column_if_missing(conn, "job_type", "TEXT NOT NULL DEFAULT 'shell'")?;
    add_column_if_missing(conn, "prompt", "TEXT")?;
    add_column_if_missing(conn, "name", "TEXT")?;
    add_column_if_missing(conn, "session_target", "TEXT NOT NULL DEFAULT 'isolated'")?;
    add_column_if_missing(conn, "model", "TEXT")?;
    add_column_if_missing(conn, "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "delivery", "TEXT")?;
    add_column_if_missing(conn, "delete_after_run", "INTEGER NOT NULL DEFAULT 0")?;
    guard.insert(db_path.to_path_buf());
    Ok(())
}
```

Then in `with_connection`, replace the 9 direct `add_column_if_missing(&conn, ...)`
calls (lines 562–570) with a single call:

```rust
    migrate_cron_columns_once(&conn, &db_path)?;

    f(&conn)
```

Note: the `CREATE TABLE IF NOT EXISTS cron_jobs (...)` in the `execute_batch`
already declares every column, so a freshly created DB is fully-formed even
before migration — the gate skipping migration for a new DB is harmless. The
migration only matters for legacy on-disk DBs from an older binary, and the
per-path `HashSet` guarantees each such DB is still migrated once.

**Verify**: `cargo test --lib cron` → all pass (including the existing
`migration_falls_back_to_legacy_expression` and `job_type_from_sql_*` tests,
which each use a fresh DB with the full `CREATE TABLE` schema).

### Step 3: Switch `record_run`'s transaction to IMMEDIATE

In `record_run`, replace line 318:

```rust
        let tx = conn.unchecked_transaction()?;
```

with:

```rust
        // IMMEDIATE (not the default DEFERRED): this reads then writes, and two
        // concurrent DEFERRED read→write transactions deadlock with a
        // SQLITE_BUSY that busy_timeout cannot resolve. See sessions/store.rs.
        let tx = rusqlite::Transaction::new_unchecked(
            conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
```

Leave the INSERT, prune DELETE, and `tx.commit()` unchanged.

**Verify**: `cargo test --lib cron` → all pass (including `record_and_prune_runs`
and `record_run_truncates_large_output`).

### Step 4: Final validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0, no diff.
- `cargo clippy --all-targets -- -D warnings` → exit 0.
- `cargo test --lib cron` → all pass.

## Test plan

- No new behavior test is strictly required — the change is a resilience/pragma
  change and the existing `cron` store tests exercise every touched path
  (`record_and_prune_runs`, `remove_job_cascades_run_history`,
  `migration_falls_back_to_legacy_expression`, `job_type_from_sql_*`). They must
  all continue to pass, proving WAL + the migration gate did not break
  initialization, cascade, or legacy fallback.
- Optional (nice to have): a test that calls `with_connection` twice on the same
  config and asserts both succeed — this exercises the migration gate's
  already-migrated branch. Structural pattern: `job_type_from_sql_reads_valid_value`.
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0
- [ ] `grep -n "journal_mode=WAL" src/cron/store.rs` returns a match in `with_connection`
- [ ] `grep -n "busy_timeout=5000" src/cron/store.rs` returns a match
- [ ] `grep -n "unchecked_transaction" src/cron/store.rs` returns **no** match
      (the DEFERRED call was replaced with `Transaction::new_unchecked` IMMEDIATE)
- [ ] `grep -n "migrate_cron_columns_once" src/cron/store.rs` shows the helper
      is defined and called exactly once inside `with_connection`
- [ ] No files outside `src/cron/store.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `with_connection` or `record_run` do not match the "Current state" excerpts.
- `rusqlite::Transaction::new_unchecked` does not compile in the pinned rusqlite
  version — report it.
- Enabling WAL causes an existing `cron` test to fail (e.g. a test that reads
  the DB from a second connection and now sees a lock) — report the failing test
  rather than reverting only part of the change.
- The fix appears to require touching any file other than `src/cron/store.rs`.

## Maintenance notes

- `plans/167` also edits `src/cron/store.rs` (`update_job`). If 167 landed first,
  `update_job` will already use `Transaction::new_unchecked(..., Immediate)`;
  that is unrelated to this plan's `record_run` change. Re-run the drift check.
- The per-path migration `HashSet` is process-local. A separate CLI process still
  runs migration once for its own process — correct, since each process opens its
  own connection.
- Reviewer should scrutinize: the migration gate is keyed on the full `db_path`
  (not a bare process-wide `Once`), so a process that opens two different cron
  DBs still migrates both.
- Deferred: holding a single long-lived cron connection (like `SessionStore`
  does) would remove per-call schema/pragma work entirely, but that is a larger
  refactor and out of scope here.
