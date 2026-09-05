# Plan 092: Standalone query rewriter: wire it or remove the knob

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/retrieve/standalone_query.rs src/kb/config.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: M (wire) / S (remove)
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`rewrite_standalone` (`src/kb/retrieve/standalone_query.rs:53`) turns a
follow-up like "tell me more about exclusions" into a self-contained search
query using prior turns. It is gated by `KB_STANDALONE_QUERY_ENABLED`
(`config.rs:92`), has a 215-line implementation with an LRU cache, a 6-turn
history window, and five documented fail-soft paths.

**It has zero production callers** — only `tests/kb/retrieve_test.rs:1100-1290`.

The parity gate notices the gap and routes around it: `parity_test.rs:101-108`
skips every `followup` entry with the comment *"depends on standalone-query
rewrite which is a separate test surface"*. So the one measurement that would
show multi-turn retrieval is broken deliberately excludes the case.

An operator setting `KB_STANDALONE_QUERY_ENABLED=true` gets nothing.

## Current state (verified at 2ca7e59)

```bash
grep -rn 'rewrite_standalone' src/ tests/ | grep -v 'standalone_query.rs:'
# tests/kb/retrieve_test.rs only
```

The function needs `chat_history: &[(String, String)]`. Nothing in the KB
retrieval path carries conversation history — `Retriever::retrieve` takes a
query string and `RetrieveOptions` (`retrieve/mod.rs:124`). The agent reaches
the KB by shelling out to `kb search`, which has no history either
(`cli.rs:48-63`). **That is the real reason it was never wired**: there is no
channel for the history to arrive on.

## The decision

**Option A — wire it.** Add an optional history input to the CLI (e.g.
`--history <file>` or stdin) and thread it through `Retriever::smart_retrieve`,
which is currently just an alias for `retrieve` (`retrieve/mod.rs:333-339`) and
was explicitly reserved for this.

- Pro: multi-turn KB questions start working; `smart_retrieve` gets its purpose.
- Con: the agent must pass history on every call, which means an ambient-hint
  change and more tokens per invocation. Non-trivial design.

**Option B — remove it.** Delete the module, the config field, and the tests;
un-skip or delete the `followup` branch in the parity gate accordingly.

- Pro: honest. There is no caller and no delivery channel for the input.
- Con: discards a complete, tested port.

Recommendation: **B for now**, with the module preserved in git history and a
one-line note in `docs/reference/kb.md` that multi-turn rewriting is not
implemented. Wiring it properly is its own feature with its own design, and
CLAUDE.md §3.2 says do not keep speculative paths waiting for a caller.

## Scope

**In scope**: one option, fully.

**Out of scope**: designing a history-passing protocol for the agent — if
Option A is chosen, that is a separate plan and this one blocks on it.

## Git workflow

```bash
git switch -c chore/remove-standalone-query-rewriter
```

## Steps (Option B)

### Step 1: Delete the module

- `src/kb/retrieve/standalone_query.rs`
- `pub mod standalone_query;` in `retrieve/mod.rs:12`

### Step 2: Remove the config surface

- `standalone_query_enabled` field (`config.rs:27`)
- its `from_env` line (`config.rs:92-93`)
- any mention in `docs/reference/kb.md` env table

**Verify**: `grep -rn 'KB_STANDALONE_QUERY_ENABLED' . --include='*.rs' --include='*.md'`
returns nothing but CHANGELOG history.

### Step 3: Update the parity gate

`parity_test.rs:101-108` skips `followup` entries citing this feature. Replace
the comment with the real reason (multi-turn rewriting is not implemented) so
the next reader is not sent looking for a module that is gone.

### Step 4: Delete the tests

`retrieve_test.rs:1109-1290` and the import at `:1100`.

### Step 5: Record the gap

One line in `docs/reference/kb.md`: follow-up questions are searched verbatim;
the agent should phrase KB queries self-containedly.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb retrieve_test
```

## Done criteria

- No module, no knob, no tests, and the parity gate's skip reason is accurate.
- Or, under Option A, a real caller plus a test proving a follow-up query is
  rewritten before search.

## STOP conditions

- A caller for `rewrite_standalone` exists that this plan did not find — stop
  and report; Option B would then be wrong.
