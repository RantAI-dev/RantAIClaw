# Plan 088: Agent KB search must return usable content, not a 120-character preview

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/axi/cli.rs src/kb/axi/ambient.rs src/kb/retrieve/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P1
- **Effort**: S–M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

The agent has exactly one way to reach the Knowledge Base, and that path throws
away almost everything retrieval produced.

There is deliberately **no** `Tool` implementation for the KB — `src/tools/mod.rs`
contains zero KB references, and `src/kb/axi/ambient.rs:5-8` states the design:
the agent shells out to `rantaiclaw kb search`. The ambient hint it injects
(`ambient.rs:31-33`) tells the agent to run:

```
rantaiclaw kb search "<question>" --top 5
```

— without `--json`. That lands on `cmd_search`'s non-JSON branch
(`src/kb/axi/cli.rs:262`), which prints `format_search_toon(&result.chunks)`
and nothing else. That formatter (`cli.rs:785-802`) emits four columns, and
`content_preview` is truncated at `CONTENT_PREVIEW_CHARS = 120` (`cli.rs:43`).

Chunks are built at 800 characters (`SmartChunkOptions::default`,
`chunk/smart.rs:72`). **The agent sees 15% of each chunk.**

Two further losses on the same line:

- `result.context` — the full chunk text joined with `---`, plus the document
  inventory — is built by `Retriever::retrieve` (`retrieve/mod.rs:283-302`) and
  then discarded unless `--json` is passed.
- When no chunk crosses the similarity threshold, `retrieve` returns
  `RetrievalResult { context: inventory, chunks: vec![] }`
  (`retrieve/mod.rs:262-268`). The inventory exists so enumeration questions
  ("what's in here?") still see the document list. On the TOON path the agent
  gets `chunks[0]{...}:` and concludes the KB is empty — while `context` holds
  "## Documents in this knowledge base (40):".

Everything downstream of this — better chunking, better embeddings, reranking —
is invisible while the agent reads 120-character fragments.

## Current state (verified at 2ca7e59)

`src/kb/axi/cli.rs:250-264`:

```rust
    if json {
        let payload = serde_json::json!({
            "context": result.context,
            "sources": result.sources.iter().map(source_to_json).collect::<Vec<_>>(),
            "chunks": &result.chunks,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print!("{}", format_search_toon(&result.chunks));
    }
```

`src/kb/axi/cli.rs:43`: `const CONTENT_PREVIEW_CHARS: usize = 120;`

`src/kb/axi/ambient.rs:29-35` is the only production producer of the agent's
KB instruction.

## Design decision (already taken — do not re-open)

Emit the retrieval **context** on the TOON path and raise the preview, rather
than switching the ambient hint to `--json`. Reasons:

- TOON exists to be cheaper than JSON for LLM context (`axi/toon.rs:1-11`).
  Telling the agent to use `--json` discards that on every KB call.
- `context` already carries per-chunk source headers (`[Title - Section]`) and
  the inventory — it is the shape the retrieval layer built for a model to read.

## Scope

**In scope**: `cmd_search` output, `CONTENT_PREVIEW_CHARS`, the empty-result
path, and the ambient hint wording if it needs to match.

**Out of scope**: `format_context_for_prompt` — that is plan 089. Do not wire
it here; the two plans must stay independently revertable.

## Git workflow

```bash
git switch -c fix/agent-kb-search-output
```

## Steps

### Step 1: Raise the preview to a useful width

Change `CONTENT_PREVIEW_CHARS` from `120` to `600` and update the doc comment
to say why the number exists (a TOON row must stay readable; the full text is
carried separately by the context block below).

**Verify**: `cargo test --features kb --test kb cli_test` — note which tests go
red; `cli_test.rs:383 kb_ingest_then_search_returns_toon` is the expected one.

### Step 2: Emit the retrieval context alongside the chunk table

In `cmd_search`'s non-JSON branch, print the context block before the TOON
table when it is non-empty:

```rust
    } else {
        // The TOON table is the machine-readable index; `context` is what the
        // model actually reads from. It carries per-chunk `[Title - Section]`
        // headers and the document inventory, and is the only place a
        // zero-chunk result still reports what the KB contains.
        if !result.context.is_empty() {
            println!("{}", result.context);
        }
        print!("{}", format_search_toon(&result.chunks));
    }
```

**Verify**: a manual `rantaiclaw kb search` on a seeded DB prints the context
block then the table.

### Step 3: Make the empty-result case honest

With Step 2 in place, a zero-chunk result already prints the inventory. Confirm
it, and add an explicit line so the agent is not left guessing:

```rust
        if result.chunks.is_empty() {
            println!(
                "No chunk crossed the relevance threshold. The documents listed \
                 above are present in scope — try a more specific query."
            );
        }
```

**Verify**: search a seeded DB for a nonsense string; the output lists the
documents and says no chunk matched, instead of an empty table.

### Step 4: Update the ambient hint if the wording no longer matches

Re-read `ambient.rs:29-35`. If the instruction implies a shape the command no
longer emits, correct it. Keep it one short block — it is injected into every
system prompt.

**Verify**: `cargo test --features kb --test kb agent_integration_test` passes.

### Step 5: Update `cli_test.rs`

`kb_ingest_then_search_returns_toon` asserts the old shape. Rewrite it to
assert the new one, and add an assertion that a zero-hit search still names a
seeded document. That second assertion is the regression guard for the bug
this plan fixes.

**Verify**: both assertions fail if you revert Step 2.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb cli_test
cargo test --features kb --test kb agent_integration_test
```

End-to-end, driving the real binary:

```bash
cargo build --release --features kb
KB_DB_PATH=/tmp/kbplan88.db ./target/release/rantaiclaw kb ingest ./README.md
KB_DB_PATH=/tmp/kbplan88.db ./target/release/rantaiclaw kb search "what is this project" --top 3
# expect: context block with [Title] headers and full chunk text, then the table
KB_DB_PATH=/tmp/kbplan88.db ./target/release/rantaiclaw kb search "zzzz nonsense" --top 3
# expect: the document inventory + the no-match line, NOT an empty table alone
```

## Done criteria

- A normal search prints chunk text a model can answer from.
- A zero-hit search still tells the agent what the KB contains.
- `cli_test` pins both.

## STOP conditions

- `Retriever::retrieve` no longer populates `context` — the whole plan rests on
  it; stop and report.
- Output size becomes a problem in practice (very large `--top`): stop and
  raise it rather than silently capping, so the trade-off is decided
  explicitly.

## Maintenance notes

`--json` keeps its existing shape. If a future change alters `context`, this
command is now a consumer — grep for `result.context` before editing
`retrieve/mod.rs:283-302`.
