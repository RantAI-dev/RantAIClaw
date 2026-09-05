# Plan 192: Give cron agent runs a stable memory scope so a scheduled job can't recall another conversation's rows

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/agent/loop_.rs src/agent/mod.rs src/cron/scheduler.rs src/tools/memory_recall.rs`
> If any of those changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

Cron-triggered agent runs go through `crate::agent::run` (the CLI/loop path).
That path builds its tool registry with `ConversationScope::default()` (= `None`)
and builds the injected memory context with `None` too. With a `None` scope, the
`memory_recall` tool does a **bare global recall across ALL conversations**
(`src/tools/memory_recall.rs:82` → `self.memory.recall(query, limit, None)`).

Every other surface is scoped: channels pass `Some(conversation_scope)`
(`src/channels/dispatch.rs:365,401-406`), the interactive `Agent` passes its
`conversation_id` (`src/agent/agent.rs:998`). A scheduled job that announces its
output into chat A therefore runs **unscoped** — so a `memory_recall` inside it
can return rows saved under chat B, another sender, or a TUI session, and the
model can quote that text into chat A's announced output. `memory_recall` is
auto-approved by default, so nothing gates this.

The injected `[Memory context]` block is already partially protected —
`should_skip` drops `MemoryCategory::Conversation` rows before injection
(`src/memory/context.rs:81`) — so the concentrated exposure is the explicit
`memory_recall` tool, which returns rows of every category.

After this plan, cron runs get a **stable scope identity** (`cron:<job_id>`).
`recall_layered` then returns that job's own rows plus the *shared/global* tier,
and **excludes other conversations' scoped rows** — the same isolation every
other surface already has. Global/shared facts still surface; cross-conversation
rows do not.

## Current state

- `src/agent/loop_.rs`
  - `pub async fn run(config, message, provider_override, model_override, temperature, peripheral_overrides) -> Result<String>` starts at **line 2013**. This is the function cron calls.
  - Registry build inside `run` (lines 2057–2071); the scope argument is line
    **2062**: `crate::tools::memory_recall::ConversationScope::default()`
    (= a handle holding `None`).
  - Two `build_context` calls inside `run`:
    - line **2306–2307** (single-message path, `if let Some(msg) = message` at
      line 2280) — **this is the path cron hits** (cron always passes `Some`).
    - line **2457–2458** (interactive REPL path, when `message` is `None`).
  - `build_context` definition (lines 240–256) hardcodes `None`:
    ```rust
    async fn build_context(mem: &dyn Memory, user_msg: &str, min_relevance_score: f64) -> String {
        memory::build_memory_context(
            mem,
            user_msg,
            min_relevance_score,
            None,                                  // <-- scope hardcoded to None
            memory::MemoryContextLimits::default(),
        )
        .await
        .block
    }
    ```
  - A `#[cfg(test)] mod tests` block exists at line **2730**; it already imports
    `SqliteMemory` (line 2821) and has `build_context_*` tests
    (e.g. `build_context_ignores_autosave_entries_but_keeps_curated_facts`, line 4063, calling `build_context` at 4092).

- `src/agent/mod.rs:19` — `pub use loop_::{process_message, run};` (this is how
  `crate::agent::run` resolves).

- `src/tools/memory_recall.rs`
  - `pub type ConversationScope = std::sync::Arc<std::sync::Mutex<Option<String>>>;` (line 17).
  - `execute` (lines 73–83): `None` scope ⇒ `self.memory.recall(query, limit, None)` (global); `Some(cid)` ⇒ `recall_layered(.., Some(cid))`.
  - It is **already correct when a scope is set** — proven by existing tests
    `a_set_conversation_scopes_the_tools_read` (line 289) and
    `an_unset_scope_reads_globally_as_before` (line 310), which use a
    `RecallScopeProbe` (line 234) that records the `session_id` of each `recall`
    call. The bug is purely that `agent::run` never *sets* the scope.

- `src/memory/mod.rs` — `recall_layered` (lines 467–475): with `Some(cid)` it
  returns the conversation's rows first, then backfills from **shared memory
  only**, excluding other conversations' scoped rows; with `None` it degrades to
  a plain global recall.

- `src/memory/context.rs` — `build_memory_context` (line 134) routes through
  `recall_layered(memory, user_message, recall_limit, conversation_id)` (line 144).

- `src/cron/scheduler.rs` — the cron call into the agent (lines 230–248):
  ```rust
  let name = job.name.clone().unwrap_or_else(|| "cron-job".to_string());
  let prompt = job.prompt.clone().unwrap_or_default();
  let prefixed_prompt = format!("[cron:{} {name}] {prompt}", job.id);
  let model_override = job.model.clone();

  let run_result = match job.session_target {
      SessionTarget::Main | SessionTarget::Isolated => {
          Box::pin(crate::agent::run(
              config.clone(),
              Some(prefixed_prompt),
              None,
              model_override,
              config.default_temperature,
              vec![],
          ))
          .await
      }
      // ...
  };
  ```

Convention: `ConversationScope` is `Arc<Mutex<Option<String>>>`; seed it with
`std::sync::Arc::new(std::sync::Mutex::new(conversation_id.clone()))`.

## Commands you will need

| Purpose   | Command                                                                 | Expected on success        |
|-----------|-------------------------------------------------------------------------|----------------------------|
| Format    | `cargo fmt --all -- --check`                                            | exit 0, no diff            |
| Lint      | `cargo clippy --all-targets -- -D warnings`                             | exit 0, no warnings        |
| New test  | `cargo test --lib build_context_scoped_forwards_conversation_id`        | 1 test passes              |
| Cron      | `cargo test --lib cron`                                                 | compiles whole lib; passes |
| Memory    | `cargo test --lib memory`                                               | all pass (no regressions)  |

Do **not** run a bare `cargo test` (workspace test is disk-heavy on this box).

## Scope

**In scope** (the only files you should modify):

- `src/agent/loop_.rs` — add `build_context_scoped`; rename `run` to
  `run_with_scope` (add param) + add a thin `run` wrapper; seed the scope and
  thread `conversation_id` to the two `build_context` calls; add one test.
- `src/agent/mod.rs` — re-export `run_with_scope`.
- `src/cron/scheduler.rs` — call `run_with_scope` with `Some(format!("cron:{}", job.id))`.

**Out of scope** (do NOT touch):

- The channel first-turn memory injection decision — channels inject memory only
  on a conversation's first turn (`src/channels/dispatch.rs:400`, `had_prior_history`
  gate). This is a **deliberate tradeoff**; do NOT change it.
- `memory_recall.rs` execute logic — already correct once a scope is set.
- `recall_layered` / `build_memory_context` internals.
- The 3 non-cron callers of `run` (`src/daemon/mod.rs:323`, `src/main.rs:1846`,
  `src/main.rs:2203`) — the thin `run` wrapper keeps their signature identical,
  so they must remain unchanged.

## Git workflow

- Branch: `advisor/192-cron-run-memory-scope`
- Conventional commits, e.g.
  `fix(cron): scope agent runs to cron:<job_id> so memory_recall can't cross conversations`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a scope-aware `build_context_scoped`, keep `build_context` as a `None` wrapper

In `src/agent/loop_.rs`, replace the current `build_context` (lines 240–256)
with a scope-aware function plus a thin back-compat wrapper:

```rust
/// Build context preamble by searching memory for relevant entries, scoped to
/// `conversation_id` when the caller has one. Entries below `min_relevance_score`
/// are dropped. With `None` this is a plain global recall (the prior behaviour).
async fn build_context_scoped(
    mem: &dyn Memory,
    user_msg: &str,
    min_relevance_score: f64,
    conversation_id: Option<&str>,
) -> String {
    memory::build_memory_context(
        mem,
        user_msg,
        min_relevance_score,
        conversation_id,
        memory::MemoryContextLimits::default(),
    )
    .await
    .block
}

/// Global-scope convenience for callers with no conversation identity.
async fn build_context(mem: &dyn Memory, user_msg: &str, min_relevance_score: f64) -> String {
    build_context_scoped(mem, user_msg, min_relevance_score, None).await
}
```

This leaves the existing `build_context` callers (line 2696 in `process_message`,
the test at line 4092) untouched.

**Verify**: `cargo fmt --all -- --check` → exit 0.

### Step 2: Rename `run` → `run_with_scope` (add a `conversation_id` param), add a thin `run` wrapper

In `src/agent/loop_.rs`:

1. Change the signature at line 2013 from
   `pub async fn run(` to
   `pub async fn run_with_scope(` and add a final parameter
   `conversation_id: Option<String>,` (after `peripheral_overrides: Vec<String>,`).
   Leave the entire body in place.

2. Seed the scope: change line 2062 from
   `crate::tools::memory_recall::ConversationScope::default()`
   to
   `std::sync::Arc::new(std::sync::Mutex::new(conversation_id.clone()))`.
   (This is the same `Arc<Mutex<Option<String>>>` type as `ConversationScope`.)

3. Thread the scope into the two `build_context` calls inside this function:
   - line 2307: `build_context(mem.as_ref(), &msg, config.memory.min_relevance_score).await`
     → `build_context_scoped(mem.as_ref(), &msg, config.memory.min_relevance_score, conversation_id.as_deref()).await`
   - line 2458: `build_context(mem.as_ref(), &user_input, config.memory.min_relevance_score).await`
     → `build_context_scoped(mem.as_ref(), &user_input, config.memory.min_relevance_score, conversation_id.as_deref()).await`

4. Immediately after `run_with_scope`, add a thin wrapper that preserves the old
   `run` signature so no other caller changes:

```rust
/// Back-compat entry point: run with no conversation scope (global memory),
/// exactly as before scoping was threaded through.
pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    peripheral_overrides: Vec<String>,
) -> Result<String> {
    run_with_scope(
        config,
        message,
        provider_override,
        model_override,
        temperature,
        peripheral_overrides,
        None,
    )
    .await
}
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0 (in particular,
`daemon/mod.rs` and `main.rs` still compile against the unchanged `run`).

### Step 3: Re-export `run_with_scope`

In `src/agent/mod.rs`, change line 19 from
`pub use loop_::{process_message, run};` to
`pub use loop_::{process_message, run, run_with_scope};`.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Point the cron scheduler at `run_with_scope` with a stable scope id

In `src/cron/scheduler.rs`, change the `crate::agent::run(...)` call (lines
240–247) to `crate::agent::run_with_scope(...)` and add the scope id as the
final argument:

```rust
Box::pin(crate::agent::run_with_scope(
    config.clone(),
    Some(prefixed_prompt),
    None,
    model_override,
    config.default_temperature,
    vec![],
    Some(format!("cron:{}", job.id)),
))
.await
```

**Verify**: `cargo test --lib cron` → compiles the whole lib and cron tests pass.

### Step 5: Add a regression test for the scope-forwarding seam

In `src/agent/loop_.rs`'s `#[cfg(test)] mod tests` block (line 2730), add a
probe Memory that records the `session_id` of each `recall` (mirror
`RecallScopeProbe` in `src/tools/memory_recall.rs:234`) and a test:

```rust
#[tokio::test]
async fn build_context_scoped_forwards_conversation_id() {
    // Probe records every session_id `recall` is called with.
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
    // (define a small `impl Memory` that pushes `session_id` into `calls` and
    //  returns Ok(vec![]) — copy the shape of memory_recall.rs's RecallScopeProbe)
    let mem: std::sync::Arc<dyn Memory> = /* Arc::new(probe) */;

    // Scoped: recall_layered(Some(cid)) reads the conversation first, then the
    // shared backfill — proving the id reached build_memory_context.
    let _ = build_context_scoped(mem.as_ref(), "q", 0.0, Some("cron:job1")).await;
    assert_eq!(
        calls.lock().unwrap().clone(),
        vec![Some("cron:job1".to_string()), None],
    );

    calls.lock().unwrap().clear();

    // Unscoped: exactly one global read, as before.
    let _ = build_context(mem.as_ref(), "q", 0.0).await;
    assert_eq!(calls.lock().unwrap().clone(), vec![None]);
}
```

If it is simpler, you may add the probe `impl Memory` at module scope inside the
test `mod tests` block. It must implement every `Memory` trait method; copy the
trivial bodies from `RecallScopeProbe` (`store`/`get`/`list`/`forget`/`count`/
`health_check` all return trivial values; only `recall` records and returns
`Ok(vec![])`).

**Verify**: `cargo test --lib build_context_scoped_forwards_conversation_id` →
1 test passes.

## Test plan

- New test `build_context_scoped_forwards_conversation_id` in
  `src/agent/loop_.rs` `mod tests` — proves the injected-context path now
  forwards the conversation id to `recall_layered` (scoped read + shared
  backfill) instead of a bare global read.
- The `memory_recall` tool's scope behaviour is **already** covered by
  `a_set_conversation_scopes_the_tools_read` and
  `an_unset_scope_reads_globally_as_before` (`src/tools/memory_recall.rs`), which
  exercise exactly the read that a seeded cron scope produces — so the tool-side
  isolation needs no new test; this plan only ensures the scope is *set*.
- Verification: `cargo test --lib build_context_scoped_forwards_conversation_id`
  (new test), plus `cargo test --lib cron` and `cargo test --lib memory` for no
  regressions.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib build_context_scoped_forwards_conversation_id` passes
- [ ] `cargo test --lib cron` exits 0 (whole-lib build green; scheduler compiles)
- [ ] `cargo test --lib memory` exits 0 (no regressions)
- [ ] `grep -n "ConversationScope::default()" src/agent/loop_.rs` no longer
      matches at the registry-build site inside `run_with_scope`
- [ ] `grep -n "run_with_scope" src/cron/scheduler.rs` matches the cron call
- [ ] The 3 non-cron `run` callers are unchanged
      (`git diff --stat 2aefb9f..HEAD -- src/daemon/mod.rs src/main.rs` shows no
      changes to those files)
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `run`, the registry-build site, or the `build_context` calls don't match the
  "Current state" excerpts (drift since this plan was written).
- Renaming `run` breaks a caller you were told is out of scope — that means the
  thin wrapper's signature doesn't match; fix the wrapper rather than the caller.
- A verification fails twice after a reasonable fix attempt.
- You find that scoping cron reads to `cron:<job_id>` makes an existing memory
  test fail in a way that suggests cron runs are *expected* to read globally —
  report it rather than weakening the scope.

## Maintenance notes

- **Scope identity choice**: `cron:<job_id>` gives each job a private, stable
  scope — it recalls its own rows plus shared/global memory, never another
  conversation's. The alternative (scoping to the *delivery target*, e.g. the
  telegram chat) would let a job share that chat's conversation memory; that is a
  usability call, not a safety one, and can be revisited. If you change it,
  update this note and the test.
- **Write side unchanged**: cron auto-saves still store under `None` (shared
  tier), so a job's own turns remain reachable via `recall_layered`'s shared
  backfill. Scoping writes too is a possible follow-up but is deliberately out of
  scope here to keep the change small and reversible.
- Reviewer should scrutinize: that the thin `run` wrapper's signature exactly
  matches the pre-change `run` (so daemon/main are untouched), and that the
  scope `Arc<Mutex<Option<String>>>` type still matches `ConversationScope`.
