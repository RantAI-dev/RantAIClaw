# Plan 183: Make `parse_delay` / `add_once` return an error instead of panicking on huge delays

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/mod.rs`
> If `src/cron/mod.rs` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`parse_delay` parses an unbounded `i64` and passes it straight to
`chrono::Duration::seconds/minutes/hours/days`, which **panic** on
out-of-representable-range values (documented chrono behavior). Its callers then
do `Utc::now() + parse_delay(...)?`, and `DateTime + Duration` **also** panics on
overflow. So `rantaiclaw cron once 999999999999d 'echo x'` aborts the process
instead of returning a clean error. `add_once` is a `pub` library entry point,
so any embedder inherits the panic. This plan makes oversized or overflowing
delays return an `anyhow` error, matching the fail-fast, explicit-error
convention already used one module over for `every_ms`.

## Current state

- `src/cron/mod.rs:305-324` — `parse_delay`, the source of both panics:

  ```rust
  fn parse_delay(input: &str) -> Result<chrono::Duration> {
      let input = input.trim();
      if input.is_empty() {
          anyhow::bail!("delay must not be empty");
      }
      let split = input
          .find(|c: char| !c.is_ascii_digit())
          .unwrap_or(input.len());
      let (num, unit) = input.split_at(split);
      let amount: i64 = num.parse()?;
      let unit = if unit.is_empty() { "m" } else { unit };
      let duration = match unit {
          "s" => chrono::Duration::seconds(amount),
          "m" => chrono::Duration::minutes(amount),
          "h" => chrono::Duration::hours(amount),
          "d" => chrono::Duration::days(amount),
          _ => anyhow::bail!("unsupported delay unit '{unit}', use s/m/h/d"),
      };
      Ok(duration)
  }
  ```

  `chrono::Duration::minutes/hours/days` panic when the value overflows the
  internal millisecond range; `Duration::seconds` panics only near
  `i64::MAX/1000` but is fixed here for uniformity.

- Callers that then do `now + duration` (a second panic site — `DateTime + Duration`
  panics on overflow):
  - `src/cron/mod.rs:112` — `crate::CronCommands::Once`:
    `let at = chrono::Utc::now() + parse_delay(&delay)?;`
  - `src/cron/mod.rs:268-271` — `add_once` (a `pub` fn):
    ```rust
    pub fn add_once(config: &Config, delay: &str, command: &str) -> Result<CronJob> {
        let duration = parse_delay(delay)?;
        let at = chrono::Utc::now() + duration;
        add_once_at(config, at, command)
    }
    ```

- The safe shape to copy already exists in the same crate:
  `src/cron/schedule.rs:33-37` (for `Every`):

  ```rust
  let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
  let delta = ChronoDuration::milliseconds(ms);
  from.checked_add_signed(delta)
      .ok_or_else(|| anyhow::anyhow!("every_ms overflowed DateTime"))
  ```

Chrono version: `0.4.45` (see `Cargo.lock`). It provides the non-panicking
constructors `Duration::try_seconds`, `try_minutes`, `try_hours`, `try_days`
(each returns `Option<Duration>`) and `DateTime::checked_add_signed` (returns
`Option<DateTime>`). Use those.

Repo conventions: `anyhow::bail!` / `anyhow::anyhow!` for errors (see
`schedule.rs`). Keep control flow explicit (KISS) — a `match` over the unit,
each arm mapping `None` to an `anyhow` error.

## Commands you will need

| Purpose   | Command                                      | Expected on success       |
|-----------|----------------------------------------------|---------------------------|
| Format    | `cargo fmt --all -- --check`                 | exit 0, no diff           |
| Lint      | `cargo clippy --all-targets -- -D warnings`  | exit 0, no warnings       |
| Tests     | `cargo test --lib cron`                      | all pass, incl. new tests |

Do NOT run a bare `cargo test` (disk-constrained box). Use `--lib cron`.

## Scope

**In scope** (the only file you should modify):
- `src/cron/mod.rs` — harden `parse_delay`, harden the `add_once` overflow add,
  and add unit tests. (The `Once` command handler at line 112 is covered by
  fixing `parse_delay` to bound the amount AND by keeping the add safe — see
  Step 2.)

**Out of scope** (do NOT touch):
- `src/cron/schedule.rs` — already safe; do not refactor it into a shared helper
  (rule-of-three not met; a single inline fix in `mod.rs` is clearer here).
- `add_once_at` (`src/cron/mod.rs:274-281`) — it takes an already-constructed
  `at: DateTime<Utc>` and does not add anything, so it cannot overflow. Leave it.
- The `AddEvery`/`Every` path — bounded elsewhere.

## Git workflow

- Branch: `advisor/183-cron-parse-delay-panic`
- Conventional commit, e.g.
  `fix(cron): return an error for oversized 'once'/delay values instead of panicking`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Bound the unit conversion in `parse_delay`

Replace the panicking `chrono::Duration::seconds/minutes/hours/days` calls with
the non-panicking `try_*` constructors, mapping `None` to an `anyhow` error.

Target shape for the `match`:

```rust
    let duration = match unit {
        "s" => chrono::Duration::try_seconds(amount),
        "m" => chrono::Duration::try_minutes(amount),
        "h" => chrono::Duration::try_hours(amount),
        "d" => chrono::Duration::try_days(amount),
        _ => anyhow::bail!("unsupported delay unit '{unit}', use s/m/h/d"),
    }
    .ok_or_else(|| anyhow::anyhow!("delay too large: {input}"))?;
    Ok(duration)
```

Note the `num.parse::<i64>()?` at the top already errors (rather than panics) on
a numeric string too large for `i64` — keep it. The `try_*` mapping handles the
in-`i64`-but-out-of-`Duration`-range case.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Make the `add_once` add non-panicking

In `add_once` (`src/cron/mod.rs:268-272`), replace `chrono::Utc::now() +
duration` with a checked add. Do the same for the `Once` command handler at
`src/cron/mod.rs:112`.

Target shapes:

```rust
pub fn add_once(config: &Config, delay: &str, command: &str) -> Result<CronJob> {
    let duration = parse_delay(delay)?;
    let at = chrono::Utc::now()
        .checked_add_signed(duration)
        .ok_or_else(|| anyhow::anyhow!("delay too large: {delay}"))?;
    add_once_at(config, at, command)
}
```

and in `handle_command`'s `crate::CronCommands::Once { delay, .. }` arm
(line ~112):

```rust
            let at = chrono::Utc::now()
                .checked_add_signed(parse_delay(&delay)?)
                .ok_or_else(|| anyhow::anyhow!("delay too large: {delay}"))?;
```

Rationale: `Duration::try_days(amount)` can succeed (the Duration is
representable) while `now + that_duration` still overflows the `DateTime`
range, so both sites need guarding. `checked_add_signed` returns
`Option<DateTime<Utc>>`.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Add unit tests

Add tests to the `tests` module at the bottom of `src/cron/mod.rs` (after
`cli_runs_lists_history`, ~line 578). `parse_delay` and `add_once` are in scope
via `use super::*` (line 328).

```rust
    #[test]
    fn parse_delay_rejects_oversized_amount_without_panicking() {
        // A value that overflows chrono::Duration for the given unit must be an
        // Err, not a panic. (Days are the tightest bound.)
        let err = parse_delay("999999999999d").unwrap_err();
        assert!(
            err.to_string().contains("delay too large")
                || err.to_string().contains("number too large")
                || err.to_string().contains("invalid digit"),
            "expected a bounded error, got: {err}"
        );
    }

    #[test]
    fn parse_delay_accepts_reasonable_values() {
        assert_eq!(parse_delay("30").unwrap(), chrono::Duration::minutes(30));
        assert_eq!(parse_delay("2h").unwrap(), chrono::Duration::hours(2));
        assert_eq!(parse_delay("45s").unwrap(), chrono::Duration::seconds(45));
        assert_eq!(parse_delay("7d").unwrap(), chrono::Duration::days(7));
    }

    #[test]
    fn add_once_errors_on_overflowing_delay_instead_of_panicking() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        // Large-but-parses-as-i64 value; must surface as Err, never panic/abort.
        let result = add_once(&config, "9999999999d", "echo x");
        assert!(result.is_err(), "oversized delay must return Err");
    }
```

Notes:
- `parse_delay_accepts_reasonable_values` guards against over-rejection.
- The exact numeric literal that trips the bound is not important; use one large
  enough to overflow `Duration::try_days` and/or the `i64` parse.
  `999999999999` (12 digits) fits `i64` but overflows the day-duration range;
  `9999999999d` (10 digits) fits `i64` and overflows `now + duration`. Both must
  be `Err`.

**Verify**: `cargo test --lib cron` → all pass, including the three new tests.

### Step 4: Prove the tests catch the panic (mutation check)

Temporarily revert Step 1's `try_days` back to `chrono::Duration::days(amount)`
(the panicking form) and run
`cargo test --lib cron parse_delay_rejects_oversized_amount_without_panicking`.
The test binary MUST fail/abort (a panic inside a `#[test]` is reported as a test
failure). Restore Step 1 and confirm it passes. This proves the test exercises
the panic path.

**Verify**: with the panicking constructor restored the named test fails; with
`try_days` it passes.

## Test plan

- New tests in `src/cron/mod.rs::tests`:
  - `parse_delay_rejects_oversized_amount_without_panicking` (the bug).
  - `parse_delay_accepts_reasonable_values` (no over-rejection).
  - `add_once_errors_on_overflowing_delay_instead_of_panicking` (the `pub`
    entry-point overflow-add path).
- Structural pattern: existing `tests` in `src/cron/mod.rs` (`test_config`
  helper at line 332).
- Verification: `cargo test --lib cron` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` exits 0; the three new tests exist and pass
- [ ] `grep -n "Duration::seconds\|Duration::minutes\|Duration::hours\|Duration::days" src/cron/mod.rs`
      returns no matches in `parse_delay` (only `try_*` forms remain there; test
      assertions comparing against `Duration::minutes(...)` are fine)
- [ ] With a panicking constructor restored the mutation test fails (Step 4)
- [ ] No files outside `src/cron/mod.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `parse_delay` no longer matches the excerpt (already hardened, refactored into
  a shared helper, or moved) — the codebase has drifted.
- `chrono::Duration::try_days`/`try_hours`/`try_minutes`/`try_seconds` do not
  exist on the pinned chrono version (compile error). Confirm the chrono version
  in `Cargo.lock`; if it predates the `try_*` API, use checked arithmetic
  (`Duration::days(amount).checked_add(...)` is still panicking, so instead
  compute via `i64::checked_mul` on the seconds and build with
  `Duration::try_seconds`) and note the substitution — or STOP and report.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

- Any new delay unit added to `parse_delay` must also use a `try_*` constructor
  and route through the same `.ok_or_else(...)` bound.
- A reviewer should confirm BOTH panic sites are closed: the unit conversion
  (Step 1) and the `now + duration` add (Step 2). Fixing only one leaves a panic.
- Deferred: no user-facing message change beyond the new "delay too large" error;
  no config or schema impact.
