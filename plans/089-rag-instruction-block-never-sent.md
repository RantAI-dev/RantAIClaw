# Plan 089: Wire or delete the RAG instruction block

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/retrieve/format.rs src/kb/axi/`
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
- **Depends on**: 088 (soft — decide after seeing the new agent output)
- **Category**: direction
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`format_context_for_prompt` (`src/kb/retrieve/format.rs:15`) builds a careful
instruction block: treat excerpts as source of truth, cite inline as
`[Document Title — Section]`, say "not specified in the available excerpts"
rather than guessing. Its module doc (`format.rs:5-8`) says:

> The exact wording is load-bearing; do NOT trim or paraphrase without
> re-running the evals at `tests/fixtures/rag-golden.json`.

**It has zero production callers.** The only references are
`tests/kb/retrieve_test.rs:1029-1090`. No model has ever received it.

So the citation discipline, the refusal wording, and the "don't substitute
general knowledge" rule are all aspiration, not behaviour. That matters
directly: without them the agent has no instruction to cite sources or to admit
a gap, which is exactly the failure mode the block was written to prevent.

## Current state (verified at 2ca7e59)

```bash
grep -rn 'format_context_for_prompt' src/ tests/
# src/kb/retrieve/format.rs:15  (definition)
# tests/kb/retrieve_test.rs:8,1029,1039,1054,1090  (tests only)
```

The agent's KB path is `cmd_search` (`cli.rs:220`), which prints TOON. There is
no place in the current pipeline where a prompt fragment is assembled — the
agent shells out and reads stdout.

## The decision

Two honest options. Pick one, record it in the PR body, do not do half of each.

**Option A — wire it (recommended).** Emit the instruction block once at the
top of `cmd_search`'s output, above the context. The agent reads stdout as tool
output, so the rules land in the same place the excerpts do.

- Pro: the block does the job it was written for; citations become likely.
- Con: adds fixed tokens to every KB call. Mitigate by emitting it only when
  `result.context` is non-empty (no excerpts, no rules needed).

**Option B — delete it.** Remove `format.rs`, its `pub mod format;` line
(`retrieve/mod.rs:9`), and the four tests. Record in the PR that RAG-answer
discipline is unowned.

- Pro: removes a module that lies about being load-bearing (CLAUDE.md §3.2).
- Con: leaves nothing instructing the agent to cite or to refuse cleanly.

Default to **A** unless measurement after plan 088 shows the token cost is
material.

## Scope

**In scope**: one of the two options above, end to end, including tests.

**Out of scope**: rewording the block. It is a verbatim port; changing the text
is a separate, evidence-backed change.

## Git workflow

```bash
git switch -c feat/wire-rag-instruction-block   # or chore/… for option B
```

## Steps (Option A)

### Step 1: Emit the block in `cmd_search`

```rust
    } else {
        if !result.context.is_empty() {
            println!("{}", format_context_for_prompt(&result));
        }
        print!("{}", format_search_toon(&result.chunks));
    }
```

`format_context_for_prompt` already embeds `result.context` and the source list
(`format.rs:35-53`), so this replaces — not supplements — the bare context
print added by plan 088. Re-read that branch before editing.

**Verify**: `rantaiclaw kb search` output starts with
`## Knowledge Base Context` and ends with a `Sources:` list.

### Step 2: Keep the existing tests, add one for the wiring

`retrieve_test.rs:1044 format_includes_instruction_block_verbatim` stays as-is.
Add a `cli_test.rs` assertion that the search output contains
`## Knowledge Base Context` — that is the guard that the function is reachable,
which is the thing that was missing.

**Verify**: the new assertion fails if you revert Step 1.

## Steps (Option B)

1. Delete `src/kb/retrieve/format.rs` and the `pub mod format;` line.
2. Delete `retrieve_test.rs:1029-1090` and its import at `:8`.
3. Note the gap in `docs/reference/kb.md`.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb retrieve_test
cargo test --features kb --test kb cli_test
```

## Done criteria

- Either the block reaches the agent and a test proves it, or the module and
  its tests are gone and the gap is documented.
- No third state where the module exists with tests but no caller.

## STOP conditions

- Plan 088 has not landed: the output branch this edits will not exist as
  described. Land 088 first or re-derive the edit.
