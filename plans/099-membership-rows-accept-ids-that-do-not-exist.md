# Plan 099: Validate group and document before creating a membership row

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/store/sqlite/groups.rs src/kb/axi/api.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

A document can be attached to a group that does not exist, or a group can be
given a document that does not exist. Both return success.

`PRAGMA foreign_keys` is off — the code says so in three places
(`groups.rs:8-11`, `groups.rs:183-184`, `intelligence.rs:411-413`) — so the
declared cascade on `document_group` (`schema.rs:107-108`) never fires and the
declared references are never checked.

`add_document_to_group_impl` (`groups.rs:200-220`) inserts blind:

```rust
            conn.execute(
                "INSERT OR IGNORE INTO document_group (document_id, group_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![document_id, group_id, now],
            )?;
```

Two routes reach it without checking anything:

- `POST /api/v1/kb/groups/{id}/documents` — `api.rs:1157-1173`. It validates
  that `document_id` is non-empty and nothing else, then returns
  `{"ok": true}`.
- Ingest with a `groups` form field — `api.rs:1399-1404`.

Consequences that compound with the unfiltered count in plan 100:
`list_groups.document_count` is a bare `COUNT(*)` over `document_group`
(`groups.rs:76`), so `{"document_id": "anything"}` inflates a knowledge base's
document count on demand, while `list_group_documents` (which joins `document`)
shows nothing.

The realistic non-adversarial path is a race: the console links a document at
ingest (`kb-panel.tsx:576`) using a group id read at page load. Delete that
group in another tab and the upload still returns 200 with the document
orphaned.

Note the inconsistency this creates inside one file: `get_group` and
`update_group` both 404 correctly on a missing id (`api.rs:1096-1099`,
`:1125-1128`), while the membership route does not.

## Current state (verified at 2ca7e59)

- No validation in either the handler or the store method
- `store_sqlite_test.rs:402 group_lifecycle_…` covers only valid ids
- No test attaches a bogus id — the behaviour is unpinned and free to fix

## Scope

**In scope**: reject membership rows whose group or document does not exist,
on both routes.

**Out of scope**: turning on `PRAGMA foreign_keys` globally. That changes
delete semantics across the whole schema and needs its own plan; explicit
checks are the smaller, reversible fix.

## Git workflow

```bash
git switch -c fix/validate-group-membership-ids
```

## Steps

### Step 1: Validate inside the store method

Do it in `add_document_to_group_impl`, not only in the handler, so both routes
and any future caller are covered by one check. Same transaction, so it cannot
race with a concurrent delete:

```rust
        tokio::task::spawn_blocking(move || -> KbResult<()> {
            let mut conn = conn.blocking_lock();
            let tx = conn.transaction()?;
            // PRAGMA foreign_keys is off (see module docs), so the declared
            // references are not enforced. Check explicitly — an unchecked
            // INSERT OR IGNORE creates a membership row pointing at nothing,
            // which inflates `document_count` while `list_group_documents`
            // shows nothing.
            let group_ok: i64 = tx.query_row(
                "SELECT COUNT(*) FROM knowledge_base_group WHERE id = ?1",
                params![group_id], |r| r.get(0))?;
            if group_ok == 0 {
                return Err(KbError::NotFound(format!("group {group_id}")));
            }
            let doc_ok: i64 = tx.query_row(
                "SELECT COUNT(*) FROM document WHERE id = ?1 AND deleted_at IS NULL",
                params![document_id], |r| r.get(0))?;
            if doc_ok == 0 {
                return Err(KbError::NotFound(format!("document {document_id}")));
            }
            tx.execute(
                "INSERT OR IGNORE INTO document_group (document_id, group_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![document_id, group_id, now],
            )?;
            tx.commit()?;
            Ok(())
        })
```

Excluding soft-deleted documents is deliberate: attaching a deleted document
would produce a membership row that no listing can ever show.

**Verify**: `KbError::NotFound` maps to HTTP 404 (`api.rs:328-334`), so the
route's response shape follows automatically.

### Step 2: Keep ingest's behaviour sane

Ingest calls this per group id at `api.rs:1399-1404` **after** the document is
stored. With Step 1, a bad group id now fails the whole request while the
document is already persisted — a worse outcome than today.

Validate the group ids **before** the expensive work instead. Add a check
immediately after the multipart parse (around `api.rs:1267`), before staging
and embedding, so a typo costs nothing:

```rust
    for group_id in &groups {
        if ctx.store.get_group(group_id).await.map_err(ApiError::from)?.is_none() {
            return Err(ApiError::not_found(format!("group {group_id} not found")).into());
        }
    }
```

Then leave the post-store attach loop as-is; it can no longer fail on a missing
group.

**Verify**: uploading with a bogus `groups` value returns 404 and stores
nothing — check `kb list` is unchanged afterwards.

### Step 3: Tests

In `store_sqlite_test.rs`:

- attaching to a nonexistent group returns `NotFound`
- attaching a nonexistent document returns `NotFound`
- the existing `group_lifecycle_…` still passes unchanged (that is the control)

In `api_test.rs`:

- `POST /kb/groups/{bogus}/documents` returns 404
- ingest with a bogus `groups` field returns 404 and leaves the document count
  at zero

**Verify**: each new test is red before its corresponding step.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb store_sqlite_test
cargo test --features kb --test kb api_test
```

## Done criteria

- Neither route can create a membership row for an id that does not exist.
- A rejected ingest leaves no document behind.
- `group_lifecycle_…` passes untouched.

## STOP conditions

- The extra `SELECT`s show up as a measurable cost on a bulk attach path that
  this plan did not find — report it rather than dropping the check.
