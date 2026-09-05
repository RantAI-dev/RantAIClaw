# 063 — Correcting a fact needs its exact key; nothing prompts consolidation

- **Finding:** #22 (memory deepscan, wave 3)
- **Written against:** `3b3d9d4`
- **Risk tier:** **high** (`src/tools/**` — CLAUDE.md §5 sets the tier by path)
- **Effort:** M
- **Depends on:** 062 (the projection budget this measures against)
- **Blocks:** 064 (flush-before-compaction uses these actions)

## Problem

`memory_store(key, content)` upserts by key and `memory_forget(key)` deletes by key. Both
require the agent to already know the exact key.

To correct a fact it must first `memory_recall` to find the key, then store — two round
trips, and only if the recall surfaces the right entry. In practice the cheaper move is to
store the correction under a *new* key, so the stale fact and its correction sit side by
side and both remain eligible for recall.

Nothing anywhere signals that core memory has outgrown what reaches the model. Since 062
the projection block is bounded and says `… N more not shown` in the file — but the agent,
which is the thing that could actually consolidate, never sees that.

Hermes addresses both with `replace` and `remove` matched on substrings rather than keys,
plus a hard wall: a write that would exceed the budget is refused until the agent merges
or drops something.

## Change

### Files in scope

- `src/tools/memory_store.rs` — a `replaces` parameter and the capacity notice
- `src/tools/memory_forget.rs` — a `contains` parameter
- `src/memory/snapshot.rs` — make the projection budget readable

### Files explicitly out of scope

- A third tool. The repo's idiom is one tool per operation, and extending two existing
  tools covers both cases without growing the surface the model has to learn.
- `Memory` trait — substring matching is a tool-level convenience over `list`, not a new
  backend obligation. Adding it to the trait would oblige five backends to implement it.
- The projection itself — 062 owns it.

### Steps

1. `memory_store` accepts optional `replaces`: a substring of the entry being superseded.
   Resolve it against stored content; on exactly one match, delete that entry as part of
   storing the new one.
2. **Ambiguity is an error, not a guess.** More than one match returns a failure listing
   the candidate keys. Deleting the wrong memory silently is worse than making the agent
   be specific.
3. No match is also an error — `replaces` is a claim about existing state, and silently
   treating it as a plain store hides that the claim was wrong.
4. `memory_forget` accepts optional `contains` as an alternative to `key`, with the same
   ambiguity rules. Exactly one of `key` or `contains` must be given.
5. After a successful `core` write, sum the stored core content. Over the projection
   budget, append a notice to the tool output naming how much is over and what to do.

### Why the wall is soft here

Hermes refuses the write. That is right for Hermes, where the bounded file *is* the
memory: over budget means the fact cannot exist.

It is wrong here. Core memory beyond the projection budget still lives in the database
and is still recallable — only the always-injected block is bounded. Refusing the write
would destroy a working capability to simulate a constraint this architecture does not
have.

So the write succeeds and the tool result carries the signal: *stored, but core memory is
now N characters over the block budget; M entries are no longer injected — consolidate
with `replaces` or `memory_forget`.* The agent gets the same prompt to act that Hermes'
wall provides, without losing the storage.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
cargo test --lib tools::
```

Never a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

1. `store_with_replaces_supersedes_the_matching_entry` — old gone, new present.
2. `store_with_ambiguous_replaces_is_rejected` — two matches, nothing deleted, both keys
   named in the error. The one that matters: it is the difference between a correction
   and silent data loss.
3. `store_with_unmatched_replaces_is_rejected` — and nothing is stored.
4. `forget_by_contains_removes_the_entry`
5. `forget_by_ambiguous_contains_is_rejected` — nothing deleted.
6. `forget_requires_exactly_one_selector` — neither, or both, is an error.
7. `core_store_reports_when_the_projection_is_over_budget` — and stays silent under it.

Each checked against pre-change behaviour.

## Escape hatches

- If `replaces` would need a new `Memory` trait method to be implemented efficiently,
  STOP and report. A tool-level scan over `list` is the intended shape; obliging five
  backends is a different decision.
- If the capacity sum shows up as a cost on a large store, STOP and report — sampling or
  caching it changes the accuracy story and deserves its own call.

## Maintenance note

`replaces` and `contains` share their resolution rules; they are one helper for a reason.
Any future selector must keep the "exactly one match or error" contract — the moment one
of them guesses, the tool can delete the wrong memory.

## Rollback

Both parameters are optional and additive; existing calls are unaffected. `git revert` is
complete, and no stored data changes shape.
