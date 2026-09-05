# Plan 162: Fix cron weekday off-by-one so `* * * * 1` means Monday, not Sunday

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/schedule.rs src/lib.rs src/main.rs docs/pillars/7-gateway-daemon.md docs/operations/auto-update.md CHANGELOG.md`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P0
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

This repo accepts **standard 5-field crontab** expressions (its own help text
and docs say so), but the `cron` crate it delegates to numbers weekdays the
**Quartz** way: `Sunday=1 … Saturday=7`. Standard crontab is `Sunday=0 …
Saturday=6` (with `7` also = Sunday). `normalize_expression` never remaps the
weekday field, so every weekday cron job is silently off by one day, and any
job that uses crontab-Sunday (`0`) is hard-rejected.

Live-reproduced by running the binary:

- `cron add '0 9 * * 1'` (crontab **Monday**) scheduled its next run on a
  **Sunday** (2026-08-23) — the crate read `1` as Sunday.
- `cron add '0 4 * * 0'` (crontab **Sunday**) was **rejected** — the crate's
  inclusive minimum for the weekday field is `1`, so `0` is out of range.

The repo's own examples are therefore wrong under its own engine: `0 9 * * 1-5`
is labeled "Mon–Fri" but fires Sun–Thu, and `0 4 * * 0` (documented "Sunday
4 AM") cannot even be created. After this plan, a 5-field expression means what
a crontab user expects, and the help/docs examples are correct.

## Current state

### The bug — `src/cron/schedule.rs:70-83`

`normalize_expression` takes a 5-field crontab string and just prepends a
seconds field, with **no weekday translation**:

```rust
pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let field_count = expression.split_whitespace().count();

    match field_count {
        // standard crontab syntax: minute hour day month weekday
        5 => Ok(format!("0 {expression}")),
        // crate-native syntax includes seconds (+ optional year)
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!(
            "Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {field_count})"
        ),
    }
}
```

`normalize_expression` is called on **every evaluation** (from
`next_run_for_schedule` at `schedule.rs:10` and `validate_schedule` at
`schedule.rs:44`). The raw crontab string is what is stored in the DB; the
translation happens at evaluation time. That is why fixing this function
immediately corrects both new and already-stored jobs — see **BACKWARD-COMPAT**
below.

### The crate — `Cargo.toml:127`

```
cron = "0.15"
```

Confirmed reproduction of this crate's weekday numbering (Quartz): the DOW
field is `1..=7` inclusive with **`Sunday=1`, `Monday=2`, … `Saturday=7`**.
`0` is below the minimum and rejected. Day **names** (`sun`, `mon`, …) are
mapped by the crate to those same Quartz ordinals, so `mon` already means the
real Monday — **names need no remap; only numeric ordinals are wrong.** You
will verify this with a test before trusting it (Step 2).

### The required numeric remap (crontab → crate)

| crontab (input) | weekday | crate ordinal (output) |
|-----------------|---------|------------------------|
| 0               | Sunday  | 1                      |
| 1               | Monday  | 2                      |
| 2               | Tuesday | 3                      |
| 3               | Wednesday | 4                    |
| 4               | Thursday | 5                      |
| 5               | Friday  | 6                      |
| 6               | Saturday | 7                     |
| 7               | Sunday  | 1                      |

Rule: for a numeric ordinal `n`, output `if n == 7 { 1 } else { n + 1 }`. Only
`0..=7` are valid crontab weekday ordinals; anything else in the weekday field
should be left untouched (it is either a name, `*`, or already invalid and the
crate will reject it).

### Wrong examples that must be corrected

- `src/lib.rs:286` (inside the `cron add` `long_about`):
  `rantaiclaw cron add '0 9 * * 1-5' 'Good morning' --tz America/New_York`
- `src/main.rs:444` (inside the `Cron` command `long_about`):
  `rantaiclaw cron add '0 9 * * 1-5' 'Good morning' --tz America/New_York`
- `docs/pillars/7-gateway-daemon.md:73`:
  `rantaiclaw cron add '0 9 * * 1-5' 'Good morning' --tz America/New_York`
- `docs/operations/auto-update.md:49`:
  `rantaiclaw cron add '0 4 * * 0' 'rantaiclaw update --yes'` (labeled "Weekly
  auto-pull, Sunday 4 AM")
- `docs/operations/auto-update.md:53`:
  `rantaiclaw cron add '0 4 * * 0' 'rantaiclaw update --yes --backup'`

These examples are **correct under the FIXED behavior** — `1-5` will be Mon–Fri
and `0` will be Sunday once the remap lands. So the examples themselves do not
change; what changes is that they now do what their labels say. **Do not edit
the doc/help example strings unless the fix does not make them correct** (it
should). Your job in the docs is only to add a one-line note where helpful (see
Step 4) — re-read each example and confirm it is accurate post-fix; if any is
still wrong, fix that specific line.

### Existing test to model after — `src/cron/schedule.rs:104-113`

```rust
#[test]
fn next_run_for_schedule_supports_timezone() {
    let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
    let schedule = Schedule::Cron {
        expr: "0 9 * * *".into(),
        tz: Some("America/Los_Angeles".into()),
    };
    let next = next_run_for_schedule(&schedule, from).unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 17, 0, 0).unwrap());
}
```

The test module (`schedule.rs:85-88`) already has `use super::*;` and
`use chrono::TimeZone;`. Your new weekday tests will also need
`use chrono::Datelike;` (for `.weekday()`).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format check | `cargo fmt --all -- --check` | exit 0, no diff |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| Scoped tests | `cargo test --lib cron::schedule` | all pass, incl. new tests |

Do **NOT** run bare `cargo test` — it builds ~27G and will exhaust the disk.
Keep the test filter scoped to `cron::schedule`.

## Scope

**In scope** (the only files you should modify):
- `src/cron/schedule.rs` — the remap + new tests
- `src/lib.rs` — verify/adjust the example on line 286 only
- `src/main.rs` — verify/adjust the examples on lines 444–448 only
- `docs/pillars/7-gateway-daemon.md` — verify/adjust line 73 + optional note
- `docs/operations/auto-update.md` — verify/adjust lines 49, 53 + optional note
- `CHANGELOG.md` (repo root) — the one-time weekday-shift note required by the
  Done criteria / STOP condition (c)/(d)

**Out of scope** (do NOT touch, even though they look related):
- `src/cron/store.rs`, `src/cron/scheduler.rs` — the raw expression stays
  stored as crontab; only evaluation-time normalization changes.
- The claw-ui repo (`/home/sulthannauval/project/rantai/claw-ui/src/components/ops/cron-panel.tsx`)
  has preset buttons ("Every Monday 9:00" → `0 9 * * 1`, "Weekdays at 9:00" →
  `0 9 * * 1-5`) that will become correct once this Rust fix lands. That is a
  **cross-repo follow-up** — record it in Maintenance notes, do not edit it here.
- Do NOT add a stored-expression migration or version marker unless the STOP
  condition below forces you to escalate the backward-compat decision.

## Git workflow

- Branch: `advisor/162-cron-weekday-quartz-offset`
- Conventional-commit title, e.g.
  `fix(cron): remap crontab weekday ordinals to the cron crate's Quartz numbering`
- Commit the code fix + tests together; the doc/help touch-ups may be the same
  commit or a follow-up commit in the same branch.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the weekday remap to `normalize_expression`

In `src/cron/schedule.rs`, add a private helper that translates the weekday
(5th) field of a 5-field expression from crontab ordinals to crate ordinals,
and call it in the `5 =>` branch **before** prepending the seconds field.

Target shape (adapt naming to match the file):

```rust
/// Translate the weekday field of a standard 5-field crontab expression from
/// crontab numbering (Sunday=0..Saturday=6, with 7=Sunday) to the `cron`
/// crate's Quartz numbering (Sunday=1..Saturday=7). Only numeric ordinals are
/// remapped; `*`, day names (mon,tue,…), and any token that is not a plain
/// crontab ordinal are left untouched — the crate maps names to the same
/// Quartz ordinals already, so `mon` is the real Monday without remapping.
fn remap_weekday_field(field: &str) -> String {
    // Split on ',' to handle lists, then handle ranges/steps within each part.
    field
        .split(',')
        .map(remap_weekday_element)
        .collect::<Vec<_>>()
        .join(",")
}

fn remap_weekday_element(element: &str) -> String {
    // Preserve an optional step suffix "/N": only the range/number before the
    // '/' references weekday ordinals.
    let (base, step) = match element.split_once('/') {
        Some((b, s)) => (b, Some(s)),
        None => (element, None),
    };
    let remapped_base = if let Some((lo, hi)) = base.split_once('-') {
        // Range: remap each endpoint independently.
        format!("{}-{}", remap_weekday_token(lo), remap_weekday_token(hi))
    } else {
        remap_weekday_token(base)
    };
    match step {
        Some(s) => format!("{remapped_base}/{s}"),
        None => remapped_base,
    }
}

/// Remap a single token: a crontab weekday ordinal 0..=7 → crate ordinal.
/// Non-numeric tokens (`*`, names) and out-of-range numbers pass through
/// unchanged so the crate applies its own parsing/validation.
fn remap_weekday_token(token: &str) -> String {
    match token.trim().parse::<u8>() {
        Ok(n @ 0..=7) => {
            let crate_ordinal = if n == 7 { 1 } else { n + 1 };
            crate_ordinal.to_string()
        }
        _ => token.to_string(),
    }
}
```

Then in `normalize_expression`, change the `5 =>` branch so it splits the five
fields, remaps only the 5th, and reassembles before prepending `"0 "`:

```rust
5 => {
    let mut fields: Vec<&str> = expression.split_whitespace().collect();
    // fields = [minute, hour, day-of-month, month, weekday]
    let weekday = remap_weekday_field(fields[4]);
    fields[4] = &weekday; // (if the `&String`→`&str` coercion trips, use
                          //  `fields[4] = weekday.as_str();`, or rebuild the
                          //  string; keep it simple and correct)
    Ok(format!("0 {}", fields.join(" ")))
}
```

Keep the `6 | 7 =>` branch exactly as-is (crate-native, no remap) and the error
branch unchanged.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0, no warnings.

### Step 2: Add table tests proving the weekday semantics

In the `#[cfg(test)] mod tests` block of `src/cron/schedule.rs`, add
`use chrono::Datelike;` and a test that resolves each expression from a fixed
`from` instant and asserts the resulting weekday. Model the structure after
`next_run_for_schedule_supports_timezone`. Use a UTC `from` so no timezone is
involved.

Required assertions (all with `tz: None`):

- `"0 9 * * 1"` → next run is a **Monday** (`.weekday() == chrono::Weekday::Mon`).
- `"0 4 * * 0"` → next run is a **Sunday** (and does **not** error — it used to
  be rejected).
- `"0 0 * * 7"` → next run is a **Sunday** (the `7`→`1` mapping).
- `"0 9 * * 1-5"` → resolve several consecutive next runs and assert each lands
  on Mon–Fri (never Sat/Sun). One robust way: starting from a fixed `from`,
  loop 7 times, each time computing the next run after the previous, and assert
  every result's weekday is in `Mon..=Fri`.
- A **name** sanity check: `"0 9 * * mon"` → next run is a **Monday**. This
  proves the crate maps names to Quartz ordinals so names need no remap. If
  this assertion fails, STOP (see STOP conditions) — the name-passthrough
  assumption is wrong and the remap must also handle names.

Example skeleton for one assertion:

```rust
#[test]
fn crontab_weekday_one_is_monday() {
    use chrono::Datelike;
    let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap(); // a Monday
    let schedule = Schedule::Cron { expr: "0 9 * * 1".into(), tz: None };
    let next = next_run_for_schedule(&schedule, from).unwrap();
    assert_eq!(next.weekday(), chrono::Weekday::Mon, "got {next}");
}
```

Pick `from` instants deliberately (e.g. a known Monday) so the "next" run is
unambiguous; if a chosen `from` already equals the target time, `after()`
returns the following occurrence — account for that when asserting.

**Verify**: `cargo test --lib cron::schedule` → all pass, including the new
tests. Then temporarily revert Step 1 (or comment out the remap call) and
re-run: the Monday/Sunday tests must FAIL, proving they actually pin the fix.
Restore Step 1 afterward.

### Step 3: Format

**Verify**: `cargo fmt --all -- --check` → exit 0. If it reports a diff, run
`cargo fmt --all` and re-check.

### Step 4: Confirm the help/docs examples are now correct

Re-read each example listed in "Wrong examples that must be corrected". Under
the fixed behavior:

- `0 9 * * 1-5` = Mon–Fri ✓ (matches its "Good morning"/weekday intent)
- `0 4 * * 0` = Sunday ✓ (matches "Sunday 4 AM")

If every example is now accurate, you do **not** need to change the strings.
Optionally add a short clarifying sentence near the examples in
`docs/operations/auto-update.md` and/or the `cron add` help, e.g. in
`src/lib.rs` `long_about` after the "5-field" sentence:

> `Weekday: 0 or 7 = Sunday, 1 = Monday … 6 = Saturday (standard crontab).`

Keep any such note factual and one line. If you find an example that is still
wrong after the fix, correct that specific line and note it in the PR.

**Verify**: `git diff --stat 2aefb9f..HEAD -- src/lib.rs src/main.rs docs/` →
only the intended lines changed (or no doc changes if you left examples as-is).

## Test plan

- New tests in `src/cron/schedule.rs`:
  - `"0 9 * * 1"` resolves to a Monday (regression: was Sunday).
  - `"0 4 * * 0"` resolves to a Sunday and does not error (regression: was
    rejected).
  - `"0 0 * * 7"` resolves to a Sunday (`7`→`1`).
  - `"0 9 * * 1-5"` resolves only to Mon–Fri across several occurrences.
  - `"0 9 * * mon"` resolves to a Monday (name-passthrough sanity).
- Structural pattern: `next_run_for_schedule_supports_timezone`
  (`schedule.rs:104`).
- Verification: `cargo test --lib cron::schedule` → all pass, including the new
  tests; and the mutation check in Step 2 (revert the remap → Monday/Sunday
  tests fail).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron::schedule` exits 0; the 5 new weekday tests exist
      and pass
- [ ] `normalize_expression` remaps only the 5th field of 5-field expressions;
      6/7-field expressions are untouched (confirmed by leaving that branch
      unchanged)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] CHANGELOG note added for the one-time weekday shift (see Maintenance
      notes / STOP)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The code in `src/cron/schedule.rs:70-83` does not match the "Current state"
  excerpt (drift since this plan was written).
- The `"0 9 * * mon"` name-passthrough test FAILS. That means the crate does
  **not** map names to the Quartz ordinals as assumed, so leaving names
  unremapped is wrong — the remap design must change. Report the observed
  behavior; do not guess a name mapping.
- The `cron = "0.15"` line in `Cargo.toml` shows a different major version — the
  weekday numbering may differ; re-verify the reproduction before proceeding.
- **BACKWARD-COMPAT decision needed.** Remapping changes the meaning of
  **already-stored** 5-field expressions the moment this ships: on their next
  reschedule, existing weekday jobs shift by one day (toward the crontab-correct
  day) and any job that was silently never firing on the intended day starts
  firing correctly. There are three options:
  (a) a one-time migration that rewrites stored expressions,
  (b) a stored-expression version marker so old rows keep old semantics, or
  (c) document the one-time shift in the CHANGELOG and accept it (this is alpha
  software).
  **Recommended: (c)** — add a loud CHANGELOG entry ("BREAKING: weekday cron
  fields now follow standard crontab numbering; existing weekday jobs shift by
  one day on next reschedule") and do NOT migrate stored data. If you are
  unsure whether (c) is acceptable for this deployment, STOP and surface the
  choice to the operator rather than silently migrating or silently accepting.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- A reviewer should scrutinize: (1) the range/list/step parsing in
  `remap_weekday_element` — confirm `1-5/2`, `0,3`, `*/2`, and `0-6` all remap
  correctly (endpoints only, step suffix preserved, `*` untouched); (2) that
  6- and 7-field crate-native expressions are still passed through verbatim.
- **Cross-repo follow-up (not in this plan):** claw-ui's cron preset buttons at
  `/home/sulthannauval/project/rantai/claw-ui/src/components/ops/cron-panel.tsx`
  fill `0 9 * * 1` for "Every Monday 9:00" and `0 9 * * 1-5` for "Weekdays at
  9:00". These were compensating for nothing (they always sent crontab
  ordinals); once this Rust fix lands they become correct automatically. No
  UI change is required, but confirm the presets still produce the intended
  days after this ships.
- The raw crontab string remains what is stored in `cron_jobs.expression`; all
  translation is evaluation-time in `normalize_expression`. Any future change
  that pre-normalizes and stores the crate form must revisit the backward-compat
  decision above.
