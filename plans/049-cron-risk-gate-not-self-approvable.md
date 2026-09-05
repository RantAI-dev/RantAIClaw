# Plan 049: Stop the model self-approving risky cron commands, and apply the risk gate to scheduled runs

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 3edb236..HEAD -- src/tools/cron_add.rs src/tools/cron_run.rs src/cron/scheduler.rs src/security/policy.rs src/tools/cron_update.rs `
> If any changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding. Line numbers drifting by a
> line or two while the quoted text matches is **not** a STOP — only a content
> mismatch is.

## Status

- **Priority**: P1
- **Effort**: S–M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `3edb236`, 2026-07-27

## Why this matters

Two independent holes in the cron pipeline, which together mean the shell
risk classification is not an effective control there.

**Hole 1 — the agent approves its own risky commands.** `approved` is a
declared parameter in the `cron_add` and `cron_run` tool schemas, so the
*model* fills it in. It is passed straight to
`SecurityPolicy::validate_command_execution`, where `!approved` is the only
thing standing between a medium- or high-risk command and execution under
`Supervised`. The error text says "Command requires explicit approval
(approved=true)", which reads like a human decision; nothing verifies a human
made it. `PendingApprovals` — the real approval registry — is never consulted.

**Hole 2 — scheduled runs skip risk classification entirely.**
`run_job_command_with_timeout` checks `can_act`, `is_rate_limited`,
`is_command_allowed`, `forbidden_path_argument`, and `record_action` — but
never calls `validate_command_execution`. So `block_high_risk_commands` and
`require_approval_for_medium_risk` are dead config for any job that fires on
its schedule, which is the unattended path with no operator present.

Chain them: the agent creates a job via `cron_add` with `approved: true`
(passing the risk gate), the job is persisted, and the scheduler later runs it
with no risk gate at all.

**Bound on severity, stated honestly**: `validate_command_execution` checks the
allowlist *first* (`src/security/policy.rs:588-590`), so a command that is not
on the allowlist is rejected before any approval logic runs, and
`is_command_allowed` still guards the scheduled path. This is not "run
anything" — it is "risk classification does not constrain what an allowlisted
command may do on cron".

**Operational consequence a reviewer must approve knowingly**: after this
change, medium-risk commands on the *default* allowlist — notably
`git commit` and `npm install` (`src/config/schema.rs:2187-2189`) — will be
**refused** on scheduled cron runs under `Supervised`, because no operator is
present to approve them. That is the intended semantics of "requires explicit
approval" on an unattended path, but it can break existing scheduled jobs.
Operators who need those jobs have two supported options: run the agent at
`off`/`Full`, or set `require_approval_for_medium_risk = false`.

## Current state

Files involved:

- `src/tools/cron_add.rs`, `src/tools/cron_run.rs` — agent-facing tools that
  expose `approved` in their JSON schema.
- `src/gateway/cron_api.rs` — the HTTP equivalent (caller-supplied query param).
- `src/cron/scheduler.rs` — the scheduled execution path.
- `src/security/policy.rs` — the gate itself.

The gate, and the ordering that bounds the severity —
`src/security/policy.rs:582-612`:

```rust
    pub fn validate_command_execution(
        &self,
        command: &str,
        approved: bool,
    ) -> Result<CommandRiskLevel, String> {
        if !self.is_command_allowed(command) {
            return Err(format!("Command not allowed by security policy: {command}"));
        }

        let risk = self.command_risk_level(command);

        if risk == CommandRiskLevel::High {
            if self.block_high_risk_commands {
                return Err("Command blocked: high-risk command is disallowed by policy".into());
            }
            if self.effective_autonomy() == AutonomyLevel::Supervised && !approved {
                return Err(
                    "Command requires explicit approval (approved=true): high-risk operation"
                        .into(),
                );
            }
        }
```

`approved` is declared in the model-facing schema —
`src/tools/cron_run.rs:35-39`:

```rust
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk shell commands in supervised mode",
                    "default": false
                }
```

…read from the model's own arguments — `src/tools/cron_run.rs:64-67`:

```rust
        let approved = args
            .get("approved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
```

…and handed to the gate — `src/tools/cron_run.rs:100-102`:

```rust
            if let Err(reason) = self
                .security
                .validate_command_execution(&job.command, approved)
```

`src/tools/cron_add.rs` has the same three pieces at `:93` (schema), `:161`
(read), and `:179` (gate call).

The scheduled path's gate block, with no `validate_command_execution` —
`src/cron/scheduler.rs:519-556`:

```rust
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
```
```rust
    if !security.is_command_allowed(&job.command) {
```
```rust
    if let Some(path) = forbidden_path_argument(security, &job.command) {
```
```rust
    if !security.record_action() {
```

Relevant default: `AutonomyConfig::default()` sets
`block_high_risk_commands: false` (`src/config/schema.rs:2224`, commented
"Easy-mode default: high-risk commands are no longer hard-blocked"), while the
serde default for a config file that omits the key is `true`
(`src/config/schema.rs:2160`). So a generated config — what a real install
has — does **not** hard-block high risk, leaving `approved` as the only gate.

Repo conventions to match:

- Tools return `Ok(ToolResult { success: false, error: Some(...) })` for a
  policy refusal rather than `Err` — see the existing refusal blocks in
  `src/tools/cron_run.rs`.
- Tests live in-file under `#[cfg(test)] mod tests`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no output |
| Lint (same as CI) | `cargo clippy --locked --all-targets -- -D clippy::correctness` | exit 0 |
| Compile incl. tests | `cargo check --all-targets` | exit 0 |
| Unit tests | `cargo test --lib` | exit 0, all pass |
| Focused tests | `cargo test --lib cron` | all pass |

Note: CI also runs a **strict-delta** clippy gate
(`scripts/ci/rust_strict_delta_gate.sh`) at `-D warnings` — restricted to the
lines your diff touches, with pedantic lints on. The table's
`-D clippy::correctness` will not catch those. Before pushing, re-run clippy
at `-D warnings` and check that no warning points at a line you added.

Note: some `skills::tests::toml_*` tests are non-hermetic against `$HOME` on
some machines. If they fail, confirm they also fail on an unmodified checkout
before treating it as your regression.

## Scope

**In scope**:

- `src/tools/cron_add.rs`
- `src/tools/cron_run.rs`
- `src/tools/cron_update.rs` — **third instance of the same hole**, found
  during review: `"approved"` in the schema at `:72`, read from model args at
  `:123`, passed to the gate at `:129`. Fixing only two of the three doors
  leaves the bypass fully open.
- `src/cron/scheduler.rs`
- `plans/README.md` — append (the table ends at row `045` on today's tree; in the execution order 046-051 each earlier plan appends its own row, so expect it to end at the row before `049`. Append rather than assuming a fixed last row):

  ```
  | 049 | Stop the model self-approving risky cron commands; apply the risk gate to scheduled runs | P1 | S–M | MED | — | security | TODO |
  ```

**Out of scope** (do NOT touch):

- `src/security/policy.rs` — `validate_command_execution` is correct as
  written. You are changing *who supplies* `approved` and *where the gate is
  called*, not the gate.
- `src/gateway/cron_api.rs` — its `approved` is a caller-supplied HTTP query
  param on an authenticated endpoint, which is a different trust model from a
  model-authored tool argument. Changing it needs an API-compatibility
  decision; leave it and note it in your report.
- The allowlist layer, `runtime_allowlist`, and `PendingApprovals` wiring.
- The scheduler's stale-policy problem (it holds a boot-time policy). Real,
  separate, planned elsewhere. Do not try to fix it here.

## Git workflow

- Branch: `fix/cron-risk-gate-not-self-approvable`
- Conventional commit titles. Example from this repo's history:
  `fix(channels): let a config reload narrow the shell allowlist`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Remove `approved` from the three tool schemas

In `src/tools/cron_add.rs`, `src/tools/cron_run.rs`, and
`src/tools/cron_update.rs`, delete the `"approved"` property from
`parameters_schema()` (at `:93`, `:35-39`, and `:72` respectively). A
parameter the model can set is not an approval.

**Verify**: `cargo check --all-targets` → exit 0, and for each of the three
files the schema block no longer mentions it:
`awk '/fn parameters_schema/,/^    }$/' <file> | grep -c '"approved"'` → `0`.

Scope the check to the schema block. A whole-file grep can never reach `0`
here — the Test plan deliberately adds a test containing the literal
`"approved": true`, and `cron_update.rs` already has one at `:282-286`.

### Step 2: Stop reading it from the model's arguments

In all three files, delete the `let approved = args.get("approved")…` binding
(`cron_add.rs:161`, `cron_run.rs:64-67`, `cron_update.rs:123`) and pass `false`
at the `validate_command_execution` call sites (`cron_add.rs:179`,
`cron_run.rs:100-102`, `cron_update.rs:129`).

Passing `false` is deliberate and is the point of the plan: under `Supervised`
a medium/high-risk cron command is now refused rather than self-approved. The
refusal message already tells the operator what happened.

**Two existing tests assert the behaviour you are removing. Updating them is
part of this step, not a STOP.**

- `src/tools/cron_add.rs:378` `medium_risk_shell_command_requires_approval` —
  its second half (around `:405-414`) calls the tool with `"approved": true`
  on `touch cron-approval-test` and asserts `approved.success`.
- `src/tools/cron_run.rs:216` `shell_run_requires_approval_for_medium_risk` —
  same shape at `:237-241`.
- `src/tools/cron_update.rs:255` `medium_risk_shell_update_requires_approval` —
  same shape, asserting `approved.success` at `:290`.

(`touch` is Medium risk per `src/security/policy.rs:558`; default autonomy is
Supervised and `require_approval_for_medium_risk` defaults to `true`, so these
genuinely exercise the removed path.)

For each of the **three**: **delete the `approved: true` half** and keep the first half, which
asserts the refusal and that the message contains `"explicit approval"`. Rename
each test to drop the now-wrong implication that approval is possible — e.g.
`medium_risk_shell_command_is_refused`,
`shell_run_refuses_medium_risk_without_operator_approval`, and
`medium_risk_shell_update_is_refused`.

**Verify**: `cargo check --all-targets` → exit 0.

### Step 3: Apply the risk gate on the scheduled path

In `src/cron/scheduler.rs`, inside `run_job_command_with_timeout`, add a
`validate_command_execution` check. Place it **after** the existing
`is_command_allowed` block (`:533`) and before `forbidden_path_argument`.

**Order matters and this is not arbitrary.** `validate_command_execution`
performs its own allowlist check first and returns
`"Command not allowed by security policy: …"` — capital `C`. The existing test
`run_job_command_blocks_disallowed_command` asserts the current lowercase
message at `src/cron/scheduler.rs:692`
(`assert!(output.contains("command not allowed"))`), and `contains` is
case-sensitive. Putting the new check **before** `is_command_allowed` changes
which message wins and breaks that test. Putting it after leaves the existing
allowlist path — and its message — untouched, so the new check only ever fires
for the risk classification, which is exactly its job.

Keep the existing message style (`"blocked by security policy: …"`) so the
retry short-circuit at `src/cron/scheduler.rs:110`, which matches on that
prefix, keeps suppressing retries of a deterministic denial.

Target shape:

```rust
    if let Err(reason) = security.validate_command_execution(&job.command, false) {
        return (
            false,
            format!("blocked by security policy: {reason}"),
        );
    }
```

Pass `false` for `approved`: on the scheduled path there is by definition no
operator present to approve anything.

**Verify**: `cargo check --all-targets` → exit 0.

### Step 4: Add the regression tests

See "Test plan". Write them before running the full suite.

**Verify**: `cargo test --lib cron` → all pass.

### Step 5: Full verification

**Verify**, all three:

- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --locked --all-targets -- -D clippy::correctness` → exit 0
- `cargo test --lib` → exit 0

## Test plan

`src/cron/scheduler.rs` already has a `#[cfg(test)] mod tests` (starts at
`:591`) containing policy-building helpers — read them first and reuse their
shape rather than inventing new ones.

1. `scheduled_run_refuses_a_high_risk_command_under_supervised` — build a
   policy at `Supervised` with `block_high_risk_commands: false` and an
   allowlist that **contains** the command's base name (so the allowlist is not
   what refuses it). Run a shell job whose command classifies as high risk.

   Use `chmod 644 f`. `chmod` is high risk (`src/security/policy.rs:483`) and
   `644`/`f` pass `is_args_safe` (which special-cases only `find` and `git`).
   **`chmod` is NOT on either default allowlist** — neither
   `AutonomyConfig::default()` (`src/config/schema.rs:2187-2199`) nor
   `SecurityPolicy::default()` (`src/security/policy.rs:141-153`) contains it.
   You **must** set `allowed_commands` to include `"chmod"` explicitly. If you
   skip that, `is_command_allowed` refuses first at
   `src/cron/scheduler.rs:533-540` with a message that *also* contains
   `"blocked by security policy"` — the test then passes on pre-fix code, is
   vacuous, and the mutation check below cannot falsify it.

   Assert the message contains `"blocked by security policy"`. Do **not** assert
   on the exit status: pre-fix, `chmod 644 f` runs against a nonexistent file in
   a fresh temp workspace and exits 1, so `success` is already `false` before
   the fix. The message is what distinguishes the two — pre-fix the output is
   `chmod`'s own stderr and contains no `"blocked by security policy"`, because
   the scheduled path never called the risk gate.

2. `scheduled_run_still_allows_a_low_risk_allowlisted_command` — the guard
   against over-blocking: a low-risk allowlisted command still runs. Without
   this, Step 3 could pass test 1 by refusing everything.

3. In `src/tools/cron_run.rs`'s test module:
   `cron_run_does_not_accept_an_approved_argument` — call the tool with
   `{"job_id": "...", "approved": true}` on a high-risk allowlisted command
   under `Supervised`, and assert it is still refused. This pins that the
   parameter cannot be smuggled back in even if a model emits it.

**Verification**: `cargo test --lib` → all pass, including the 3 new tests.

**Mutation check (required before you call this done)**:

- Revert Step 3 (remove the `validate_command_execution` call from the
  scheduler). Test 1 must fail.
- Revert Step 2 (read `approved` from args again in `cron_run.rs`). Test 3 must
  fail.

  For that mutation to falsify, **test 3 must assert on `result.error`**, not on
  `result.output` and not on a Debug dump of the whole result. The two refusals
  are distinguished by *which field* they land in, not by their text:

  - Tool gate (post-fix, what test 3 pins): `cron_run.rs:104-108` returns
    `error: Some("Command requires explicit approval (approved=true): …")` with
    `output` empty.
  - Scheduler gate (Step 3, reached only when the tool gate passes):
    `cron_run.rs:126-138` puts the scheduler's message in **`output`** and sets
    `error: Some("cron job execution failed")`.

  Note both messages contain the substring `"explicit approval"` after Step 3 —
  the scheduler wraps the *same* `validate_command_execution` reason. So a
  substring match against the whole result does **not** discriminate. Asserting
  merely "still refused" fails for the same reason: `CronRunTool` reaches the
  scheduler gate anyway (`cron_run.rs:123` → `run_job_manual`, `scheduler.rs:65`
  → `run_job_command_with_timeout`, `:513`), so Step 3 keeps refusing and the
  test stays green under its own mutation. Reverting Step 2 *and* Step 3
  together does not rescue it either — the command still exits nonzero on its
  own. The existing test at `cron_run.rs:232-235` already asserts on `.error`;
  keep that shape.

Restore both. If either still passes under its own mutation, that test is not
covering the change — STOP and report.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --locked --all-targets -- -D clippy::correctness` exits 0
- [ ] `cargo test --lib` exits 0
Each command below was **run against the current tree** and returns the
"before" value shown, so each is genuinely falsifiable.

- [ ] `grep -rn "let approved = args" src/tools/cron_add.rs src/tools/cron_run.rs src/tools/cron_update.rs | wc -l` returns `0` (before: `3`).
      Scope it to the three files: repo-wide the pattern also matches
      `src/tools/shell.rs:287`, which is out of scope and must keep its
      operator-supplied `approved`.
- [ ] The `"approved"` property is gone from all three schemas — check the schema
      block specifically, because the Test plan deliberately writes a test
      containing the literal `"approved": true`:
      `awk '/fn parameters_schema/,/^    }$/' src/tools/cron_run.rs | grep -c '"approved"'` returns `0`, and the same for `src/tools/cron_add.rs` and `src/tools/cron_update.rs`
- [ ] `grep -rn "validate_command_execution" src/cron/scheduler.rs | wc -l`
      returns at least `1` (before: `0`)
- [ ] The three new tests exist and pass; both mutation checks were performed
      and each named test failed under its own mutation
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The excerpts in "Current state" do not match the live code (content
  mismatch, not line drift).
- A test **other than** the three named in Step 2
  (`medium_risk_shell_command_requires_approval`,
  `shell_run_requires_approval_for_medium_risk`,
  `medium_risk_shell_update_requires_approval`) asserts the tool *accepts*
  `approved`. Those three are expected and Step 2 tells you how to update
  them; any other one is new information.
- A scheduler test **other than** `run_job_command_blocks_disallowed_command`
  fails after Step 3. That one is protected by the ordering Step 3 specifies;
  if it fails anyway, the check was placed in the wrong position — re-read
  Step 3 before doing anything else.
- You find that `command_risk_level` classifies so broadly that test 2
  (low-risk still runs) cannot be satisfied with a realistic command — that
  would mean this change blocks ordinary cron usage far beyond the
  `git commit` / `npm install` cases already named in "Why this matters", and
  the operator needs to decide before it ships.

## Maintenance notes

- **Step 3 also affects the manual run paths, not only scheduled runs.**
  `run_job_command_with_timeout` is reached by `run_job_manual`
  (`scheduler.rs:65` → `:513`), whose callers are the HTTP endpoint
  (`src/gateway/cron_api.rs:380`), the TUI force-run (`src/tui/app.rs:3054`),
  the CLI `cron run` (`src/cron/mod.rs:240`), and `CronRunTool`. So an
  operator's authenticated `approved=true` passes the endpoint's own gate at
  `cron_api.rs:375` and is then overridden by the hard-coded `false` one layer
  down. That is a real behaviour change to a documented endpoint
  (`docs/reference/commands.md:159`) and must be called out in the PR.
  If the operator wants the HTTP `approved` honoured, that is a follow-up:
  thread the caller's flag through `run_job_manual` rather than hard-coding
  `false` there.
- The HTTP path (`src/gateway/cron_api.rs:375`) still *accepts* `approved` as a
  query param, but per the point above it no longer takes effect. That is a different trust model — an authenticated operator
  calling the API — but if the project ever wants one story for "who may
  approve", that call site is the remaining one.
- If a real approval flow is wanted for cron later, the mechanism already
  exists: `PendingApprovals` (`SecurityPolicy::pending`). It is not wired into
  cron at all today. Wiring it is a feature, not a fix, and deliberately out of
  this plan.
- Reviewers should check that Step 3's message keeps the
  `"blocked by security policy:"` prefix — `execute_job_with_retry` matches on
  that string to avoid retrying a deterministic denial, and a reworded message
  would silently reintroduce retry storms on refused jobs.
