# 062 — `MEMORY.md` becomes the projection of core memories

- **Finding:** #19 (memory deepscan, wave 3)
- **Written against:** `0e458fb`
- **Risk tier:** medium (`src/memory/**`)
- **Effort:** M
- **Depends on:** 061 (one context builder — the recall tier this complements)
- **Blocks:** 063 (`replace` / consolidation needs a projected budget to act on)

## Problem

Two memory systems run in parallel and neither knows about the other.

**The file tier.** `MEMORY.md` is scaffolded by the onboarding wizard
(`onboard/wizard.rs`), injected into the system prompt with a
`BOOTSTRAP_MAX_CHARS` budget (`agent/prompt.rs`), and the wizard tells the agent in
so many words: *"When someone says remember this → update daily file or MEMORY.md"*.

**The database tier.** `brain.db` is written by `memory_store`, surfaced by scored
recall, and is what `memory_forget` and `memory recall` operate on.

On the default `sqlite` backend **no code path writes `MEMORY.md`** — only
`MarkdownMemory::store` does, and that backend is not the default. So the tier that is
guaranteed to reach the model contains scaffold prose and nothing else, forever, while
everything the agent actually learns lands somewhere the prompt never reads.

That is the root of finding #3's severity: because the guaranteed tier is empty, the
entire burden falls on scored recall — and the scoring was broken.

## Approach

DB stays authoritative. `MEMORY.md` becomes its rendered projection.

Not dual-write (no conflict rule) and not "read core from the DB instead" (that would
discard the human-readable file both OpenClaw and Hermes deliberately keep, and break
what the wizard promises). Projection has one direction and reuses machinery that
already exists in `snapshot.rs`.

### Generated block, hand-written surroundings

The wizard's prose and any operator notes must survive. So the projection owns a
delimited region and nothing else:

```
<hand-written guidance — untouched>

<!-- rantaiclaw:memory:begin -->
- user_lang: prefers Bahasa Indonesia
<!-- rantaiclaw:memory:end -->
```

Outside the markers: human-authored, injected as-is, never rewritten.
Inside: generated from `MemoryCategory::Core`, overwritten on every regeneration.

This deliberately makes the projection **one-way**. A bidirectional sync would need
conflict rules, an import path, and a way to tell an edit from a stale render — none of
which is worth it when "write facts through the agent, write prose by hand" is a clear
enough split. The markers say so in the file.

### Freshness

`Agent::turn_inner` builds the system prompt once, when history is empty
(`agent/agent.rs`) — the prompt is frozen for the session, which is what makes prefix
caching work with the `cache_control` the Anthropic provider already sends.

So the projection is regenerated when the backend is constructed, and a core memory
stored mid-session appears in the file immediately but in the *prompt* only next
session. That is exactly Hermes' tradeoff, and it is already this codebase's behaviour —
this plan does not change it. Within-session freshness stays with the recall tier, which
runs per turn. Two tiers, complementary: frozen core, live search.

## Change

### Files in scope

- `src/memory/snapshot.rs` — render the projection block; reuse the existing export
- `src/memory/mod.rs` — regenerate after the backend is built

### Files explicitly out of scope

- `agent/prompt.rs` — it already injects `MEMORY.md`; nothing to change
- `MEMORY_SNAPSHOT.md`, `export_snapshot`, `hydrate_from_snapshot` — disaster recovery,
  a separate concern, untouched
- `MarkdownMemory` — it owns `MEMORY.md` directly on its own backend; projecting there
  too would double-write. Projection applies to sqlite and lucid only
- `onboard/wizard.rs` — its scaffold survives by sitting outside the markers

### Steps

1. Add `project_core_memories(workspace_dir)` to `snapshot.rs`: read `category = 'core'`
   from `brain.db`, render `- key: content` lines, and splice them between the markers
   in `MEMORY.md`, creating the file and the block when absent.
2. Preserve everything outside the markers byte-for-byte. When the file exists without
   markers, append the block rather than replacing the file.
3. Call it from `create_memory` after the backend is constructed, for `Sqlite` and
   `Lucid` only. Best-effort: log and continue on failure, like hygiene and snapshot do.
4. Bound the block. `BOOTSTRAP_MAX_CHARS` is 20,000 and the whole file is injected every
   session; an unbounded projection is a silent per-session token cost. Cap it and say in
   the block when entries were omitted.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
```

Never a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

1. `projection_writes_core_memories_into_the_block`
2. `projection_preserves_content_outside_the_markers` — scaffold prose above and a note
   below must both survive verbatim. This is the one that matters: it is what makes the
   change safe to ship to existing workspaces.
3. `projection_replaces_a_previous_block_rather_than_appending` — regenerating twice must
   not stack two blocks.
4. `projection_appends_a_block_to_a_file_without_markers`
5. `projection_excludes_non_core_categories`
6. `projection_is_bounded` — many large core memories must not produce an unbounded block.
7. `projection_creates_the_file_when_absent`

Each checked against pre-change behaviour; before this plan `MEMORY.md` is never written
at all on sqlite, so any assertion about its content fails.

## Escape hatches

- If preserving surrounding content cannot be done reliably (nested markers, a partially
  written block from an interrupted run), STOP and report. Corrupting a file an operator
  hand-writes is worse than not projecting at all.
- If projecting on every backend construction shows up as a measurable startup cost on a
  large `brain.db`, STOP and report — moving it to the hygiene cadence is a different
  tradeoff and deserves its own decision.

## Maintenance note

`MEMORY.md` now has two owners with a clear border: the marker block belongs to the
runtime, everything else to whoever is holding the pen. Any future writer must respect
the markers. 063 builds on this — the consolidation wall needs a rendered budget to
measure against.

## Rollback

`git revert` stops the projection. The marker block stays in any `MEMORY.md` already
written; it is plain markdown and remains readable, just no longer updated. Nothing in
the database changes.
