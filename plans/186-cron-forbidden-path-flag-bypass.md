# Plan 186: Close the flag-value bypass in the cron forbidden-path guard

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`forbidden_path_argument` (`src/cron/scheduler.rs:~415-452`) is a
defense-in-depth guard that blocks a scheduled shell job from touching a
disallowed path. It tokenizes the command and checks each token against
`is_path_allowed`. But it `continue`s on any token that `starts_with('-')`
(`src/cron/scheduler.rs:435`) **before** the path check (439-447). So a forbidden
path embedded in a **flag value** — a path passed as `--file=<forbidden>`,
`--output=<forbidden>`, or a short-flag-glued `-o<forbidden>` — slips past the
guard that a bare `cat <forbidden>` trips (the exact behavior the test at
`src/cron/scheduler.rs:744-757` pins with `cat /etc/passwd`). The allowlist and
risk gates still run upstream, so this is defense-in-depth, not the only barrier
— but it runs on the **unattended** scheduled path where no operator is present,
so a bypass there is worth closing. This plan extends the guard to inspect the
value portion of `--flag=value` (and glued short flags) so a forbidden path can
no longer hide behind a flag.

## Current state

`src/cron/scheduler.rs:433-448` — the token loop with the early skip:
```rust
for token in &tokens[idx..] {
    let candidate = strip_wrapping_quotes(token);
    if candidate.is_empty() || candidate.starts_with('-') || candidate.contains("://") {
        continue;   // <-- 435: a `--file=/etc/shadow` token is skipped whole
    }

    let looks_like_path = candidate.starts_with('/')
        || candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate.starts_with("~/")
        || candidate.contains('/');

    if looks_like_path && !security.is_path_allowed(candidate) {
        return Some(candidate.to_string());
    }
}
```

The guard is called from `run_job_command_with_timeout`
(`src/cron/scheduler.rs:522-527`), which returns a "forbidden path argument"
refusal, after the allowlist (504) and risk gate (518).

Existing test that pins the intended behavior
(`src/cron/scheduler.rs:744-757`):
```rust
async fn run_job_command_blocks_forbidden_path_argument() {
    // ... allowed_commands = ["cat"], job = "cat /etc/passwd"
    let (success, output) = run_job_command(&config, &security, &job).await;
    assert!(!success);
    assert!(output.contains("forbidden path argument"));
    assert!(output.contains("/etc/passwd"));
}
```
Test helpers `test_config` / `test_job` live in the same
`#[cfg(test)] mod tests` (module opens at `src/cron/scheduler.rs:570`).

Repo conventions:
- `is_path_allowed(candidate)` is the single source of truth for path
  admissibility — reuse it; do not reimplement path logic.
- `strip_wrapping_quotes` already normalizes a token; keep using it.
- KISS: extend the existing loop; do not restructure the tokenizer.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests   | `cargo test --lib cron` | all pass |
| Drift   | `git diff --stat 2aefb9f..HEAD -- src/cron/scheduler.rs` | only your changes |

Do **not** run a bare `cargo test` (disk-constrained). Scope with `--lib`.

## Scope

**In scope**:
- `src/cron/scheduler.rs` — `forbidden_path_argument` flag-value handling +
  a new test

**Out of scope**:
- The allowlist / risk-classification gates (upstream, unchanged).
- Any change to `is_path_allowed` or the workspace-allow logic.
- Broadening the guard to non-cron shell execution (the tools' shell path) —
  this plan is the scheduled cron path only.

## Git workflow

- Branch: `advisor/186-cron-forbidden-path-flag-bypass`
- Conventional commit
  (e.g. `fix(cron): check forbidden paths inside flag values`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Inspect the value portion of `--flag=value` (and glued short flags)

Rework the early-skip branch (`src/cron/scheduler.rs:435`) so that a flag token
is not blindly skipped when it carries a path in its value:

1. Keep skipping `candidate.is_empty()` and `candidate.contains("://")` (URLs).
2. When `candidate.starts_with('-')`:
   - If it contains `=`, take the substring **after the first `=`** as the
     value, `strip_wrapping_quotes` it, and run that value through the same
     `looks_like_path` + `is_path_allowed` check. If the value is a forbidden
     path, `return Some(value)`.
   - For a **glued short flag** (e.g. `-o/etc/passwd`): after confirming it is a
     short flag (starts with a single `-`, not `--`), consider the remainder
     after the leading flag letter(s) as a candidate value and apply the same
     path check. Keep this conservative — if the remainder does not
     `looks_like_path`, ignore it (it is an ordinary flag).
   - Otherwise (a plain flag with no path-shaped value), `continue` as today.

Target shape (illustrative, adapt to the surrounding code):
```rust
if candidate.contains("://") {
    continue;
}
if let Some(stripped) = candidate.strip_prefix('-') {
    // --flag=value  OR  -flag=value
    if let Some((_, value)) = candidate.split_once('=') {
        let value = strip_wrapping_quotes(value);
        if path_is_forbidden(security, value) {
            return Some(value.to_string());
        }
        continue;
    }
    // glued short flag: -o/etc/passwd (single leading dash, no `=`)
    if !stripped.starts_with('-') {
        // remainder after the first char (the flag letter)
        if let Some(rest) = stripped.get(1..) {
            let rest = strip_wrapping_quotes(rest);
            if path_is_forbidden(security, rest) {
                return Some(rest.to_string());
            }
        }
    }
    continue;
}
if candidate.is_empty() {
    continue;
}
// ... existing bare-token path check ...
```
Extract the `looks_like_path && !security.is_path_allowed(...)` logic into a
small local `path_is_forbidden(security, s) -> bool` so the bare-token branch and
the flag-value branch share one implementation (avoids drift).

**Verify**: `cargo test --lib cron` → the existing
`run_job_command_blocks_forbidden_path_argument` (744-757) still passes (bare
`cat /etc/passwd` still caught). `cargo clippy --all-targets -- -D warnings` →
exit 0.

### Step 2: Add regression tests for the flag-value vectors

In the same test module (opens at `src/cron/scheduler.rs:570`), add tests
modeled on `run_job_command_blocks_forbidden_path_argument`:

- `--file=/etc/shadow` form: `allowed_commands = ["cat"]`,
  `job = test_job("cat --file=/etc/shadow")` → refused with "forbidden path
  argument" and the path in the message. Reuse `cat` (not an invented program
  name): the existing `run_job_command_blocks_forbidden_path_argument` test
  already proves `cat` clears both the allowlist gate at 504 and the risk gate at
  518 and reaches the path guard, so no guesswork about which neutral name is
  low-risk is needed.
- Glued short flag `-o/etc/passwd` form: same structure.
- **Negative control** (guard against over-blocking): a flag whose value is a
  PATH-SHAPED workspace-relative allowed path (e.g. `--out=./notes.txt` under the
  temp workspace — it must start with `./` so `looks_like_path` is true and the
  value actually flows through `is_path_allowed`; a bare `--out=notes.txt` has no
  `/` and is skipped by `looks_like_path` before reaching the guard, so it would
  pass even if the fix over-blocks) is **not** refused — assert `success` or at
  least that the failure is not "forbidden path argument". This proves Step 3's
  concern is handled.

**Verify**: `cargo test --lib cron` → all new tests pass.

### Step 3: Verify workspace-allowed paths still pass (over-blocking check)

`is_path_allowed` already admits workspace-relative paths. Confirm the negative
control test from Step 2 passes: a job whose flag value is a legitimate
in-workspace path must still run. If stricter matching newly refuses a legit
job, the `looks_like_path` + `is_path_allowed` reuse (not a broader regex) is
what keeps parity — do not add path heuristics beyond what the bare-token branch
already uses.

**Verify**: the negative-control test passes; `cargo test --lib cron` green.

## Test plan

- `--file=/etc/shadow` is caught (new).
- `-o/etc/passwd` glued short flag is caught (new).
- A workspace-relative flag value is NOT caught (negative control, new).
- The existing bare `cat /etc/passwd` test still passes (unchanged).
- Verification: `cargo test --lib cron` → all pass, including the new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` passes; the three new tests exist and pass; the
      existing forbidden-path test still passes
- [ ] `forbidden_path_argument` inspects `--flag=value` and glued short-flag
      values, sharing one `path_is_forbidden` implementation with the bare-token
      branch
- [ ] A workspace-relative flag value is not newly refused (negative control)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report if:

- The "Current state" excerpt (esp. the `starts_with('-')` skip at line 435 and
  the test at 744-757) does not match live code (drift since 2aefb9f).
- Making the matching stricter refuses a legitimate workspace path you cannot
  keep passing via `is_path_allowed` — report before shipping (MED risk: the
  whole point is not to newly break legit jobs).
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- A reviewer should scrutinize: that the `=`-split takes the value (not the flag
  name), that URLs (`contains("://")`) are still skipped, and that the negative
  control proves no over-blocking of workspace paths.
- This is defense-in-depth on the unattended path; it does not replace the
  allowlist/risk gates. If those are ever relaxed, this guard becomes more
  load-bearing — keep its test coverage.
- Deferred: applying the same flag-value inspection to the interactive tools'
  shell path (out of scope; cron-only here).
