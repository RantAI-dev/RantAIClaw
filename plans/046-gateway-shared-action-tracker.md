# Plan 046: Restore hourly action-rate enforcement on the gateway webhook path

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 3edb236..HEAD -- src/security/policy.rs src/gateway/mod.rs`
> If either file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `3edb236`, 2026-07-27

## Why this matters

`max_actions_per_hour` is documented in `src/config/schema.rs` as "the primary
runaway guard actually enforced in the agent loop". It is currently
unenforceable on `POST /webhook` and the other gateway-relayed surfaces,
because each turn builds a brand-new `SecurityPolicy` and therefore a
brand-new, empty `ActionTracker`. The sliding-window counter restarts from
zero on every turn, and since one turn is capped at 15 tool iterations while
the default budget is 200, the limit can never be reached.

This is a regression introduced by commit `767c3ea` and shipped in the
`v0.13.0-alpha` release (`git tag --contains 767c3ea` → `v0.13.0-alpha`).
Before that commit the gateway built one `Arc<SecurityPolicy>` at startup and
every tool shared its tracker for the process lifetime, so the budget
accumulated across webhook turns as intended.

The per-turn rebuild itself is deliberate and must stay — it is what keeps
autonomy level, allowlist, and the Strict preset's tool filtering fresh. The
bug is that the rebuild also discards state that is *supposed* to accumulate.

## Current state

Files involved:

- `src/security/policy.rs` — `ActionTracker` and `SecurityPolicy`; the
  constructor that creates a fresh tracker every time.
- `src/gateway/mod.rs` — `build_tools_factory`, the per-turn closure that
  constructs a policy for each webhook turn.

`ActionTracker` is a plain (non-shared) field with a **deep-copying** manual
`Clone`, so handing a clone to a new policy does *not* share counting —
`src/security/policy.rs:40-45` and `:76-83`:

```rust
/// Sliding-window action tracker for rate limiting.
#[derive(Debug)]
pub struct ActionTracker {
    /// Timestamps of recent actions (kept within the last hour).
    actions: Mutex<Vec<Instant>>,
}
```

```rust
impl Clone for ActionTracker {
    fn clone(&self) -> Self {
        let actions = self.actions.lock();
        Self {
            actions: Mutex::new(actions.clone()),
        }
    }
}
```

The field on the policy — `src/security/policy.rs:98`:

```rust
    pub tracker: ActionTracker,
```

Every constructor makes a fresh one. `src/security/policy.rs:982` (inside
`from_config_with_policy_dir`) and `src/security/policy.rs:181` (inside
`impl Default`):

```rust
            tracker: ActionTracker::new(),
```

The gateway builds a whole new policy per turn — `src/gateway/mod.rs:486-494`:

```rust
fn build_tools_factory(
    runtime: Arc<dyn runtime::RuntimeAdapter>,
    mem: Arc<dyn Memory>,
) -> ToolsFactory {
    Arc::new(move |config: &Config| {
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
```

The two readers that make this load-bearing — `src/security/policy.rs:929-937`:

```rust
    pub fn record_action(&self) -> bool {
        let count = self.tracker.record();
        count <= self.max_actions_per_hour as usize
    }

    /// Check if the rate limit would be exceeded without recording.
    pub fn is_rate_limited(&self) -> bool {
        self.tracker.count() >= self.max_actions_per_hour as usize
    }
```

Called from `src/tools/shell.rs:292` (`is_rate_limited`) and
`src/tools/shell.rs:379` (`record_action`), and from
`src/security/policy.rs:918` inside `enforce_tool_operation`.

**Safety note established during planning**: no *production* site clones a
`SecurityPolicy` by value — all 71 tool fields hold `Arc<SecurityPolicy>`
(`grep -rn "security: Arc<SecurityPolicy>" src/tools/ | wc -l` → 71).

Three **test** sites clone by value: `src/security/policy.rs:1145` and `:1168`
(both `let tool = daemon.clone();`), plus `:2357`
(`let policy_b = policy_a.clone();` in
`runtime_allowlist_shared_across_clones`, which asserts only on
`runtime_allowlist` — already `Arc`-shared, so it stays green). None asserts
anything about the action counter — they exercise the autonomy and allowlist
overrides — so making the tracker shared does not change what they test.
**This is expected; finding these three is not a reason to stop.**

There is one existing test that *does* depend on the deep copy, and Step 1
deals with it explicitly — see that step.

Repo conventions to match:

- Shared-mutable state on `SecurityPolicy` already uses `Arc<...>` fields —
  see `runtime_allowlist: Arc<RwLock<HashSet<String>>>` at
  `src/security/policy.rs:103` and `autonomy_runtime` at `:120`. Follow that
  shape.
- Locking uses `parking_lot` (`use parking_lot::{Mutex, RwLock};`,
  `src/security/policy.rs:1`) — its guards do not return `Result`, so no
  `.unwrap()` on lock acquisition.
- Tests live in the same file under `#[cfg(test)] mod tests`. Model new
  policy tests on `set_allowed_commands_narrows_across_clones` in
  `src/security/policy.rs`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no output |
| Lint (same as CI) | `cargo clippy --locked --all-targets -- -D clippy::correctness` | exit 0 |
| Unit tests | `cargo test --lib` | exit 0, all pass |
| Focused tests | `cargo test --lib security::policy` | all pass |

Note: CI also runs a **strict-delta** clippy gate
(`scripts/ci/rust_strict_delta_gate.sh`) at `-D warnings` — restricted to the
lines your diff touches, with pedantic lints on. The table's
`-D clippy::correctness` will not catch those. Before pushing, re-run clippy
at `-D warnings` and check that no warning points at a line you added.

Note on `cargo test --lib`: a handful of `skills::tests::toml_*` tests are
non-hermetic against `$HOME` on some machines. If they fail, confirm they also
fail on an unmodified checkout before treating it as your regression.

## Scope

**In scope** (the only files you should modify):

- `src/security/policy.rs`
- `src/gateway/mod.rs`
- `plans/README.md` — append the status row for this plan (the table currently
  ends at row `045`; there is no row for `046` yet). Append exactly:

  ```
  | 046 | Restore hourly action-rate enforcement on the gateway webhook path | P1 | S | LOW | — | security | TODO |
  ```

**Out of scope** (do NOT touch, even though they look related):

- `src/channels/mod.rs` — the channels runtime has its own separate staleness
  problems, planned elsewhere. Do not "fix" them here.
- `src/agent/agent.rs`, `src/agent/loop_.rs`, `src/cron/**` — other surfaces
  construct their own policies. Leaving each with its own tracker is correct
  for this plan; unifying them is a later, larger change.
- The per-turn rebuild in `build_tools_factory` — keep it. It is deliberate.
  You are changing only what the rebuilt policy does with the tracker.
- `policy_dir` and `pending` being `None` on the gateway path. Related, real,
  and explicitly deferred — do not address them here.

## Git workflow

- Branch: `fix/gateway-shared-action-tracker`
- Conventional commit titles. Example from this repo's history:
  `fix(gateway): build the webhook tool registry per turn, not at boot`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Make the tracker a shared handle

In `src/security/policy.rs`, change the field at `:98` from
`pub tracker: ActionTracker` to `pub tracker: Arc<ActionTracker>`.

Because `Arc<T>` is `Clone` for any `T`, the manual
`impl Clone for ActionTracker` (`:76-83`) is no longer required to satisfy
`#[derive(Debug, Clone)]` on `SecurityPolicy` (`:86`). Delete that impl — a
deep-copying clone is now actively misleading, since the whole point is that
handles share.

**Deleting it breaks exactly one existing test, and that is expected.**
`action_tracker_clone_is_independent` at `src/security/policy.rs:1645` calls
`tracker.clone()` on a bare `ActionTracker` and asserts the copy counts
independently:

```rust
    #[test]
    fn action_tracker_clone_is_independent() {
        let tracker = ActionTracker::new();
```

That property is exactly what this plan removes, so the test is obsolete.
**Delete the whole test function.** Do not try to preserve it by keeping the
impl, and do not treat its breakage as a STOP condition.

Update both construction sites to wrap: `:181` and `:982` become
`tracker: Arc::new(ActionTracker::new()),`.

Add a constructor that accepts an existing tracker, next to
`from_config_with_policy_dir`. Keep the existing constructors working by
delegating. Suggested shape:

```rust
    /// Build from config while REUSING an existing action tracker.
    ///
    /// The rate-limit window is process state, not config state: a caller that
    /// rebuilds its policy per turn (the gateway tool factory) must carry the
    /// same tracker forward or the hourly budget silently restarts every turn.
    pub fn from_config_with_shared_tracker(
        autonomy_config: &crate::config::AutonomyConfig,
        workspace_dir: &Path,
        policy_dir: Option<PathBuf>,
        tracker: Arc<ActionTracker>,
    ) -> Self {
        Self {
            tracker,
            ..Self::from_config_with_policy_dir(autonomy_config, workspace_dir, policy_dir)
        }
    }
```

**Verify**: `cargo check --all-targets` → exit 0.

Use `--all-targets`, not `cargo build --lib`: `build --lib` does not compile
`#[cfg(test)]` code, so it would report success while the test module is
broken and you would only discover it two steps later.

### Step 2: Hoist one tracker into the gateway factory

In `src/gateway/mod.rs`, inside `build_tools_factory` (`:486`), create a
single `Arc<ActionTracker>` **outside** the returned closure and move it in,
so every turn's policy shares it. Then construct the per-turn policy with
`from_config_with_shared_tracker`, passing `None` for `policy_dir` to preserve
today's behaviour exactly.

Target shape:

```rust
fn build_tools_factory(
    runtime: Arc<dyn runtime::RuntimeAdapter>,
    mem: Arc<dyn Memory>,
) -> ToolsFactory {
    // One tracker for the process. The registry is rebuilt per turn so policy
    // stays fresh, but the rate-limit window must NOT restart with it — that
    // is what made `max_actions_per_hour` unenforceable on this path.
    let tracker = Arc::new(crate::security::policy::ActionTracker::new());
    Arc::new(move |config: &Config| {
        let security = Arc::new(SecurityPolicy::from_config_with_shared_tracker(
            &config.autonomy,
            &config.workspace_dir,
            None,
            Arc::clone(&tracker),
        ));
```

Use the full path `crate::security::policy::ActionTracker` exactly as written.
`ActionTracker` is **not** re-exported from `crate::security` —
`src/security/mod.rs:47` is `pub use policy::{AutonomyLevel, SecurityPolicy};`
only. The module itself is public (`pub mod policy;`), so the full path works.
Do **not** add `ActionTracker` to that `pub use` list: `src/security/mod.rs` is
not in this plan's scope.

**Verify**: `cargo check --all-targets` → exit 0.

### Step 3: Add the regression tests

See "Test plan" below for exactly what to write. Write them before running the
full suite.

**Verify**: `cargo test --lib security::policy` → all pass, including the new
tests.

### Step 4: Full verification

**Verify**, all three:

- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --locked --all-targets -- -D clippy::correctness` → exit 0
- `cargo test --lib` → exit 0

## Test plan

Add to the `#[cfg(test)] mod tests` block in `src/security/policy.rs`, modelled
structurally on the existing `set_allowed_commands_narrows_across_clones`:

1. `shared_tracker_accumulates_across_rebuilt_policies` — build an
   `AutonomyConfig` with `max_actions_per_hour` set to a small number (e.g. 3).
   Create one `Arc<ActionTracker>`. Build **two** policies from it via
   `from_config_with_shared_tracker`. Call `record_action()` twice on the
   first and twice on the second; assert the fourth call returns `false`
   (budget exhausted). This fails on the pre-fix code because each policy
   would carry its own counter.

2. `independent_policies_do_not_share_a_tracker` — build two policies with the
   plain `from_config` constructor and assert that exhausting the budget on
   one leaves the other still allowing actions. This pins that the *default*
   behaviour is unchanged for surfaces that legitimately want their own
   window.

Add to `src/gateway/mod.rs`'s test module:

3. `tools_factory_shares_one_action_tracker_across_turns` — in
   `src/gateway/mod.rs`'s test module. **Model it on the existing
   `tools_factory_tracks_the_autonomy_level_of_the_config_it_is_given` at
   `src/gateway/mod.rs:3786`** — it already shows how to build a factory with
   `MockMemory` (`src/gateway/mod.rs:2943`) and a runtime, and how to find and
   execute a tool from the returned registry.

   Assert **behaviourally**, not structurally: set
   `config.autonomy.max_actions_per_hour` to a small number (e.g. 2), invoke
   the factory twice, and use `file_write` from each registry — it calls
   `record_action()` at `src/tools/file_write.rs:146`. Writes from the second
   registry must start refusing once the shared budget from the first is
   exhausted.

   **Do not** try to compare trackers with `Arc::ptr_eq`. The factory returns
   `Vec<Box<dyn Tool>>`, and `pub trait Tool` (`src/tools/traits.rs:22`)
   exposes only `name`, `description`, `parameters_schema`, `execute`, and
   `spec` — there is no downcast and no accessor to a tool's policy. The
   behavioural route is the only one available.

**Verification**: `cargo test --lib` → all pass, including the 3 new tests.

**Mutation check (required before you call this done)** — two separate
mutations, because the two tests exercise different seams:

- For **test 1**: make `from_config_with_shared_tracker` ignore its `tracker`
  argument and call `ActionTracker::new()` instead. Test 1 must fail. (Test 1
  never touches `build_tools_factory`, so mutating Step 2 cannot falsify it —
  do not expect it to.)
- For **test 3**: revert Step 2 to `SecurityPolicy::from_config(...)`. Test 3
  must fail.

Restore both afterwards. If either test still passes under its own mutation,
that test is not covering the fix — STOP and report.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --locked --all-targets -- -D clippy::correctness` exits 0
- [ ] `cargo test --lib` exits 0
- [ ] `grep -c "impl Clone for ActionTracker" src/security/policy.rs` returns `0`
- [ ] `grep -c "fn action_tracker_clone_is_independent" src/security/policy.rs` returns `0` (obsolete test deleted)
- [ ] `grep -c "tracker: ActionTracker::new()" src/security/policy.rs` returns `0` — both construction sites now read `tracker: Arc::new(ActionTracker::new()),`
- [ ] `grep -c "pub tracker: Arc<ActionTracker>" src/security/policy.rs` returns `1`
- [ ] The three new tests exist and pass; **both** mutation checks above were performed, and each named test failed under its own mutation
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The excerpts in "Current state" do not match the live code (drift). Line
  numbers drifting by a line or two while the quoted text matches is **not** a
  STOP — only a content mismatch is.
- You find a site that clones a `SecurityPolicy` **by value** and asserts on
  the action counter. The three known value-clone sites (`policy.rs:1145`,
  `:1168`, `:2357`) do not, and `action_tracker_clone_is_independent` is deleted by
  Step 1 — so any *other* such site is new information and needs a decision.
- Removing `impl Clone for ActionTracker` breaks compilation anywhere other
  than the two construction sites and the one test Step 1 tells you to delete.
- Making the gateway share a tracker causes an existing test to fail that is
  not one of yours. Report which; do not "fix" it by reverting the sharing.
- A verification command fails twice after a reasonable fix attempt.

## Maintenance notes

- The same class of bug exists wherever a policy is rebuilt frequently. If any
  other surface later adopts a per-turn factory, it must reuse the tracker the
  same way — the tracker, `runtime_allowlist`, and `pending` are all
  *process* state, not config state.
- Reviewers should check specifically that `build_tools_factory` creates the
  tracker **outside** the closure. Creating it inside compiles fine and looks
  correct at a glance while restoring the exact bug.
- Deliberately deferred out of this plan: the gateway path passes
  `policy_dir: None`, so `/allow <cmd> --persist` grants are never loaded
  there and the shell tool's cascading approval cannot run. Both are real and
  belong to the larger policy-propagation work, not here.
