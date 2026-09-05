# Plan 200: Reclassify code-executing git/cargo/npm subcommands as Medium risk

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/policy.rs`

## Status

- **Priority**: P1 (security — silent code execution under Supervised)
- **Effort**: M
- **Risk**: MED (UX: `cargo build`/`npm run` will start prompting)
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`command_risk_level` classifies commands so the Supervised gate knows what to
prompt for. Its Medium-risk verb lists cover state-changing subcommands, but
they **omit the subcommands that execute arbitrary code**, so those classify
`Low` and — for an allowlisted base — run with **no approval prompt**:

- `cargo build` / `cargo run` / `cargo test` — a workspace `build.rs` (writable
  in-workspace via `file_write`) executes arbitrary code at build time;
  `run`/`test` execute arbitrary project/test code.
- `npm run <script>` / `npm test` / `npm exec` — run arbitrary `package.json`
  scripts.
- `git bisect run <prog>`, `git submodule foreach '<cmd>'`,
  `git filter-branch` — run an arbitrary program.

This is the sibling of the trap the code comment at `command_risk_level`
already warns about for global options: the verb scan is correct, but the verb
**lists** are missing the command-executing entries. Making these Medium means a
Supervised session (with the default `require_approval_for_medium_risk = true`)
hits the approval gate instead of executing silently.

## Current state

### Medium verb lists — `src/security/policy.rs:544-585`

```rust
            let medium = match base.as_str() {
                "git" => args.iter().any(|verb| matches!(verb.as_str(),
                    "commit" | "push" | "reset" | "clean" | "rebase" | "merge"
                    | "cherry-pick" | "revert" | "branch" | "checkout" | "switch"
                    | "tag" | "clone" | "fetch" | "ls-remote" | "pull"
                    // MISSING: bisect, submodule, filter-branch
                )),
                "npm" | "pnpm" | "yarn" => args.iter().any(|verb| matches!(verb.as_str(),
                    "install" | "add" | "remove" | "uninstall" | "update" | "publish"
                    // MISSING: run, test, exec, ci, exec, dlx (pnpm/yarn)
                )),
                "cargo" => args.iter().any(|verb| matches!(verb.as_str(),
                    "add" | "remove" | "install" | "clean" | "publish"
                    // MISSING: run, build, test, bench, +anything that runs build.rs
                )),
                "touch" | "mkdir" | "mv" | "cp" | "ln" => true,
                _ => false,
            };
```

The comment above this block already establishes the intent: state-changing
verbs must be Medium so an unapproved Supervised session hits the gate. Code
execution is strictly more dangerous than state change, so these belong here.

## The fix

Extend the three verb lists to include the code-executing subcommands:

- **git**: add `bisect`, `submodule`, `filter-branch`.
- **cargo**: add `run`, `build`, `test`, `bench` (all can execute `build.rs` or
  project/test code).
- **npm/pnpm/yarn**: add `run`, `test`, `exec`, `ci`; for pnpm/yarn also `dlx`.

Keep the "scan all args for the verb" approach (it already handles leading
global options). A rare false positive only over-prompts, which is the safe
direction, as the existing comment notes.

Do **not** attempt to whitelist "safe" scripts (e.g. `npm run lint`) — the
script body is arbitrary and defined in-repo; treat the whole class as Medium.

## Files

- **In scope**: `src/security/policy.rs` — the `medium` match in
  `command_risk_level` only.
- **Out of scope**: the High-risk list, `is_args_safe` (plan 196), the
  allowlist matcher, any UX/prompt code.

## STOP conditions

- If a test asserts that `cargo build` (or `npm run`, `git bisect`) is
  `Low`/auto-approved by design, STOP and report — that test encodes the
  behavior this plan removes; it must be updated, not the plan abandoned.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib security::policy` passes with new tests.
4. New tests assert the new classifications:

```rust
#[test]
fn code_executing_subcommands_are_medium() {
    let p = /* a policy; command_risk_level does not need allowlisting */;
    for cmd in [
        "cargo build", "cargo run --release", "cargo test",
        "npm run deploy", "npm test", "npm exec foo",
        "git bisect run ./x.sh", "git submodule foreach 'sh -c x'",
    ] {
        assert_eq!(p.command_risk_level(cmd), CommandRiskLevel::Medium, "{cmd}");
    }
}

#[test]
fn read_only_git_stays_low() {
    let p = /* policy */;
    assert_eq!(p.command_risk_level("git status"), CommandRiskLevel::Low);
    assert_eq!(p.command_risk_level("git log --oneline"), CommandRiskLevel::Low);
}
```

Verify `code_executing_subcommands_are_medium` FAILS before the edit.

## Test plan

Add the two tests to the `command_risk_level` test area in `policy.rs`.
`command_risk_level` is a pure classifier and needs no allowlist setup, so the
fixture is minimal.

## Risk & rollback

- **Risk**: MED — the common dev loop (`cargo build`, `npm run`) will now prompt
  under Supervised. That is the intended behavior (these execute arbitrary
  in-repo code); operators who want them unprompted can allowlist the exact
  basename and/or lower autonomy. Call this out in the PR + CHANGELOG.
- **Rollback**: single-file revert; no schema/config/migration change.

## Maintenance note

When a new build/task-runner tool becomes common (e.g. `bun run`, `uv run`,
`make`), add its code-executing subcommands here. The rule of thumb: any
subcommand that runs a repo-defined script or a `build.rs`-style hook is at
least Medium.
