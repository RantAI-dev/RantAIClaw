# Plan 007: Close the git allowlist gap for `-u`/abbreviated `--upload-pack` and un-gated transport verbs

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/security/policy.rs`
> If the file changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The shell command allowlist blocks the arbitrary-program-execution vectors
`git … --upload-pack=…` / `--receive-pack=…` / `--exec=…` (against a local path,
git launches the named program via the shell = RCE that bypasses the allowlist).
But it only matches the exact long spellings. Git also accepts the **short alias
`-u <program>`** for `--upload-pack` (documented for `clone`/`fetch`/`ls-remote`/
`pull`), and git's parse-options accepts **unambiguous prefix abbreviations** of
long options — neither of which the literal string checks catch. Worse, the
transport verbs `clone`/`fetch`/`ls-remote`/`pull` are not in the "medium" risk
set, so such a command is classified Low and never even hits the approval
prompt. This is the same allowlist-completeness class that has repeatedly yielded
real bypasses in this repo (`find -exec`, the long-form `--upload-pack`). The
in-code comment already acknowledges the RCE; this closes the remaining forms.

## Current state

- `src/security/policy.rs:707-732` — the `git` arm of `is_args_safe`:
  ```rust
  "git" => {
      // ... comment explaining --upload-pack/--receive-pack/--exec = RCE ...
      !args.iter().any(|arg| {
          arg == "config"
              || arg.starts_with("config.")
              || arg == "alias"
              || arg.starts_with("alias.")
              || arg == "-c"
              || arg == "--upload-pack"
              || arg.starts_with("--upload-pack=")
              || arg == "--receive-pack"
              || arg.starts_with("--receive-pack=")
              || arg == "--exec"
              || arg.starts_with("--exec=")
      })
  }
  ```
  Not covered: `-u` (short for `--upload-pack`), and abbreviated long forms
  (e.g. `--upload-pac=…`, `--up=…` if unambiguous). `args` are already
  lowercased by the caller (per the comment at line 718).

- `src/security/policy.rs:488-520` — `command_risk_level`: the "medium" git
  verbs list. The auditor found `clone`/`fetch`/`ls-remote`/`pull` **absent**,
  so a git transport command is classified Low and skips the approval gate. Read
  this function to confirm the exact structure and the current medium-verb list
  before editing.

- Tests to extend: `src/security/policy.rs:1675-1676` currently assert only the
  full `--upload-pack=` spelling is blocked. Read the surrounding test module
  (`grep -n "upload-pack\|is_args_safe\|command_risk_level" src/security/policy.rs`).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Policy tests | `cargo test policy` | all pass, incl. new |

## Scope

**In scope**:
- `src/security/policy.rs` — the `git` arm of `is_args_safe` and the git
  medium-verb list in `command_risk_level`, plus tests in the same file.

**Out of scope** (do NOT touch):
- The `find` arm, path validation, or other command arms.
- The default `command_allowlist` in `src/config/schema.rs` — the allowlist
  membership is fine; this is about per-arg filtering and risk classification.
- Over-engineering a full git option parser — a targeted match on the dangerous
  option identities is sufficient (see Step 1).

## Git workflow

- Branch: `advisor/007-git-allowlist-upload-pack-shortforms`
- One commit; message e.g.
  `security(policy): block git -u/abbreviated upload-pack and risk-gate transport verbs`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Reject the `-u` short form and abbreviated long forms

Extend the `git` arm's `any(...)` predicate. Add:
- `arg == "-u"` (short for `--upload-pack`; the program is the *next* arg, but
  rejecting the flag itself is enough to block the command).
- Any `arg` that, with a leading `--` and up to `=value` stripped, is a
  non-empty **prefix of** `"upload-pack"`, `"receive-pack"`, or `"exec"` and is
  at least, say, 4 chars after `--` (to avoid over-blocking unrelated flags).
  Concretely, add a small helper:

```rust
fn is_dangerous_git_long_opt(arg: &str) -> bool {
    // args are already lowercased
    let Some(rest) = arg.strip_prefix("--") else { return false };
    let name = rest.split('=').next().unwrap_or(rest);
    if name.len() < 4 { return false; }               // avoid over-broad matches
    ["upload-pack", "receive-pack"].iter().any(|full| full.starts_with(name))
        || (name.len() >= 4 && "exec".starts_with(name))
}
```
Then in the predicate add `|| arg == "-u" || is_dangerous_git_long_opt(arg)`.
Keep all existing exact-match arms (they still catch `config`, `-c`, etc.).

Note: `"exec".starts_with(name)` with `name.len() >= 4` only matches `exec`
itself; that is intentional (git has other `--exec*`-unrelated options? there
are none dangerous here — keep it exact for `exec`). The prefix logic mainly
matters for the longer `upload-pack`/`receive-pack` names.

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 2: Risk-gate the git transport verbs

In `command_risk_level` (`policy.rs:488-520`), add `clone`, `fetch`,
`ls-remote`, and `pull` to the git medium-risk verb set so a transport command
that somehow carries a program-naming flag (or any transport command) hits the
approval prompt instead of being classified Low. Match the existing structure
exactly (read how the current medium verbs are represented — a slice, a match,
etc. — and extend that).

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

## Test plan

- Extend the policy test module (same file). Add assertions:
  1. `git_upload_pack_short_flag_blocked`: `is_args_safe("git", &["clone", "-u", "/x/evil", "."])`
     returns false (unsafe).
  2. `git_upload_pack_abbrev_blocked`: an abbreviated long form (e.g.
     `--upload-pac=/x`) returns false.
  3. `git_receive_pack_abbrev_blocked`: similar for receive-pack.
  4. `git_normal_clone_still_allowed`: `is_args_safe("git", &["clone", "https://example.com/r.git"])`
     returns true (no false positive on ordinary clones).
  5. `git_transport_verbs_are_risk_gated`: `command_risk_level` for
     `git clone …` / `git fetch …` is at least Medium (whatever the enum variant
     is named — read it).
  - Keep the existing `--upload-pack=` test passing.
  - **Prove the guard is not vacuous**: before finalizing, temporarily revert
    Step 1 locally and confirm test #1 fails; then re-apply. (Do not commit the
    revert.)
- Verification: `cargo test policy` → all pass including the new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test policy` passes; new tests for `-u`, abbreviated forms, and
      transport-verb risk-gating exist and pass
- [ ] `git_normal_clone_still_allowed` passes (no false positive)
- [ ] Only `src/security/policy.rs` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `git` arm or `command_risk_level` structure does not match the excerpts
  (drift since `4d35107`).
- Adding `clone`/`fetch`/`pull` to the medium set causes a flood of existing
  tests to fail in a way suggesting these verbs are relied upon as Low elsewhere
  — report the coupling rather than forcing it.
- The prefix-abbreviation matching starts blocking legitimate common flags
  (a test surfaces a false positive) — narrow the helper and report the case.

## Maintenance notes

- This is defense-in-depth: the end-to-end exploitability of `-u` depends on
  git's local-transport handling, which could not be run in the audit
  environment (confidence MED). Blocking the flag is cheap and correct
  regardless.
- The recurring lesson (see prior `find -exec` / `--upload-pack` fixes): any
  allowlisted binary that can execute a named program from a flag is an RCE
  vector — when adding binaries to the default allowlist, audit their
  program-spawning flags. Consider a follow-up sweep of other allowlisted
  binaries (`npm`/`cargo` run-scripts, etc.) — noted, not in scope here.
- Reviewer should confirm the abbreviation helper's minimum-length guard is
  tight enough to avoid false positives yet catches real prefixes.
