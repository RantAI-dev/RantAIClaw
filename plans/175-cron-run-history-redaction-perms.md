# Plan 175: Redact cron run history, lock down the jobs.db file mode, and stop echoing full commands

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/store.rs src/cron/scheduler.rs src/gateway/cron_api.rs src/security/mod.rs src/channels/whatsapp_storage.rs src/agent/loop_.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

Cron run history stores raw command output and is world-readable. `record_run`
(`src/cron/store.rs:304-332`) truncates via `truncate_cron_output` but applies
**no redaction** (a `redact` helper exists at `src/security/mod.rs:55`, unused
here). Shell results are stored as full stdout/stderr
(`src/cron/scheduler.rs:552-559`), and policy-refusal messages echo the **entire
command line** into stored output (`src/cron/scheduler.rs:504-511`). The
database is opened with `Connection::open` and **no `set_permissions`**
(`src/cron/store.rs:513-521`), so `jobs.db` lands at the process umask (commonly
0644) — contrast `src/channels/whatsapp_storage.rs:28-43`, which forces 0600 for
its credential store. Finally, `GET /api/v1/cron/{id}/runs`
(`src/gateway/cron_api.rs:131-145`) and `GET /api/v1/cron` return the stored
output/command/prompt verbatim to any pairing-token holder. Net effect: secrets
that pass through a scheduled command's environment or output are persisted in
cleartext, in a world-readable file, and served back over the API. This plan
scrubs stored output, stops echoing full command lines in refusals, and
restricts the DB file mode.

## Current state

Files and roles:

- `src/cron/store.rs` — `record_run` (304-332): `let bounded_output =
  output.map(truncate_cron_output);` then INSERT — **no redaction**.
  `with_connection` (513-521): `Connection::open(&db_path)` with no
  `set_permissions`; the DB path is
  `config.workspace_dir.join("cron").join("jobs.db")` (line 514).
- `src/cron/scheduler.rs` — shell result assembled at 552-559 (`combined =
  format!("status={}\nstdout:\n{}\nstderr:\n{}", ...)`); refusal messages echo
  `job.command` at 504-511 (`"blocked by security policy: command not allowed:
  {job.command}"`).
- `src/agent/loop_.rs:48-87` — `SENSITIVE_KV_REGEX` (48-50) + `fn
  scrub_credentials(input: &str) -> String` (55): pattern-based masking of
  generic `key=value` / `key:value` secrets, keeping a 4-char prefix and
  replacing the rest with `*[REDACTED]`. Currently **private** — Step 1 exports
  it `pub(crate)` and reuses it. This is the adopted scrubber.
- `src/providers/mod.rs:773` — `pub fn scrub_secret_patterns` masks only known
  token prefixes (`sk-`, `ghp_`, …), NOT generic `key=value`; **not** used here
  (would miss env secrets). Listed only to rule it out.
- `src/security/mod.rs:53-61` — `pub fn redact(value: &str) -> String` (shows
  first 4 chars + `***`). This is a coarse whole-value redactor; **not** used
  here (would mangle legitimate output).
- `src/channels/whatsapp_storage.rs:28-43` — `restrict_file_permissions(path)`:
  the 0600 best-effort pattern (Unix `PermissionsExt` + `from_mode(0o600)`,
  logged-not-fatal on error, no-op on non-Unix) to copy.
- `src/gateway/cron_api.rs:131-145` — `list_cron_runs` returns `runs` verbatim.

Key excerpts:

`src/cron/store.rs:312-313` (no redaction before persist):
```rust
) -> Result<()> {
    let bounded_output = output.map(truncate_cron_output);
```

`src/cron/scheduler.rs:504-511` (full command echoed into stored output):
```rust
if !security.is_command_allowed(&job.command) {
    return (
        false,
        format!(
            "blocked by security policy: command not allowed: {}",
            job.command
        ),
    );
}
```

`src/cron/store.rs:520-521` (DB opened at umask):
```rust
let conn = Connection::open(&db_path)
    .with_context(|| format!("Failed to open cron DB: {}", db_path.display()))?;
```

`src/channels/whatsapp_storage.rs:28-43` (the 0600 pattern to reuse):
```rust
fn restrict_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("... could not restrict {} to 0600: {e}", path.display());
        }
    }
    #[cfg(not(unix))] { let _ = path; }
}
```

Repo conventions & security notes:
- CLAUDE.md §3.6: "Never log secrets, raw tokens, or sensitive payloads." Stored
  run history is a payload sink — the same rule applies.
- The agent path already has a scrub heuristic — **reuse it, do not reinvent.**
  This plan commits to a specific target (Step 1): `scrub_credentials`
  (`src/agent/loop_.rs:55`), which masks generic `key=value` / `key:value`
  secrets (token, api_key, password, secret, user_key, bearer, credential) via
  `SENSITIVE_KV_REGEX`. It is currently private; Step 1 exports it `pub(crate)`.
  The alternative — `scrub_secret_patterns` (`src/providers/mod.rs:773`, already
  `pub`) — masks **only** known token prefixes (`sk-`, `ghp_`, `xoxb-`, …), NOT
  generic `key=value`, so it would miss an env secret like `DB_PASSWORD=...`;
  that is why this plan uses `scrub_credentials` instead.
- File-permission hardening is best-effort and Unix-gated, logged not fatal
  (whatsapp pattern). Restrict the `-wal` / `-shm` siblings too when present.
- **Coordinate with plan 179**: 179 also edits `with_connection`
  (`src/cron/store.rs:513-573`) — WAL/`busy_timeout`/migration — while Step 3
  here adds the chmod in the same function. The regions are disjoint (low
  merge risk), but whichever lands second should rebase over the first.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Find scrubber | `grep -rn "fn scrub\|fn redact\|fn sanitize" src/security src/agent src/memory src/providers` | confirms `scrub_credentials` (`src/agent/loop_.rs`) and `scrub_secret_patterns` (`src/providers/mod.rs`) — this plan uses the former (see Step 1) |
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests   | `cargo test --lib cron` | all pass |
| Tests   | `cargo test --lib security` | all pass |
| Drift   | `git diff --stat 2aefb9f..HEAD -- src/cron` | only your changes |

Do **not** run a bare `cargo test` (disk-constrained). Scope with `--lib`.

## Scope

**In scope**:
- `src/cron/store.rs` — scrub output in `record_run`; chmod the DB (+siblings)
  in `with_connection`
- `src/cron/scheduler.rs` — replace the full-command echo in the two refusal
  messages (504-511 and any sibling that echoes `job.command`) with job id +
  offending basename only
- `src/agent/loop_.rs` — change `fn scrub_credentials` (line 55) to
  `pub(crate) fn scrub_credentials` so `src/cron/store.rs` can reuse it. This is
  a visibility-only edit (no behavior change) to the scrubber this plan adopts;
  do not otherwise touch this file.
- (reference-only) `src/security/mod.rs` — `redact` is a whole-value redactor,
  not used here; do not weaken it.

**Out of scope**:
- The API returning stored fields verbatim — restricting the DB + scrubbing at
  write time is the fix; do not change the response shape (clients depend on it).
  If a reviewer wants API-side masking, that is a separate follow-up.
- The HTTP/CLI risk-gate parity — that is plan 174.
- Adding a `created_by` column — that is plan 172.

## Git workflow

- Branch: `advisor/175-cron-run-history-redaction-perms`
- Conventional commits, one per step
  (e.g. `fix(cron): scrub secrets from stored run output`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Scrub output before persisting run history

This plan **reuses `scrub_credentials`** (`src/agent/loop_.rs:55`) — the
pattern-based scrubber that masks generic `key=value` / `key:value` secrets via
`SENSITIVE_KV_REGEX`. Do **not** invent a bespoke regex list, and do **not** use
`src/security/mod.rs:55 redact` (a whole-value redactor that would mangle
legitimate output). The prefix-only `scrub_secret_patterns`
(`src/providers/mod.rs:773`) is rejected because it misses generic env secrets
like `DB_PASSWORD=...`.

1. Confirm `scrub_credentials` still matches the "Current state" excerpt (run the
   `grep` from the commands table), then change its declaration in
   `src/agent/loop_.rs` from `fn scrub_credentials(input: &str) -> String` to
   `pub(crate) fn scrub_credentials(input: &str) -> String`. This is a
   visibility-only change — do not alter its body or the `SENSITIVE_KV_REGEX`
   pattern. Reach it from the cron store via `crate::agent::loop_::scrub_credentials`
   (confirm the module path; adjust if `loop_` is not the public module name).
2. In `record_run` (`src/cron/store.rs:312-313`), route `output` through the
   scrubber **before** `truncate_cron_output`:
   `let bounded_output = output.map(|o| truncate_cron_output(&crate::agent::loop_::scrub_credentials(&o)));`
   (adapt the import path to how `src/cron/store.rs` reaches sibling modules).

**Verify**: `cargo test --lib cron` → pass. Add a test: a run whose output
contains a `key=value`-shaped secret matched by `SENSITIVE_KV_REGEX` (e.g.
`api_key=` or `password=` followed by a neutral 8+ char dummy placeholder —
never a real credential) is stored with the value masked (assert the raw dummy
value is absent from the stored `output`, and the `*[REDACTED]` marker is
present). Note the scrubber matches `key=value`/`key:value` forms, not bare
tokens, so shape the test input accordingly.

### Step 2: Stop echoing the full command in refusal messages

In `src/cron/scheduler.rs`, the refusal at 504-511 echoes `job.command`. Replace
it with the job id plus the offending command's **basename** only, e.g.:
```rust
format!(
    "blocked by security policy: command not allowed (job {}, program `{}`)",
    job.id,
    // first whitespace-token, basename only
    program_basename(&job.command),
)
```
Add a tiny local `program_basename(cmd: &str) -> &str` (or `String`) that takes
the first token and strips any path prefix. Apply the same treatment to any
sibling refusal in this function that embeds the full `job.command` (check the
`is_command_allowed` echo only — the `validate_command_execution` reason at
518-519 is command-free for the risk branches, so leave it, but confirm by
reading it).

**Verify**: `cargo test --lib cron` → the existing
`run_job_command_blocks_disallowed_command` test (`src/cron/scheduler.rs:730-742`)
asserts `output.contains("command not allowed")` — that substring survives your
change, so the test still passes. Confirm it does; if your wording drops
"command not allowed", update the assertion in the same commit and note it.

### Step 3: Restrict the jobs.db file mode to 0600

In `src/cron/store.rs` `with_connection` (513-521), after
`std::fs::create_dir_all(parent)` and after `Connection::open` succeeds,
best-effort chmod the DB file to 0600 following the whatsapp pattern. Also
restrict the `-wal` and `-shm` sibling files if they exist (SQLite creates them
in WAL mode). Guard the whole block with `#[cfg(unix)]` and log-not-fatal on
error; no-op on non-Unix. Do the chmod on every `with_connection` call is
acceptable (idempotent), but prefer doing it once right after open.

**Verify**: `cargo test --lib cron` → pass. On a Unix box, add a test that
after a `record_run`, `jobs.db`'s mode masks to `0o600` (query via
`std::fs::metadata(...).permissions().mode() & 0o777`).

## Test plan

- `src/cron/store.rs` tests: (a) a secret-shaped substring in run output is
  masked in the stored row; (b) [Unix] `jobs.db` mode is `0o600` after a write.
- `src/cron/scheduler.rs` tests: the disallowed-command refusal no longer
  contains the full command line but still contains "command not allowed" (or
  the updated assertion).
- Model the store tests after the existing cron store tests; model the scheduler
  assertion after `run_job_command_blocks_disallowed_command`.
- Verification: `cargo test --lib cron` and `cargo test --lib security` → pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` and `cargo test --lib security` pass; new tests exist
- [ ] `record_run` scrubs output before persisting
- [ ] Refusal messages no longer embed the full `job.command`
- [ ] [Unix] `jobs.db` (and any `-wal`/`-shm`) is created/restricted to 0600
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report if:

- The "Current state" excerpts do not match live code (drift since 2aefb9f) —
  in particular, if `scrub_credentials`/`SENSITIVE_KV_REGEX` at
  `src/agent/loop_.rs:48-87` no longer matches the excerpt, or the module can no
  longer be reached from `src/cron/store.rs` with a `pub(crate)` export, STOP and
  report rather than inventing a bespoke regex list.
- Chmod cannot be applied without racing the SQLite WAL sibling creation —
  report the ordering issue rather than half-fixing it.
- A verification fails twice after a reasonable fix attempt.

## REMEDIATION (must be in the PR description)

Any existing `jobs.db` written before this fix may already contain cleartext
secrets and be world-readable. The PR must instruct operators to **treat any
existing `<workspace>/cron/jobs.db` as secret-bearing** and to **rotate any
credential that could have appeared in a scheduled command's output or
environment** (the exact credential type depends on the operator's jobs — API
keys, tokens, DB passwords). Chmod on next open does not undo prior exposure.

## Maintenance notes

- A reviewer should scrutinize: that the scrubber is the *shared* one (no
  drift), that the basename helper cannot itself leak a path, and that the chmod
  is best-effort (never blocks the scheduler from starting).
- Deferred: API-side masking of stored fields in `list_cron`/`list_cron_runs`
  responses — out of scope here; write-time scrubbing + file-mode restriction is
  the primary fix.
