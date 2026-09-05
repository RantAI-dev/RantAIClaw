# Plan 105: Ambient KB context must follow the operator's intent, not a file on disk

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/axi/ambient.rs src/kb/axi/api.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: 102
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

The agent is told the Knowledge Base exists based on whether a file is present,
not on whether the operator wants it used.

`src/kb/axi/ambient.rs:26-35`:

```rust
pub fn kb_ambient_context() -> Option<String> {
    let path = resolve_kb_db_path();
    if !Path::new(&path).exists() {
        return None;
    }
    Some("Knowledge base available. To search documents, run: ...")
}
```

Once a single document has ever been ingested, `kb.db` exists forever. So the
hint is injected into every system prompt even after the operator clears the
credentials — the agent then shells out to `kb search` and gets an error, every
turn, with no way to learn it should stop.

Plan 102 gives this a real signal. The check should be "the operator turned it
on", not "a file is on disk".

There is a second, quieter bug in the same area. `ensure_kb_ctx` caches the
built context keyed **only** by the database path (`api.rs:170-181`):

```rust
    let mut guard = KB_CTX.lock().await;
    if let Some(cached) = guard.as_ref() {
        if cached.path == path {
            return match &cached.ctx { ... };
        }
    }
```

The gateway's `PUT /config/knowledge` calls `clear_kb_ctx()` explicitly, so that
path is fine. But a key changed any other way — `/setup knowledge` in the TUI, a
hand edit of `config.toml`, a profile switch — leaves the stale context in place
until the process restarts, including a cached `Err`.

## Current state (verified at 2ca7e59)

- `kb_ambient_context` — `ambient.rs:26`, 38 lines total
- Its only caller is the system-prompt assembly; grep before editing
- `tests/kb/agent_integration_test.rs:26,55` pins the two existing cases:
  db exists → `Some`, db missing → `None`
- `KB_CTX` is `Mutex<Option<CachedCtx>>` — `api.rs:157`. The module doc at
  `api.rs:12` still calls it a `OnceCell`; that is stale.

## Scope

**In scope**: gate the ambient hint on `enabled`; key the context cache on the
credentials too.

**Out of scope**: the shape of the hint text — plan 088 owns that.

## Git workflow

```bash
git switch -c fix/ambient-follows-enabled
```

## Steps

### Step 1: Take config, not just a path

`kb_ambient_context()` takes no arguments today. Give it the knowledge config
so it can see intent:

```rust
/// Returns `Some(text)` when the operator has activated the Knowledge Base AND
/// a database is reachable. Deliberately gated on intent rather than on the
/// file alone: `kb.db` survives a credential clear, and a hint the agent cannot
/// act on produces a failing shell call every turn.
pub fn kb_ambient_context(knowledge: &crate::config::KnowledgeConfig) -> Option<String> {
    if !knowledge.enabled {
        return None;
    }
    let path = resolve_kb_db_path();
    if !Path::new(&path).exists() {
        return None;
    }
    Some(...)
}
```

**There are two call sites, not one** — both in `src/agent/loop_.rs`, at
`:2252` and `:2675`, each inside a `#[cfg(feature = "kb")]` block, and both
re-exported through `src/kb/axi/mod.rs:23`. Verified: `config` is already in
scope at both (`loop_.rs:2259` reads `config.autonomy`, `:2680` reads
`config.memory`), so passing `&config.knowledge` costs nothing.

Keep the file check — an enabled KB with no database still has nothing to
search.

**Verify**: `cargo build --features kb`; `grep -rn 'kb_ambient_context' src/`
shows exactly the definition, the re-export, and the two call sites.

### Step 2: Update the agent-integration tests

`agent_integration_test.rs` has two tests. Extend to three:

1. enabled + db exists → `Some`
2. enabled + db missing → `None`
3. **disabled + db exists → `None`** ← the regression this plan fixes

Case 3 must be red before Step 1.

### Step 3: Key the context cache on the credentials

> **Ordering note**: plan 104 also edits `ensure_kb_ctx`, adding the `enabled`
> gate at the top of the function. Land 104 first and rebase this step onto it;
> the two edits are in the same function and will conflict otherwise.

In `CachedCtx`, store a cheap fingerprint of the resolved keys alongside the
path, and treat a change as a cache miss:

```rust
struct CachedCtx {
    path: PathBuf,
    /// Hash of the resolved credentials. The gateway calls `clear_kb_ctx` on
    /// its own writes, but a key changed via the TUI wizard, a hand-edited
    /// config, or a profile switch would otherwise keep serving a context
    /// built with the old credential — including a cached `Err`.
    cred_fingerprint: u64,
    ctx: Result<Arc<KbContext>, String>,
}
```

Use `DefaultHasher` over the two key strings. **Never store or log the keys
themselves** — the fingerprint is a comparison token only.

**Verify**: `clear_kb_ctx_resets_cache` (`api.rs:1531`) still passes; add a test
that changing the key in `state.config` rebuilds the context without an explicit
`clear_kb_ctx` call.

### Step 4: Fix the stale module doc

`api.rs:12-17` describes `KB_CTX` as a `OnceCell` whose failures persist "until
the process restarts". Both halves are now wrong. Rewrite to match the mutex +
fingerprint behaviour.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb agent_integration_test
cargo test --features kb --test kb api_test
```

Manual — the loop that misbehaves today:

```bash
# with the KB enabled and a document ingested, confirm the agent is told about it
cargo run --features kb -- --debug 2>&1 | grep -i 'knowledge base available'
# deactivate, restart, confirm the hint is gone
```

## Done criteria

- A deactivated KB produces no ambient hint even with `kb.db` present.
- A key change from any surface invalidates the cached context.
- The module doc matches the code.

## STOP conditions

- `kb_ambient_context` has more than one caller and they cannot all supply the
  config — report before threading it further.
