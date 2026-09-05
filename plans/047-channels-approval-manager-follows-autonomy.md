# Plan 047: Make the channels owner-approval gate follow an autonomy reload

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 3edb236..HEAD -- src/approval/mod.rs src/channels/mod.rs src/security/policy.rs`
> If any of those changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `3edb236`, 2026-07-27

## Why this matters

On the channels/daemon runtime, the object that decides *whether a tool call
needs the owner's in-chat approval* is built once when the daemon starts and
never rebuilt. The autonomy hot-reload updates `SecurityPolicy` but not this
object.

The failure is **fail-open on a tightening change**, which is the worst
direction:

1. Daemon starts while autonomy is `off` (i.e. `Full`).
2. Operator runs `rantaiclaw autonomy smart` to tighten.
3. `SecurityPolicy` correctly moves to `Supervised` — `can_act()` now returns
   true and command gating resumes.
4. The approval manager still holds `Full`. `needs_approval` short-circuits to
   `false` for `Full`, so **no approval is ever requested** and the configured
   owner is never asked.

So the operator tightens the setting and silently loses the owner-approval
gate until the daemon restarts. The owner list itself (`/claim` →
`approval_owners`) is already hot-reloaded correctly; only the "do we need to
ask at all" decision is stale.

## Current state

Files involved:

- `src/approval/mod.rs` — `ApprovalManager`, which caches the autonomy level.
- `src/channels/mod.rs` — builds the manager once at startup, stores it on the
  runtime context, and consults it per inbound message.
- `src/security/policy.rs` — already carries a live autonomy override.

The manager caches the level as a plain field —
`src/approval/mod.rs:58-69`:

```rust
pub struct ApprovalManager {
    /// Tools that never need approval (from config).
    auto_approve: HashSet<String>,
    /// Tools that always need approval, ignoring session allowlist.
    always_ask: HashSet<String>,
    /// Autonomy level from config.
    autonomy_level: AutonomyLevel,
    /// Session-scoped allowlist built from "Always" responses.
    session_allowlist: Mutex<HashSet<String>>,
    /// Audit trail of approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
}
```

`src/approval/mod.rs:71-81`:

```rust
impl ApprovalManager {
    /// Create from autonomy config.
    pub fn from_config(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
        }
    }
```

The two short-circuits that make a stale level dangerous —
`src/approval/mod.rs:86-96`:

```rust
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        // Full autonomy never prompts.
        if self.autonomy_level == AutonomyLevel::Full {
            return false;
        }

        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }
```

Built once at daemon start — `src/channels/mod.rs:3720-3726`:

```rust
        channel_approval: if config.channels_config.autonomous_tools {
            None
        } else {
            Some(Arc::new(crate::approval::ApprovalManager::from_config(
                &config.autonomy,
            )))
        },
```

Consulted on every inbound message — `src/channels/mod.rs:1923`:

```rust
                ctx.channel_approval.as_deref(),
```

And it gates whether the owner is asked at all —
`src/channels/mod.rs:1881-1883`:

```rust
    let chat_relay_backend =
        if ctx.channel_approval.is_some() && !runtime_defaults.approval_owners.is_empty() {
```

The reload path patches only the policy, never this manager —
`src/channels/mod.rs:671-676` (inside `maybe_apply_runtime_config_update`):

```rust
    ctx.security
        .set_allowed_commands(next_defaults.allowed_commands.as_ref().clone());
```
```rust
    ctx.security.set_autonomy(next_defaults.autonomy_level);
```

The live-override mechanism this plan will reuse —
`src/security/policy.rs:637-639`:

```rust
    pub fn effective_autonomy(&self) -> AutonomyLevel {
        self.autonomy_runtime.read().unwrap_or(self.autonomy)
    }
```

Repo conventions to match:

- Optional collaborators attached after construction use
  `Arc<RwLock<Option<...>>>` — see `SecurityPolicy::pending` and
  `SecurityPolicy::set_pending` at `src/security/policy.rs:114` and `:993`.
- `parking_lot` locks; guards do not return `Result`.
- Tests live in-file under `#[cfg(test)] mod tests`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no output |
| Lint (same as CI) | `cargo clippy --locked --all-targets -- -D clippy::correctness` | exit 0 |
| Unit tests | `cargo test --lib` | exit 0, all pass |
| Focused tests | `cargo test --lib approval` | all pass |

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

- `src/approval/mod.rs`
- `src/channels/mod.rs`
- `plans/README.md` — append the status row for this plan (the table currently
  ends at row `045` today, and at `046` once plan 046 has run — append to
  whatever the last row is, do not assume `045`). Append exactly:

  ```
  | 047 | Make the channels owner-approval gate follow an autonomy reload | P1 | S | MED | — | security | TODO |
  ```

**Out of scope** (do NOT touch):

- `src/gateway/mod.rs` and `src/gateway/api_v1.rs` — those rebuild their
  `ApprovalManager` per turn/request already and are correct. Do not
  "harmonise" them here.
- `auto_approve` / `always_ask` staleness on channels. Real, same family,
  deliberately deferred — this plan fixes the level only, because the level is
  the one with the fail-open short-circuits.
- The boot-pinned channels tool registry and system prompt. Separate plan.
- `src/security/policy.rs` — you read from it, you do not modify it.

## Git workflow

- Branch: `fix/channels-approval-follows-autonomy`
- Conventional commit titles. Example from this repo's history:
  `fix(channels): let a config reload narrow the shell allowlist`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

> **Imports first.** `src/approval/mod.rs` today imports `AutonomyLevel`
> (`:13`) but **not** `std::sync::Arc`. Add `use std::sync::Arc;` before you
> start, or the first `cargo check` fails on an unresolved name rather than on
> anything this plan is about.
>
> Do **not** also add `use crate::security::SecurityPolicy;`. The snippets below
> spell the type fully-qualified (`Arc<crate::security::SecurityPolicy>`), so
> that import would be unused in the lib target — and CI's strict-delta gate
> (`scripts/ci/rust_strict_delta_gate.sh`, `-D warnings` on the lines your diff
> touches) fails on exactly that. Keep the fully-qualified form, or import the
> type *and* switch every snippet to the short form; do not mix.

### Step 1: Let the manager read a live autonomy source

In `src/approval/mod.rs`, add an optional policy handle to `ApprovalManager`
and have `needs_approval` prefer it over the cached field. Keep the cached
field as the fallback so every existing caller keeps working unchanged.

Target shape:

```rust
pub struct ApprovalManager {
    // ... existing fields ...
    /// Live autonomy source. When set, `needs_approval` reads through this
    /// instead of the boot-time `autonomy_level`, so a config hot-reload that
    /// tightens autonomy also tightens the approval gate. `None` keeps the
    /// snapshot behaviour for callers that rebuild per turn anyway.
    policy: Option<Arc<crate::security::SecurityPolicy>>,
}
```

Add a builder that attaches it, next to `from_config`:

```rust
    /// Attach a live policy so the autonomy level is read at decision time.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<crate::security::SecurityPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }
```

Add a private accessor and use it in `needs_approval` in place of both
`self.autonomy_level` reads:

```rust
    fn effective_autonomy(&self) -> AutonomyLevel {
        match &self.policy {
            Some(p) => p.effective_autonomy(),
            None => self.autonomy_level,
        }
    }
```

Set `policy: None` in `from_config`. Do not change any other method.

**Verify**: `cargo check --all-targets` → exit 0. (`--all-targets` compiles the
test modules too; `build --lib` would report success while a test module is
broken.)

### Step 2: Wire it on the channels path

In `src/channels/mod.rs` at the construction site (`:3720-3726`), chain
`.with_policy(...)` using the same `Arc<SecurityPolicy>` that is stored as
`ctx.security`.

**The local is named `security`**, bound at `src/channels/mod.rs:3243` inside
`start_channels_with_cancellation` (fn starts at `:3193`):
`let security = Arc::new(SecurityPolicy::from_config_with_policy_dir(`. It is
the only `let security` binding in the file, and the same struct literal you
are editing already reads `security: Arc::clone(&security),` a few lines above
at `:3715`. Use `Arc::clone(&security)`.

The resulting expression should still be an
`Option<Arc<ApprovalManager>>` — wrap after the builder call, e.g.
`Some(Arc::new(ApprovalManager::from_config(&config.autonomy).with_policy(Arc::clone(&security))))`.

Do **not** change `:1881-1883` or `:1923`; they keep working as-is.

**Verify**: `cargo check --all-targets` → exit 0. (`--all-targets` compiles the
test modules too; `build --lib` would report success while a test module is
broken.)

### Step 3: Add the regression tests

See "Test plan". Write them before running the full suite.

**Verify**: `cargo test --lib approval` → all pass.

### Step 4: Full verification

**Verify**, all three:

- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --locked --all-targets -- -D clippy::correctness` → exit 0
- `cargo test --lib` → exit 0

## Test plan

Add to the `#[cfg(test)] mod tests` block in `src/approval/mod.rs`:

1. `needs_approval_follows_a_live_autonomy_tightening` — build a
   `SecurityPolicy` whose autonomy is `Full`, build an `ApprovalManager` from
   a matching `AutonomyConfig` and attach the policy with `with_policy`.
   Assert `needs_approval("shell")` is `false`. Then call
   `policy.set_autonomy(AutonomyLevel::Supervised)` and assert
   `needs_approval("shell")` is now `true`. **This is the exact fail-open
   scenario**: on the pre-fix code the second assertion fails.

2. `needs_approval_follows_a_live_autonomy_loosening` — the mirror: start
   `Supervised` (expect `true` for a tool that is not auto-approved), call
   `set_autonomy(AutonomyLevel::Full)`, assert `false`.

3. `needs_approval_without_a_policy_uses_the_snapshot` — build with plain
   `from_config` (no `with_policy`) and assert behaviour is unchanged from
   today for `Full`, `ReadOnly`, and `Supervised`. This pins that per-turn
   callers are unaffected.

**How to build the inputs — read this before writing the tests.** The test
module in `src/approval/mod.rs` already has two config helpers you should use:
`supervised_config()` at `:342` and `full_config()` at `:351`. Note
`AutonomyConfig::default()` is Supervised, so test 1 needs `full_config()`.

For the policy, do **not** try to reuse `default_policy()` — that is a private
helper inside `src/security/policy.rs`'s own test module (`:1093`) and is not
reachable from `src/approval/mod.rs`. Build one directly instead;
`SecurityPolicy` is public via `src/security/mod.rs:47`:

```rust
let policy = Arc::new(SecurityPolicy {
    autonomy: AutonomyLevel::Full,
    ..SecurityPolicy::default()
});
```

**Keep the `Arc` and pass a clone**, or the test cannot fail:

```rust
let mgr = ApprovalManager::from_config(&full_config()).with_policy(Arc::clone(&policy));
// ... later, to flip it live:
policy.set_autonomy(AutonomyLevel::Supervised);
```

Writing `.with_policy(Arc::new(policy))` moves the only handle in and leaves
the test with nothing to call `set_autonomy` on — it would compile and pass
vacuously.

`needs_approval("shell")` is the right probe. Note `supervised_config()`
(`src/approval/mod.rs:344`) *does* put `shell` in `always_ask` — that does not
affect these tests, because the `Full` and `ReadOnly` short-circuits at
`:88-95` run **before** the `always_ask` check, which is exactly the behaviour
under test.

**Verification**: `cargo test --lib` → all pass, including the 3 new tests.

**Mutation check (required before you call this done)**: temporarily make
`effective_autonomy()` ignore `self.policy` and always return
`self.autonomy_level`. Confirm tests 1 and 2 fail. Restore. If they still
pass, the tests are not covering the fix — STOP and report.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --locked --all-targets -- -D clippy::correctness` exits 0
- [ ] `cargo test --lib` exits 0
- [ ] `needs_approval` no longer reads the cached field directly. Check with a
      range-scoped grep rather than eyeballing: find the line range of
      `needs_approval` (`awk '/pub fn needs_approval/,/^    }/' src/approval/mod.rs`)
      and confirm `self.autonomy_level` appears **zero** times inside it, while
      appearing exactly once in the new `effective_autonomy` fallback
- [ ] `grep -c '\.with_policy(' src/channels/mod.rs` returns `1` (before: `0`).
      Do **not** grep for bare `with_policy`: that already returns `1` today
      because it is a substring of `from_config_with_policy_dir` at
      `src/channels/mod.rs:3243`, and would return `2` after a correct change.
- [ ] The three new tests exist and pass; the mutation check was performed and
      tests 1 and 2 failed under mutation
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The excerpts in "Current state" do not match the live code (drift). Line
  numbers drifting by a line or two while the quoted text matches is **not** a
  STOP — only a content mismatch is.
- The local `security` binding at `src/channels/mod.rs:3243` is absent, or
  there is more than one candidate policy in that function. Getting the
  *wrong* policy here would be silently useless — report rather than guessing.
- Adding the field to `ApprovalManager` breaks a caller you cannot fix inside
  the in-scope files.
- Any existing approval test fails. Report which; do not weaken it.
- A verification command fails twice after a reasonable fix attempt.

## Maintenance notes

- `auto_approve` and `always_ask` remain snapshots on this path. If a future
  change makes those hot-reloadable, the same `policy` handle can serve them —
  do not add a second mechanism.
- Reviewers should confirm the policy passed to `with_policy` is the **same
  `Arc`** stored as `ctx.security`, not a fresh policy built from the same
  config. A separate instance compiles and reads plausibly while restoring the
  bug, because `set_autonomy` would then write to a different allocation.
- The gateway and console-chat paths intentionally do not use `with_policy` —
  they build a fresh manager per turn from live config, so attaching a handle
  would be redundant. Leaving them alone is correct, not an oversight.
