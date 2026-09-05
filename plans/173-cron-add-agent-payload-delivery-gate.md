# Plan 173: Gate cron agent-job delivery targets and payload against the caller's own conversation

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/tools/cron_add.rs src/agent/loop_.rs src/cron/scheduler.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (cross-ref plans/172, plans/168)
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

The `JobType::Agent` branch of `cron_add` validates only that `prompt` is
non-empty (`src/tools/cron_add.rs:184-194`) and accepts a model-supplied
`delivery` (`channel` + `to`) **verbatim** (`src/tools/cron_add.rs:215-227`)
with no check that `to` belongs to the caller's own conversation. Contrast the
Shell branch, which calls `validate_command_execution` first
(`src/tools/cron_add.rs:170`). The origin-chat safety net
(`maybe_inject_channel_delivery`, `src/agent/loop_.rs:1058-1094`) only injects a
delivery target when the model set **none** (lines 1070-1078) — an explicit,
attacker-chosen `delivery` is preserved untouched. At fire time,
`deliver_if_configured` (`src/cron/scheduler.rs:360-391`) sends to the stored
`to` with no re-validation. Combined with the daemon-privilege scheduled run
(plan 172), a single indirect prompt injection during one channel turn can
install a recurring headless agent job that announces its output to an
attacker-chosen chat — a persistent exfiltration channel created from one
transient injection. This plan validates the delivery target at creation time
against the caller's conversation and re-validates at fire time, and applies a
minimal payload gate symmetric with the shell branch.

## Current state

Files and roles:

- `src/tools/cron_add.rs` — `CronAddTool` (struct at 9-12: only `config` +
  `security`, **no reply_target/sender**). Agent branch 184-243; prompt-only
  validation 185-194; verbatim `delivery` acceptance 215-227; the shell branch's
  contrasting gate is `self.security.validate_command_execution(command, false)`
  at line 170.
- `src/agent/loop_.rs` — `maybe_inject_channel_delivery(call, channel_name,
  reply_target)` (1058-1094): only injects when the model set no real delivery
  (1070-1078); it is the code that *knows* the caller's `reply_target`. It is
  invoked from the tool-dispatch functions where `channel_reply_target`
  (`Option<&str>`) and `guest_gate` are in scope — see the dispatch at lines
  1224-1249 and the sequential path around 1360.
- `src/cron/scheduler.rs` — `deliver_if_configured(config, job, output)`
  (360-391): reads `job.delivery.{channel,to}`, checks
  `channel_supports_announce_delivery`, then `channel_impl.send(...)` to the
  stored `to` with **no target re-validation**.

Key excerpts:

`src/tools/cron_add.rs:215-227` (delivery taken verbatim):
```rust
let delivery = match args.get("delivery") {
    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
        Ok(cfg) => Some(cfg),
        Err(e) => { /* ... error ... */ }
    },
    None => None,
};
```

`src/agent/loop_.rs:1070-1078` (safety net preserves an explicit delivery):
```rust
let has_real_delivery = call
    .arguments
    .get("delivery")
    .and_then(|d| d.get("mode"))
    .and_then(serde_json::Value::as_str)
    .is_some_and(|mode| mode != "none");
if has_real_delivery {
    return None;   // leaves an attacker-chosen delivery untouched
}
```

`src/cron/scheduler.rs:370-391` (fire-time send, no re-validation):
```rust
let target = delivery.to.as_deref()
    .ok_or_else(|| anyhow::anyhow!("delivery.to is required for announce mode"))?;
// ...
channel_impl.send(&SendMessage::new(output, target)).await?;
```

Repo conventions:
- The shell branch is the model for a creation-time gate: check, then return a
  `ToolResult { success:false, error: Some(reason) }` on refusal (see
  `src/tools/cron_add.rs:170-176`).
- The caller's conversation target (`channel_reply_target`) is only available in
  the **agent loop dispatch layer** (`src/agent/loop_.rs`), not inside
  `CronAddTool`. That is where the delivery cross-check must be enforced (Step 1).
- `effective_autonomy()` / `can_act()` gate mutations
  (`src/security/policy.rs`); the tool already routes mutations through
  `enforce_mutation_allowed` (`src/tools/cron_add.rs:19-53`).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0, no diff |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests   | `cargo test --lib cron` | all pass |
| Tests   | `cargo test --lib channels` | all pass |
| Drift   | `git diff --stat 2aefb9f..HEAD -- src/tools/cron_add.rs src/agent/loop_.rs src/cron/scheduler.rs` | only your changes |

Do **not** run a bare `cargo test` (disk-constrained). Scope with `--lib`.

## Scope

**In scope**:
- `src/agent/loop_.rs` — enforce a delivery-target cross-check for `cron_add`
  in the dispatch layer where `channel_reply_target` is known
- `src/cron/scheduler.rs` — `deliver_if_configured` fire-time re-validation
  hook (see Step 3)
- `src/tools/cron_add.rs` — minimal payload gate (ReadOnly refusal + provenance
  log) symmetric with the shell branch

**Out of scope**:
- Adding a `created_by` column / owner gate — that is plan 172.
- Widening or changing which channels support announce delivery — the gate at
  `deliver_if_configured` deliberately stays `channel_supports_announce_delivery`.
- Any change to `DeliveryConfig`'s shape in `src/cron/types.rs`.

## Git workflow

- Branch: `advisor/173-cron-add-agent-payload-delivery-gate`
- Conventional commits, one per step
  (e.g. `fix(cron): reject foreign delivery target on cron_add`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Reject a foreign delivery target at creation (dispatch layer)

The tool cannot see the caller's conversation, but the dispatch layer can:
`channel_reply_target` is already plumbed to `maybe_inject_channel_delivery`
(`src/agent/loop_.rs:1058`, called at 1241 and ~1360).

1. Add a sibling helper (next to `maybe_inject_channel_delivery`) e.g.
   `fn reject_foreign_cron_delivery(call, channel_name, reply_target) ->
   Option<String>` that returns `Some(reason)` when: the tool is `cron_add`,
   the call carries an explicit `delivery` with `mode != "none"`, AND the
   `delivery.to` differs from `reply_target` for the same `channel_name`
   (i.e. the model chose a target that is not the caller's own conversation).
2. Enforce it in the sequential dispatch path (the `for call in calls` loop
   that begins around `src/agent/loop_.rs:1252`, alongside the existing
   guest-gate check) **and** ensure the parallel fast-path (1235-1248) is not a
   bypass: a `cron_add` call with an explicit foreign delivery must be routed
   through the checked path (simplest: treat `cron_add` calls as non-parallel,
   or run the check before the parallel branch). On rejection, return a denied
   `ToolExecutionResult` (mirror how a guest-gate denial is turned into a
   result in the same loop).
3. **Escape hatch**: an owner turn (`guest_gate.is_none()`) may legitimately
   route cross-channel. Gate the rejection so it applies to **guest turns**, or
   to any turn when a new `channels_config` opt-out is *false*. Keep it simple:
   reject for guest turns; allow for owner turns. State this in the PR.

**Verify**: unit tests next to the existing `maybe_inject_channel_delivery`
tests (`src/agent/loop_.rs:2754-2800`): a `cron_add` call whose `delivery.to`
matches `reply_target` is allowed; one whose `to` is a different chat id is
rejected for a guest turn. `cargo test --lib` for the loop module → pass.

### Step 2: Minimal payload gate in the tool (symmetric with shell)

In `src/tools/cron_add.rs`, in the `JobType::Agent` branch (after the prompt
non-empty check at 185-194, before `enforce_mutation_allowed`):

1. Refuse agent-job creation when the effective autonomy is read-only. The tool
   already has `self.security`; use the same predicate the shell path relies on
   (`self.security.can_act()` is already checked inside `enforce_mutation_allowed`
   at line 20 — confirm an agent job also passes through it; it does, at 229).
   If read-only refusal is already covered by `enforce_mutation_allowed`, add
   an explicit early `can_act()` guard **only if** the agent branch could reach
   `add_agent_job` without it — verify by reading 229-231. If already covered,
   skip adding a duplicate and note it.
2. Add a provenance log line (`tracing::info!`) at agent-job creation recording
   that a cron **agent** job was created and its schedule kind — but **never log
   the full prompt** (it may carry injected content or secrets; log its length
   only). This gives operators an audit trail without leaking payload.

**Verify**: `cargo test --lib cron` → pass. Manually confirm no `tracing` line
prints the raw `prompt`.

### Step 3: Re-validate the delivery target at fire time

In `src/cron/scheduler.rs` `deliver_if_configured` (360-391), before the
`channel_impl.send(...)` at 391:

1. Keep the existing `channel_supports_announce_delivery` gate.
2. Add a defensive check that the resolved `target` is non-empty and
   well-formed for the channel (reuse whatever per-channel target validation
   already exists; if none, at minimum reject an empty/whitespace `to`). This is
   the belt-and-suspenders layer for jobs written before Step 1 shipped or via a
   path Step 1 does not cover (e.g. HTTP create — see plan 172/174).
3. Do **not** attempt owner/conversation matching here — the job has no live
   caller at fire time. The correctness gate is Step 1 at creation; Step 3 is
   the fail-safe.

**Verify**: `cargo test --lib cron` → pass. Add a test that
`deliver_if_configured` returns an error (not a panic) for an announce job with
an empty `to`.

## Test plan

- `src/agent/loop_.rs` tests (model after 2754-2800): matching target allowed;
  foreign target rejected for a guest turn; parallel path does not bypass.
- `src/tools/cron_add.rs` tests: read-only autonomy refuses agent-job creation;
  the prompt is never emitted in a log (assert via a captured subscriber if the
  harness supports it, else document the manual check).
- `src/cron/scheduler.rs` tests: `deliver_if_configured` rejects an empty `to`.
- Verification: `cargo test --lib cron` and `cargo test --lib channels` → all
  pass, including new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` and `cargo test --lib channels` pass; new tests exist
- [ ] A guest `cron_add` call with a delivery target that is not the caller's
      conversation is rejected; a matching target is allowed
- [ ] The raw agent-job `prompt` is never written to a log line
- [ ] `deliver_if_configured` fails safely (no panic) on an empty/foreign `to`
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report if:

- The "Current state" excerpts do not match live code (drift since 2aefb9f).
- Restricting `delivery.to` would break a legitimate cross-channel routing use
  case you can identify, and the owner escape hatch (Step 1.3) does not cover it
  — report the case before shipping a stricter gate.
- The parallel dispatch fast-path (1235-1248) cannot be made to honor the check
  without a larger refactor — report the scope rather than expanding it.
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- Cross-ref plan 172 (creator/origin + owner-gate) and plan 168 (delivery
  routing) — this plan closes the *target-choice* hole; 172 closes the
  *privilege* hole. Land both for full coverage.
- A reviewer should scrutinize: that the parallel fast-path is not a bypass, and
  that the owner escape hatch does not accidentally re-open the guest hole.
- Deferred: full per-channel address validation in `deliver_if_configured` if a
  channel-specific target format check does not already exist — Step 3 only adds
  the empty/whitespace guard.
