# Plan 208: Validate command-allowlist writes — reject multi-token entries, warn on dangerous basenames (CLI + API)

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/approval/permissions.rs src/security/policy.rs src/gateway/config_api.rs src/main.rs`

## Status

- **Priority**: P2 (correctness + safety — dead entries and unguarded widening)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug / security
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The shell gate matches the owner command allowlist (`config.autonomy.allowed_commands`)
by **basename** with exact equality. But the writers that populate that list do
not enforce the basename shape, producing two problems:

1. **Silently dead entries.** `permissions add allow-command "git status"`
   (CLI or TUI) or a config-API `allowed_commands: ["git status"]` stores
   `"git status"` verbatim. The reader compares it against a basename (`git`),
   so it can never match — the operator gets a confirmed-but-inert entry. The
   runtime `/allow` path already rejects multi-token input
   (`add_runtime_command`: "basename must be a single token"); the
   `permissions add allow-command` and config-API paths do not.
2. **No caution when widening to a dangerous basename.** `permissions add
   allow-command rm` (or `dd`, `sudo`, `bash`) is accepted with no warning. The
   only loud warning today is for owner `"*"`. High-risk basenames still route
   through the risk gate under Supervised (so an allowlisted `rm` is not
   auto-run at default), but Medium/Low destructive basenames, or any basename
   once autonomy is `off`/Full, execute with no further confirmation — and the
   operator gets no signal they widened the blast radius.

## Current state

### `permissions::apply` stores the value verbatim — `src/approval/permissions.rs:113-160`

```rust
pub fn apply(config: &mut Config, target: Target, op: Op, value: &str) -> ChangeOutcome {
    let value = normalize(target, value);   // normalize() only trims
    ...
    let list = match target {
        ...
        Target::AllowCommand => &mut config.autonomy.allowed_commands,
    };
    // Op::Add pushes `value` with no token/shape validation.
```

### The runtime path DOES validate — `src/security/policy.rs:1063-1071` (mirror this)

```rust
    pub fn add_runtime_command(&self, basename: &str, persist: bool) -> anyhow::Result<()> {
        let basename = basename.trim();
        if basename.is_empty() { anyhow::bail!("runtime allowlist: empty basename"); }
        if basename.contains(char::is_whitespace) {
            anyhow::bail!("runtime allowlist: basename must be a single token");
        }
        ...
```

### The config API replaces the list with no validation — `src/gateway/config_api.rs:369-371`

```rust
    if let Some(v) = body.allowed_commands {
        cfg.autonomy.allowed_commands = v;   // no per-entry basename validation
    }
```

## The fix

### Step 1 — validate basename shape in `permissions::apply` for `AllowCommand`

For `Target::AllowCommand`, before pushing:

- Strip any path prefix to a basename (`rsplit('/').next()`), matching how the
  gate reduces the command.
- Reject a value containing whitespace or shell/glob metacharacters (mirror
  `add_runtime_command`): return a `changed: false` outcome with a clear message
  ("allow-command must be a single basename, e.g. `docker` not `docker ps`").

Keep the other targets (`Owner`, `GuestTool`, `GuestCommand`) unchanged — this
validation is specific to the basename-matched allowlist.

### Step 2 — warn (don't block) on a dangerous basename

When an `AllowCommand` add targets a basename in the High-risk set (the list in
`command_risk_level`, `policy.rs:496-528`: `rm`, `dd`, `sudo`, `su`, `chmod`,
`chown`, `mkfs`, `curl`, `wget`, `nc`, `ssh`, …), still add it, but return a
message that flags the widening ("⚠ allowlisting a high-risk command — it will
run without the risk prompt when autonomy is off/full"). Mirror the tone of the
existing owner-`"*"` warning at `src/main.rs:2544`.

Expose the High-risk basename set from `policy.rs` (a small `pub(crate)` helper
or const) so both the risk classifier and this warning read one source.

### Step 3 — apply the same validation at the config-API boundary

In `set_autonomy` (`config_api.rs:369`), validate each `allowed_commands` entry
with the same basename check before assigning; reject the request (400) or drop
invalid entries with a warning field. Rejecting is cleaner (the client sent a
malformed entry). Do not silently store multi-token entries.

## Files

- **In scope**: `src/approval/permissions.rs` (validation), `src/security/policy.rs`
  (expose the High-risk basename set), `src/gateway/config_api.rs` (API-boundary
  validation). The CLI `permissions add` and TUI `/permissions add` both route
  through `permissions::apply`, so they are fixed transitively.
- **Out of scope**: the `/allow` runtime path (already validates), the glob-vs-
  basename enforcement question (plan 206), the claw-ui panel (plan 213).

## STOP conditions

- If `permissions::apply` already validates single-token basenames (drift), skip
  Step 1 and report.
- If exposing the High-risk set from `policy.rs` would require making a large
  internal list public, keep it `pub(crate)` and note it; do not widen the API
  surface more than needed.

## Done criteria

1. `cargo fmt`/`clippy`/`cargo test -p rantaiclaw --lib approval::permissions security::policy gateway` clean.
2. New tests:

```rust
#[test]
fn allow_command_rejects_multi_token() {
    let mut cfg = Config::default();
    let out = apply(&mut cfg, Target::AllowCommand, Op::Add, "git status");
    assert!(!out.changed);
    assert!(!cfg.autonomy.allowed_commands.iter().any(|c| c == "git status"));
}

#[test]
fn allow_command_strips_path_to_basename() {
    let mut cfg = Config::default();
    let out = apply(&mut cfg, Target::AllowCommand, Op::Add, "/usr/bin/docker");
    assert!(out.changed);
    assert!(cfg.autonomy.allowed_commands.iter().any(|c| c == "docker"));
}

#[test]
fn allow_command_warns_on_dangerous_basename() {
    let mut cfg = Config::default();
    let out = apply(&mut cfg, Target::AllowCommand, Op::Add, "rm");
    assert!(out.changed);                    // still added
    assert!(out.message.contains("high-risk") || out.message.contains("⚠"));
}
```

Plus a config-API test asserting `allowed_commands: ["git status"]` is rejected
or sanitized (mirror the existing `config_api` autonomy tests).

## Test plan

Add the `permissions::apply` tests to that module; add the API test alongside
the existing `set_autonomy` tests in `gateway/config_api.rs`. Use neutral
fixtures.

## Risk & rollback

- **Risk**: LOW — rejecting malformed entries and warning on dangerous ones is a
  strict improvement; the warning does not block, preserving operator power.
- **Rollback**: revert the three files; no schema/migration change.

## Maintenance note

The runtime `/allow` path, `permissions add allow-command`, and the config API
should share one basename-validation helper so they cannot drift again — this
plan aligns three writers to the shape the single reader expects. Consider
extracting `validate_allow_basename(&str) -> Result<String>` and calling it from
all three.
