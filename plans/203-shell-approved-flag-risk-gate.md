# Plan 203: Stop the model-supplied `approved` flag from self-clearing the shell risk gate

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tools/shell.rs src/security/policy.rs`

## Status

- **Priority**: P2 (security — model self-approves risky allowlisted commands)
- **Effort**: S
- **Risk**: MED (changes the approval contract for allowlisted risky commands)
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The `shell` tool exposes an `approved` boolean in its parameter schema
(`src/tools/shell.rs:272-290`), described as "Set true to explicitly approve
medium/high-risk commands". The **model** supplies tool arguments, so the model
can set `approved=true` itself. For an **allowlisted** basename,
`validate_command_execution` (`src/security/policy.rs:618-638`) treats that
model-supplied boolean as the "explicit approval" that lets a Medium/High-risk
command run without the human decision the wording implies.

There is no human in that specific loop. The allowlist and the cascading human
approval for *non*-allowlisted basenames are unaffected, so the blast radius is
"allowlisted-but-risky commands under Supervised" — but the `approved` flag
lets the model wave its own risk gate, which is not what "explicit approval"
should mean.

## Current state

### The schema advertises `approved` — `src/tools/shell.rs:272-290`

```rust
    // schema: "approved": { type boolean, "Set true to explicitly approve
    //   medium/high-risk commands" }
    // execute(): reads `approved` from args at :287-290 and passes it to
    //   validate_command_execution(command, approved)
```

### It satisfies the risk gate — `src/security/policy.rs:618-638`

(Read the live code — the shape is roughly:)

```rust
        // High branch has its OWN Supervised+!approved refusal (not a bare
        // "else falls through"); Medium branch is gated on
        // require_approval_for_medium_risk + Supervised + !approved.
        if risk == CommandRiskLevel::High {
            if self.fields().block_high_risk_commands { /* deny */ }
            // then, when Supervised && !approved => Err (requires approval)
        }
        if risk == CommandRiskLevel::Medium
            && !approved                       // <-- model-supplied `approved` skips the gate
            && self.fields().require_approval_for_medium_risk
            && /* Supervised */
        { /* require approval */ }
```

The exact structure is at `policy.rs:618-638` — confirm it against the live
file. The load-bearing point: for an allowlisted command, `approved=true`
satisfies the Medium/High approval requirement, so the model can wave its own
risk gate without a human decision.

## The fix

The correct source of "explicit approval" is the human approval backend (the
same path non-allowlisted basenames already use at `shell.rs:346`), not a
model-supplied boolean. Choose the least-disruptive correct option:

### Option A (preferred) — drive risk approval through the human backend

Route Medium/High-risk allowlisted commands through the same
`approvals.request_decision_in(...)` human decision that non-allowlisted
basenames already trigger, and ignore the model-supplied `approved` for the
risk gate. The `approved` parameter is then either removed from the schema or
repurposed as a no-op the runtime ignores (document which).

### Option B (smaller, if A is too invasive) — remove `approved` from the model surface

Remove `approved` from the tool's public parameter schema so the model cannot
set it, and pass `approved=false` from the shell tool unconditionally into
`validate_command_execution`. Any legitimate operator "pre-approval" then comes
from the allowlist or a lowered autonomy level, not a per-call model boolean.
This is a strict tightening: Medium-risk allowlisted commands will prompt (or be
blocked) under Supervised where they previously could self-approve.

Pick Option A if an approval backend is readily reachable at that call site
(it is, for the cascading loop). Otherwise Option B.

Under a non-interactive backend (channels/gateway), the human decision defaults
to deny, which fails safe.

## Files

- **In scope**: `src/tools/shell.rs` (the `approved` param + how it is passed),
  and — only for Option A — the risk-approval wiring. `src/security/policy.rs`
  signature stays unless Option B removes the `approved` parameter entirely.
- **Out of scope**: the allowlist matcher, the risk classifier (plan 200), the
  non-allowlisted cascading loop (already correct).

## STOP conditions

- If removing/ignoring `approved` breaks a test that asserts a Medium-risk
  allowlisted command runs with `approved=true` and no prompt, STOP and report —
  that test encodes the behavior this plan intentionally changes; update it to
  expect a human decision (A) or a prompt/block (B).
- If `approved` is consumed anywhere other than the risk gate (search for its
  readers), account for those before changing the signature.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib tools::shell security::policy` passes with
   updated/new tests.
4. New/updated tests:
   - Under Supervised with `require_approval_for_medium_risk=true`, an
     allowlisted Medium-risk command with a model-supplied `approved=true` does
     NOT execute without a human decision (Option A: it requests one; Option B:
     it prompts/blocks).
   - A Low-risk allowlisted command still runs without a prompt (no regression).

## Test plan

Mirror the shell tool's existing approval tests (they inject a decision via a
test backend). Add the "model-approved risky command still needs a human
decision" case. Keep the Low-risk no-regression case.

## Risk & rollback

- **Risk**: MED — this changes the approval contract for allowlisted risky
  commands; some flows that relied on `approved=true` will now prompt. That is
  the intended hardening; call it out in the PR + CHANGELOG.
- **Rollback**: revert `shell.rs` (and the small policy change if Option A);
  no schema/config/migration change.

## Maintenance note

Any "explicit approval" signal on a tool must originate from the human/operator
path, never from model-supplied arguments — the model controls the args, so a
model-settable approval is self-approval. Reviewers should reject new tool
parameters that let the model satisfy its own gate.
