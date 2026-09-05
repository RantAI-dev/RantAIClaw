# Plan 195: Render `at`/`every` schedules on the TUI and web console (not just the CLI)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index. This plan touches TWO repos: the Rust repo and the
> separate `claw-ui` Next.js repo. Scope and verify each half independently.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/tui/commands/cron.rs src/tui/app.rs src/cron/types.rs`
> (Rust) and inspect `claw-ui/src/components/ops/cron-panel.tsx`. If any in-scope
> file changed since this plan was written, compare the "Current state" excerpts
> against the live code before proceeding; on a mismatch, treat it as a STOP
> condition.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

Jobs scheduled with `at` (one-shot) or `every` (interval) render with a **blank**
schedule everywhere except the CLI. The stored `expression` column is only
populated for `cron`-kind jobs — `schedule_cron_expression` (`src/cron/schedule.rs:63-68`)
returns `None` for `At`/`Every`, and the store writes `unwrap_or_default()` → `""`
(`src/cron/store.rs:40` and `:82`). The CLI was fixed by commit `90a5c36` to render
via the `Display for Schedule` impl (`src/cron/types.rs:77-88`, reached through
`src/cron/mod.rs:44`). But five TUI sites and the web console still print the raw
empty `expression`:

- TUI `/cron list` — `src/tui/commands/cron.rs:104` prints `j.expression` → blank.
- TUI `/cron add` confirmation ("Expr: {}") — `src/tui/commands/cron.rs:146` prints
  `job.expression` → blank for at/every (same symptom).
- TUI `/cron edit` confirmation ("Expr: {}") — `src/tui/commands/cron.rs:182` prints
  `job.expression` → blank for at/every (same symptom).
- TUI jobs picker secondary line — `src/tui/commands/cron.rs:216` → blank
  (live-confirmed: an every/at job shows a blank schedule column in the picker).
- TUI detail panel "Schedule" row — `src/tui/app.rs:3411` → blank
  (live-confirmed: blank Schedule row for at/every jobs).
- Web console — `claw-ui/src/components/ops/cron-panel.tsx:340` renders
  `{j.expression || j.schedule.kind}` → the bare word `"at"`/`"every"` with no time
  (live-confirmed).

The fix is to render from the structured `schedule` (which is always populated),
not the `expression` string. The Rust `Display` impl already exists and is tested;
the TUI sites just need to use `j.schedule` instead of `j.expression`, and the web
panel needs a small TS formatter mirroring that `Display`.

## Current state

### Rust — the `Display` impl already exists (`src/cron/types.rs:77-88`)

```rust
impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cron { expr, tz } => match tz {
                Some(tz) => write!(f, "{expr} ({tz})"),
                None => write!(f, "{expr}"),
            },
            Self::At { at } => write!(f, "at {}", at.to_rfc3339()),
            Self::Every { every_ms } => write!(f, "every {every_ms}ms"),
        }
    }
}
```

There is already a test `schedule_display_is_clean_per_variant` (`types.rs:180-204`)
asserting non-empty Display for cron/at/every — so the Rust `Display` half is done;
you are only redirecting the render sites to use it.

`CronJob` carries both `pub expression: String` and `pub schedule: Schedule`
(`types.rs:117-136`). `Schedule` is `PartialEq`/`Display`.

### TUI site 1 — `src/tui/commands/cron.rs:99-108` (`list_text`)

```rust
                let _ = write!(
                    out,
                    "  {} [{}] {} · next {} · {}\n    {}\n",
                    name,
                    if j.enabled { "on" } else { "paused" },
                    j.expression,                              // ← line 104, blank for at/every
                    j.next_run.to_rfc3339(),
                    j.last_status.as_deref().unwrap_or("never run"),
                    what,
                );
```

### TUI site 2 — `src/tui/commands/cron.rs:214-219` (`build_cron_picker` secondary)

```rust
                secondary: format!(
                    "{} · next {} · {}",
                    j.expression,                              // ← line 216, blank for at/every
                    j.next_run.to_rfc3339(),
                    j.last_status.as_deref().unwrap_or("never run")
                ),
```

`crate::cron::{..., Schedule, ...}` is already imported at the top of this file
(line 7), and `Schedule` implements `Display`, so `j.schedule` formats directly
in a `{}` slot.

### TUI site 3 — `src/tui/app.rs:3411` (`build_cron_detail_panel`)

```rust
            .status_with(StatusKind::Info, "Schedule", job.expression.clone())   // ← line 3411
```

`job` here is a `crate::cron::CronJob` (`crate::cron::get_job(...)`), so
`job.schedule` is in scope and `job.schedule.to_string()` yields the Display string.

### claw-ui — `src/components/ops/cron-panel.tsx:340`

```tsx
                  {j.expression || j.schedule.kind} · next {fmtWhen(j.next_run)}
```

TS types (`claw-ui/src/lib/types.ts:131-134`):

```ts
export type CronSchedule =
  | { kind: "cron"; expr: string; tz?: string | null }
  | { kind: "at"; at: string }
  | { kind: "every"; every_ms: number };
```

A `fmtWhen(ts)` timestamp helper already exists at `cron-panel.tsx:21-30`
(`new Date(ms).toLocaleString()`).

## Commands you will need

| Purpose        | Command                                                                     | Expected on success |
|----------------|-----------------------------------------------------------------------------|---------------------|
| Rust format    | `cargo fmt --all -- --check`                                                | exit 0, no diff     |
| Rust lint      | `cargo clippy --all-targets -- -D warnings`                                 | exit 0, no warnings |
| Rust tests     | `cargo test --lib cron`                                                     | all pass            |
| claw-ui build  | `cd /home/sulthannauval/project/rantai/claw-ui && ./node_modules/.bin/next build` | build succeeds |

Do NOT run a bare `cargo test` (disk-constrained box). claw-ui has no eslint config —
verify via `next build` + a browser check.

## Scope

**In scope (Rust):**
- `src/tui/commands/cron.rs` — sites 1 and 2 (`j.expression` → `j.schedule`), plus
  the `/cron add` (`:146`) and `/cron edit` (`:182`) "Expr:" confirmations
  (`job.expression` → `job.schedule`; same blank-for-at/every symptom).
- `src/tui/app.rs` — site 3 (`job.expression.clone()` → `job.schedule.to_string()`).
- `src/cron/types.rs` — a small assertion test (the Display impl already exists).

**In scope (claw-ui — SEPARATE repo, its own build/release):**
- `src/components/ops/cron-panel.tsx` — line 340: replace the `.kind` fallback with
  a `formatSchedule(schedule)` helper mirroring the Rust `Display`.

**Out of scope:**
- The stored `expression` column and `schedule_cron_expression` — leaving `""` for
  at/every is fine; do NOT change the store or add a computed column. Rendering,
  not storage, is the fix.
- The CLI (`src/cron/mod.rs`) — already correct via `90a5c36`; do not touch.
- Any change to the `Schedule` `Display` format — it is tested; keep it stable so
  the TS mirror stays in sync.

## Git workflow

- Rust branch: `advisor/195-cron-schedule-render-parity`.
- claw-ui: commit in the claw-ui repo separately (it has its own git history and release cadence).
- Conventional commits, e.g. `fix(tui): render at/every cron schedules via Display, not the empty expression string`.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1 (Rust): TUI list — use `j.schedule`

`src/tui/commands/cron.rs`, in `list_text` (line 104), change the `j.expression`
argument in the `write!` to `j.schedule` (it Displays directly):

```rust
                    j.schedule,   // was: j.expression
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2 (Rust): TUI picker — use `j.schedule`

Same file, `build_cron_picker` secondary (line 217), change `j.expression` to
`j.schedule`:

```rust
                secondary: format!(
                    "{} · next {} · {}",
                    j.schedule,   // was: j.expression
                    j.next_run.to_rfc3339(),
                    j.last_status.as_deref().unwrap_or("never run")
                ),
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2b (Rust): TUI add/edit confirmations — use `job.schedule`

Same file (`src/tui/commands/cron.rs`). The `/cron add` confirmation in `add_text`
(line 146) and the `/cron edit` confirmation in `edit_text` (line 182) both print
`job.expression` in their `"…  Expr: {}\n  Next: {}"` `format!` — blank for at/every,
the identical symptom. `job` here is the `CronJob` returned by `add_shell_job`/
`add_agent_job`/`update_job`, so `job.schedule` is in scope and Displays directly.
Change the `job.expression` argument to `job.schedule` in both confirmations:

```rust
        Ok(job) => format!(
            "✅ Added cron job {}\n  Expr: {}\n  Next: {}",
            job.id,
            job.schedule,   // was: job.expression
            job.next_run.to_rfc3339()
        ),
```

(and the mirroring `"✅ Updated cron job …"` arm in `edit_text`).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3 (Rust): TUI detail panel — use `job.schedule.to_string()`

`src/tui/app.rs:3411`, change:

```rust
            .status_with(StatusKind::Info, "Schedule", job.expression.clone())
```

to:

```rust
            .status_with(StatusKind::Info, "Schedule", job.schedule.to_string())
```

(`status_with` takes an owned `String` here — `job.schedule.to_string()` provides it.
Confirm the signature accepts `String`; if it needs `impl Into<String>`, `.to_string()`
still satisfies it.)

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4 (Rust): add an assertion test

The Display impl already has `schedule_display_is_clean_per_variant` in
`src/cron/types.rs`. Add a focused non-empty assertion so the regression (blank
at/every) is explicitly pinned:

```rust
    #[test]
    fn schedule_display_is_non_empty_for_at_and_every() {
        use super::Schedule;
        use chrono::Utc;
        let at = Schedule::At { at: Utc::now() };
        let every = Schedule::Every { every_ms: 60_000 };
        assert!(!at.to_string().is_empty());
        assert!(!every.to_string().is_empty());
        assert!(at.to_string().starts_with("at "));
        assert!(every.to_string().starts_with("every "));
    }
```

**Verify**: `cargo test --lib cron` → all pass, including the new test.

### Step 5 (claw-ui): format the schedule instead of falling back to `.kind`

In `claw-ui/src/components/ops/cron-panel.tsx`, add a helper near `fmtWhen`
(after line 30) that mirrors the Rust `Display`:

```tsx
function formatSchedule(s: CronSchedule): string {
  switch (s.kind) {
    case "cron":
      return s.tz ? `${s.expr} (${s.tz})` : s.expr;
    case "at":
      return `at ${fmtWhen(s.at)}`;
    case "every": {
      const mins = s.every_ms / 60000;
      // Prefer a friendly "every N min" when it divides cleanly; fall back to ms.
      return Number.isInteger(mins) && mins >= 1
        ? `every ${mins} min`
        : `every ${s.every_ms}ms`;
    }
  }
}
```

(`CronSchedule` is already imported at line 6.) Then change line 340 from
`{j.expression || j.schedule.kind}` to `{formatSchedule(j.schedule)}`:

```tsx
                  {formatSchedule(j.schedule)} · next {fmtWhen(j.next_run)}
```

Note: the `every` display here uses friendly minutes rather than the Rust `Nms`
form, matching the create-form's minute-based input (`cron-panel.tsx:72`). This is
an intentional UI-side nicety; the cron/at forms match the Rust text exactly.

**Verify (claw-ui)**:
```bash
cd /home/sulthannauval/project/rantai/claw-ui && ./node_modules/.bin/next build
```
→ build succeeds (no TS errors — the `switch` is exhaustive over the three
`CronSchedule` variants). Then load the console, create an `every` and an `at` job,
and confirm the Schedules panel shows `every 5 min` / `at <local time>` instead of
a bare `every`/`at`.

## Test plan

- Rust: the new `schedule_display_is_non_empty_for_at_and_every` test plus the
  existing `schedule_display_is_clean_per_variant`. The TUI sites are verified by
  the render path (they now pass `Schedule` into `{}` / `to_string()`); confirm by
  reading the diff that no site still references `j.expression`/`job.expression` for
  the schedule column.
- claw-ui: `next build` passes; manual browser check that at/every jobs render a
  non-blank schedule.

Verification: `cargo test --lib cron` → all pass; `next build` → success.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the new test exists and passes
- [ ] `rg -n "j\.expression|job\.expression" src/tui/commands/cron.rs src/tui/app.rs`
      returns no matches for the schedule render (all five TUI sites now use `.schedule`)
- [ ] (claw-ui) `next build` succeeds; `rg -n "j.schedule.kind" src/components/ops/cron-panel.tsx` returns nothing
- [ ] (claw-ui) browser check: an `every`/`at` job shows a non-blank schedule
- [ ] No files outside the in-scope list are modified (`git status` in each repo)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The cited lines do not match the "Current state" excerpts (drift — e.g. a later
  commit already switched a site to `j.schedule`).
- `status_with` at `app.rs:3411` does not accept the `String` from `.to_string()` —
  report the actual signature rather than casting around it.
- The claw-ui `formatSchedule` switch produces a TS "not all code paths return"
  error — that means `CronSchedule` gained a variant; STOP and report (the Rust
  `Display` would need a matching arm too).
- `cargo test --lib cron` or `next build` fails twice after a reasonable fix attempt.

## Maintenance notes

- **Keep the two in sync**: `formatSchedule` (TS) mirrors `Display for Schedule`
  (Rust). If either the Rust `Display` format or the `CronSchedule` variant set
  changes, update the other. The `every` rendering intentionally differs (friendly
  minutes in the UI vs `Nms` in Rust) — that is the one deliberate divergence.
- **Reviewer scrutiny**: confirm no site still reads the empty `expression` for the
  schedule label, and that the stored `expression` column is untouched (this is a
  render-only fix).
- **Deferred**: populating `expression` for at/every at the store layer was
  considered and rejected — it would duplicate state that `schedule` already holds
  and risk the two drifting. Render from `schedule`, keep `expression` cron-only.
