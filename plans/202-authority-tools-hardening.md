# Plan 202: Harden the authority-mutating tools — refuse wildcard owner, gate them under ReadOnly

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tools/manage_permissions.rs src/tools/issue_pairing_code.rs src/approval/permissions.rs src/approval/mod.rs`

## Status

- **Priority**: P1 (security — privilege escalation on an owner turn)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`manage_permissions` can mutate the very policy that authorizes it. Two gaps:

1. **Wildcard owner is only warned, not refused.** Adding owner `"*"` grants
   ownership (and the full tool surface) to **every** sender; the tool only
   emits a warning string (`manage_permissions.rs:202-211`), it does not refuse.
   A single prompt-injected owner turn can execute
   `{action:"add",target:"owner",value:"*"}` and open the agent to anyone.
2. **Authority mutation runs under ReadOnly/Strict with no guard.** The approval
   gate returns `needs_approval == false` under `ReadOnly` and delegates
   blocking to each tool (`approval/mod.rs:138-141`). Most acting tools enforce
   `can_act()`/`enforce_tool_operation`, but `manage_permissions` and
   `issue_pairing_code` contain **zero** such checks — so under the "Strict /
   deny-by-default, no prompts" preset an owner turn can still mutate the owner
   list or mint an owner-promoting pairing code. Both are owner-only, so this is
   a policy-consistency gap rather than a guest-reachable escalation — but
   "Strict" silently permitting authority writes is wrong.

`manage_permissions` can also add a basename to the shell allowlist
(`AllowCommand` → `config.autonomy.allowed_commands`), which then feeds the
shell gate. That widening is bounded (it can't touch `forbidden_paths`), but
combined with (2) it means a prompt-injected owner turn under Strict can
pre-approve commands.

## Current state

### Wildcard owner is warned, not refused — `src/tools/manage_permissions.rs:186-211`

```rust
    // permissions::apply(...) at :186 performs the mutation.
    // For owner "*", :202-211 only appends a warning string to the output;
    // there is no early return / refusal.
```

`Target::AllowCommand` routes into `config.autonomy.allowed_commands`
(`src/approval/permissions.rs:122-127`). The only hard guard is
last-owner-removal (`manage_permissions.rs:165-180`).

### No `can_act`/ReadOnly guard in the authority tools

`src/tools/manage_permissions.rs` and `src/tools/issue_pairing_code.rs` contain
zero `can_act()` / `enforce_tool_operation` / `ReadOnly` references (confirmed
by grep). Contrast `src/tools/memory_store.rs:138` and
`src/tools/git_operations.rs:529`, which gate on `can_act()`.

### The ReadOnly fail-open — `src/approval/mod.rs:138-141`

```rust
    // Under ReadOnly, needs_approval returns false ("blocks everything —
    // handled elsewhere"), delegating the block to each tool. Tools with no
    // independent guard therefore run.
```

## The fix

### Step 1 — refuse wildcard owner

In `manage_permissions` (or in `permissions::apply` for `Target::Owner`), refuse
a value of `"*"` (and any empty/whitespace-only value) outright with an error,
instead of warning. Wildcard ownership is never a safe interactive action from
a tool call. Keep the existing last-owner-removal guard.

If there is a legitimate operator need for a wildcard owner (there is a CLI
warning path for `owner '*'` at `src/main.rs:2544`), keep that CLI path as the
**only** way to set it; the tool refuses. State this split in the PR.

### Step 2 — gate the authority tools on `can_act()`

**Important — neither tool currently holds a `SecurityPolicy`, so you must
thread one in first.** `ManagePermissionsTool` holds only `config: Arc<Config>`
(`src/tools/manage_permissions.rs:40-49`) and `IssuePairingCodeTool` is a
fieldless unit struct (`src/tools/issue_pairing_code.rs:43-49`); the registry
builds them as `ManagePermissionsTool::new(config.clone())` and
`IssuePairingCodeTool::new()` (`src/tools/mod.rs:279,284`). So `self.security`
does not exist on these tools and a bare `self.security.can_act()` will not
compile.

Do this:

1. Add an `Arc<SecurityPolicy>` field to each tool and take it in `new(...)`
   (mirror how a tool that already holds a policy is constructed — e.g.
   `ShellTool`/`ProxyConfigTool` take `Arc<SecurityPolicy>`). Update the two
   registry call sites in `src/tools/mod.rs:279,284` to pass the same policy the
   other tools receive.
2. At the top of each tool's `execute`, refuse when `!self.security.can_act()`
   with a clear ReadOnly error. This makes ReadOnly/Strict deny them uniformly
   rather than relying on the (absent) approval prompt. (The gating helper is
   `can_act()`; a tool that additionally records an action uses
   `enforce_tool_operation(ToolOperation::Act, …)` as `memory_store.rs:138`
   does, and `git_operations` gates via `can_act()` at `git_operations.rs:519` —
   either is fine here; a plain `can_act()` refusal is sufficient.)

- `manage_permissions`: refuse when `!self.security.can_act()`.
- `issue_pairing_code`: same. Minting an owner-capable code is an authority
  grant; it must not run under ReadOnly.

If threading the policy into these two tools proves to ripple further than the
two `new(...)` sites + the two registry lines, STOP and report the blast radius
before proceeding — do not fake a `can_act()` call that does not compile.

### Step 3 (optional, if low-risk to wire) — force `manage_permissions` to prompt

If an `ApprovalManager` is reachable from the tool, additionally route
`manage_permissions` through an interactive approval independent of autonomy
level, so a prompt-injected owner turn cannot silently widen `allowed_commands`
even under Full. If wiring the backend is non-trivial, DEFER this to a
follow-up and note it — Steps 1+2 are the required floor.

## Files

- **In scope**: `src/tools/manage_permissions.rs`, `src/tools/issue_pairing_code.rs`,
  `src/tools/mod.rs` (the two registry call sites at :279,:284 — pass the policy),
  and possibly `src/approval/permissions.rs` (for the wildcard refusal, if that
  is the cleanest place).
- **Out of scope**: the `proxy_config` gate (plan 201), `cron_remove` denylist
  (plan 205), the allowlist-writer validation for the CLI/API surfaces
  (plan 209), the ReadOnly approval-flow semantics beyond adding per-tool
  guards.

## STOP conditions

- If `manage_permissions`/`issue_pairing_code` already `can_act()`-gate (drift),
  skip Step 2 and report.
- If refusing wildcard owner in `permissions::apply` breaks the CLI `owner '*'`
  path (which intentionally warns and proceeds), scope the refusal to the
  **tool** call site only, not the shared `apply`, and report.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib tools::manage_permissions tools::issue_pairing_code approval` passes with new tests.
4. New tests:
   - `manage_permissions` with `{action:"add",target:"owner",value:"*"}` returns
     an error and does not add `*` to owners.
   - Under a ReadOnly policy, `manage_permissions` and `issue_pairing_code`
     return the ReadOnly/blocked error and mutate nothing.
   - A normal owner add (a concrete sender id) under Supervised still succeeds
     (no regression).

## Test plan

Mirror the existing `manage_permissions` tests (they build a policy + config and
call the tool). Add the wildcard-refusal case, the ReadOnly-deny case for both
tools, and the no-regression normal-add case. Use neutral fixture ids
(`rantaiclaw_user`, `test_owner`) per the repo's identity-safe naming rule.

## Risk & rollback

- **Risk**: MED — Step 2 makes both tools inert under ReadOnly, which is the
  intent; verify no legitimate ReadOnly workflow depended on minting a pairing
  code (it should use the CLI). Step 1 removes a (dangerous) capability from the
  tool surface; the CLI retains it.
- **Rollback**: revert the touched tool files; no schema/config/migration change.

## Maintenance note

Authority-mutating tools (owner list, pairing codes, permissions, allowlist)
must each carry their own `can_act()` guard because the ReadOnly path delegates
blocking to the tool. A test that enumerates `OWNER_ONLY_TOOLS` and asserts each
is inert under ReadOnly would prevent the next such omission — consider adding
it as a follow-up.
