# 064 — Compaction never promotes anything into memory

- **Finding:** #18 (memory deepscan, wave 3)
- **Written against:** `5954875`
- **Risk tier:** medium (`src/agent/**`)
- **Effort:** M (scoped S; the tool-set question below made it M)
- **Depends on:** 063 (`replaces` is what makes a flush consolidate rather than pile up)
- **Blocks:** nothing

## Problem

`/compress` calls `compact_streaming`, which produces a structured summary and swaps it
into history. The summary even has a `## Key facts established` section.

But that summary is a **system message in this session's history**. Nothing reaches
`brain.db`. When the session ends, every durable fact the conversation surfaced is gone
unless the agent happened to call `memory_store` at the time.

OpenClaw runs a silent turn before compaction asking the agent to save what matters.
Nothing equivalent happens here — compaction is the natural moment to promote facts, and
it passes unused.

### What is *not* wrong

The summary is preserved, so nothing is lost *within* the session, and the session record
persists to `sessions.db`. The gap is narrower than "compaction loses data": facts do not
become **durable memory**, which is what survives into the next session's prompt.

## Approach

A dedicated flush turn, agent-initiated, before the summary is built.

### Why not scrape the summary instead

Parsing `## Key facts established` and storing the bullets would cost nothing extra — the
summary is already paid for. It is also exactly the mistake this codebase already made
once: `is_assistant_autosave_key` exists because model-authored summaries were
auto-saved and then re-injected as facts, letting fabrications harden.

Scraping repeats that. A tool call does not: the agent chooses what to store, and it goes
through the same gate as any other write.

### Tool set

The flush turn is given **only the memory tools**, built fresh from the agent's own
`memory` and `security` handles. Not a subset of the live registry — a new small `Vec`,
which needs no change to the shared loop's signature.

That matters for more than tidiness. A flush turn holding `shell` could take an action
while nominally tidying up.

## Change

### Files in scope

- `src/agent/agent.rs` — the flush turn, called from `compact_streaming`
- `src/agent/compaction.rs` — the flush prompts

### Files explicitly out of scope

- `run_structured_loop` — reused as-is; no signature change
- The compaction summary itself, its prompts and its envelope
- `loop_.rs`'s CLI compaction path — it has no `Agent` and no memory handle to flush with

### Steps

1. Add flush prompts to `compaction.rs`, next to the compaction ones: store durable
   facts only, use `replaces` when correcting, do nothing if nothing qualifies.
2. Add `Agent::flush_durable_memory(&to_compact)`, building a two-tool registry
   (`memory_store`, `memory_forget`) and driving `run_structured_loop` over a **scratch**
   history that is discarded. Only the memory writes survive.
3. Call it from `compact_streaming` before the summary request.
4. Best-effort throughout: no security policy means skip, and any error is logged and
   swallowed. Compaction must never fail because the flush did.
5. Cap tool iterations low. This is one bounded errand, not an agentic session.

### Cost

One extra model call per compaction, plus its tool rounds. `/compress` is user-initiated
and infrequent, and the alternative is losing the session's facts. Worth stating plainly
rather than hiding.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib agent::
cargo test --lib memory::
```

## Test plan

1. `compaction_flush_stores_durable_facts` — a scripted provider that calls
   `memory_store`; the entry must exist afterwards.
2. `compaction_flush_does_not_pollute_history` — the scratch turn must leave no trace in
   the agent's history; only the summary envelope belongs there.
3. `compaction_succeeds_when_the_flush_turn_fails` — a provider that errors on the flush
   call must not break compaction.
4. `compaction_flush_exposes_only_memory_tools` — assert the registry handed to the loop
   holds exactly the memory tools. This is the safety property: a flush turn must not be
   able to reach `shell`.

Test 4 is the one to write first. The others describe convenience; that one describes
what happens if the tool set is ever widened by accident.

## Escape hatches

- If driving `run_structured_loop` over a scratch history turns out to mutate agent state
  in some way I have not seen, STOP and report rather than working around it.
- If the extra call shows up as a latency problem on `/compress`, STOP and report —
  making it conditional is a product decision, not this plan's.

## Maintenance note

The flush registry is built explicitly, not filtered from the live one. Anyone adding a
tool to the agent does not accidentally hand it to the flush turn — which is the point,
and the reason test 4 exists.

## Rollback

`git revert` removes the flush; compaction returns to summary-only. No schema, config or
stored-data change.
