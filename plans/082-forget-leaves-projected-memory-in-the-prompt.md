# 082 — Forgetting a core memory leaves it in the prompt-injected projection

Written against `7114f88`. Risk tier: **MEDIUM** (`src/tools/**`, `src/gateway/**`,
prompt content). Affects the **default** backend (`sqlite`) and `lucid`.

`MEMORY.md` is injected into the system prompt unconditionally
(`src/agent/prompt.rs:281-286`). On `sqlite`/`lucid` that file is a *projection*
of the `core` rows in `brain.db`, written by
`snapshot::project_core_memories` (`src/memory/snapshot.rs:120`).

Nothing re-projects after a delete except the CLI. `refresh_projection`
(`src/memory/cli.rs:187`) is called from exactly three places, all in `cli.rs`:
`handle_add` (`:222`), `handle_clear_key` (`:343`), `handle_clear` (`:383`).
A grep across `src/tools/`, `src/tui/`, `src/gateway/` for
`project_core_memories|refresh_projection` returns nothing.

So a core memory deleted through the agent's own tool, the TUI, or the HTTP API
is removed from the authoritative store and **stays in the file that reaches the
model**.

## Evidence

Probe against the real tool + real projection. All six probes behind plans
082-085 are in `plans/notes/082-085-memory-probes.patch` — apply it with
`git apply`, run `cargo test --lib -- memory:: memory_forget`, and every one
fails on its intended assertion with its controls passing. They are written to
become the regression tests, not to be thrown away.

```
---- tools::memory_forget::tests::probe_forget_leaves_the_projection_stale ----
the prompt-injected file still holds the forgotten entry:
<!-- rantaiclaw:memory:begin -->
<!-- Generated from core memory. Edits inside this block are overwritten; write prose outside it. -->
- rotation_note: staging credentials rotate weekly
<!-- rantaiclaw:memory:end -->
```

Three controls in the same probe passed: the projection did write the entry
(`project_core_memories` returned 1), the tool reported success, and
`mem.get("rotation_note")` returned `None`. Only the projection is stale.

`project_core_memories` also runs at backend construction
(`src/memory/mod.rs:352-359`), so the staleness window is "until this process
builds a memory backend again". For `rantaiclaw run` that is the next process.
For the **gateway and the TUI — both long-lived — it is the rest of the
process lifetime**, and a new session started inside that process reads the
stale file.

## Affected call sites

Delete paths (stale-present — the deleted fact keeps reaching the model):

- `src/tools/memory_forget.rs:95-111` — the agent's own tool
- `src/tui/commands/memory.rs:251-271` — `remove_memory` (`/memory remove`, `/memory forget`)
- `src/gateway/api_v1.rs:1699-1707` — `memory_delete` (`DELETE /api/v1/memory/{key}`)

Store paths (stale-absent — a `core` memory just written is not in the file the
prompt injects, contradicting the comment at `src/memory/mod.rs:349-351` which
claims "a memory stored mid-session lands in the file now"; that is only true on
the CLI path):

- `src/tools/memory_store.rs` — `MemoryStoreTool::execute`
- `src/tui/commands/memory.rs:214-249` — `add_memory`
- `src/gateway/api_v1.rs:1601` — `memory_create`

## Fix

`refresh_projection` is private to `cli.rs` and carries the backend gate that
makes it safe (skip anything that is not `Sqlite`/`Lucid`, because
`MarkdownMemory` owns `MEMORY.md` directly — projecting there would write it
twice). Promote it, do not duplicate it:

1. Move `refresh_projection` into `src/memory/snapshot.rs` (or `src/memory/mod.rs`)
   as a `pub fn`, keeping the `classify_memory_backend` gate and the
   `tracing::warn!`-on-error behaviour verbatim. Leave `cli.rs` calling the moved
   function so the CLI path is unchanged.
2. Call it after a successful mutation at each of the six sites above.

Constraint that shapes the signature: the callers do not all hold a `Config`.
`refresh_projection` currently needs one for `config.memory.backend` +
`config.storage.provider.config` + `config.workspace_dir`. The tools hold a
`SecurityPolicy` and an `Arc<dyn Memory>`; the gateway holds `AppState` (which
has the config behind a lock); the TUI holds `TuiContext`. Two workable shapes —
pick one and use it everywhere:

- Pass `&Config` (gateway/TUI have it; the tools need it threaded into
  `MemoryStoreTool::new`/`MemoryForgetTool::new`), or
- gate on `memory.name()` instead of the config (`"sqlite"` / `"lucid"`) and take
  only `workspace_dir`. Cheaper to wire, and the backend instance is the thing
  that actually decides whether the projection is owned elsewhere.

Prefer the second unless it loses the `storage.provider.config` case — check
`effective_memory_backend_name` before deciding, since `postgres` is selected
through `storage.provider.config` rather than `memory.backend`.

**Do not** make the projection a write-through side effect inside
`Memory::forget`/`Memory::store`. That would put filesystem work behind a trait
every mock in the test suite implements, and `MarkdownMemory` would recurse into
its own file. Keep it at the call sites.

## Non-goals

- Bidirectional sync between `MEMORY.md` and `brain.db`. Explicitly rejected in
  `snapshot.rs:113-118`; that reasoning still holds.
- Rebuilding the system prompt mid-session. The prompt is built once per session
  by design; this plan only makes the *file* correct so the next session is.

## Validation

- Unit (`src/tools/memory_forget.rs`): store a `core` memory, project, forget via
  the tool, assert `MEMORY.md` no longer contains the key. Assert the three
  controls too (projection wrote it, tool reported success, store is empty) so
  the test cannot pass vacuously.
- Unit (`src/tools/memory_store.rs`): store a `core` memory via the tool, assert
  it appears in `MEMORY.md` without an intervening backend construction.
- Unit: same pair for `memory_delete`/`memory_create` in `src/gateway/api_v1.rs`
  (both already have real-memory test helpers: `paired_state_with_real_memory`,
  `state_with_real_memory`).
- Regression guard: a `markdown`-backend test asserting the refresh is a **no-op**
  — `MEMORY.md` must not gain a `rantaiclaw:memory:begin` block, because that
  backend writes the file itself.
- `cargo test --lib -- memory:: memory_forget memory_store api_v1`

## Rollback

One commit per surface (tool / TUI / gateway) on top of the move commit. The move
commit is behaviour-preserving on its own and can stay if a call-site commit is
reverted.
