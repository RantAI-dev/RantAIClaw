# Plan 153: memory context must never inject auto-saved conversation rows

> **Executor instructions**: One PR, one concern: the `[Memory context]`
> injection filter. Read the whole plan before editing. Run every verification
> command; the live repro in step 4 must fail **before** the fix and pass
> **after** (probe needs a control). If anything under "STOP conditions"
> occurs, stop and report. When done, add this plan's row in
> `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat d0089a4..HEAD -- src/memory/context.rs src/channels/dispatch.rs src/agent/agent.rs src/tools/memory_store.rs`
> All line numbers below are from `d0089a4`. If this diff is non-empty,
> re-verify each cited line before editing.

## Status

- **DONE 2026-08-17** — PR #555 (merged 286a225), released **v0.22.0-alpha**; live-verified in tmux with the 55 echo rows still present as the A/B control

- **Priority**: P0 — live defect, reproduced twice by the operator on
  2026-08-16/17; a stale request injected from memory made the model execute
  tools (`skills_list`, `glob_search`) the user never asked for
- **Effort**: S
- **Risk**: LOW (read-side filter only; no schema change, no data migration)
- **Depends on**: none
- **Category**: bugfix (memory)
- **Planned at**: `d0089a4`, 2026-08-17

## Why this matters — two live repros

**Repro 1 (cross-surface).** In the TUI the operator typed
`berikan saya cerita cinderella`. The TUI showed
`↺ recalled 2 memories: telegram_sulthannauval_telegram_1360247715_256, …`
and the model answered a **different, older Telegram request** (a Python
parallelogram tutorial) instead of the question asked. The recalled row exists
in `profiles/default/workspace/memory/brain.db`:

```
key:        telegram_sulthannauval_telegram_1360247715_256
session_id: telegram:sulthannauval
category:   conversation
content:    "coba coba berikan saya snippet code untuk menghitung luas
             jajargenjang dalam python berikut penjelasan detail..."
```

**Repro 2 (same-surface, worse).** The operator typed `hello`. The TUI showed
`↺ recalled 4 memories: 4 from this conversation` (four `user_msg_<uuid>`
auto-saves), and the model treated a **previous** request as the current one:
it called `skills_list()` and `glob_search({"pattern":"**/*"})` and listed
installed skills — tool execution triggered by a stale injected request, not
by the user. A stale request that can trigger side-effectful tools is the
severity driver here.

## Root cause

Every auto-save path stores raw conversation turns with
`MemoryCategory::Conversation`:

- channel dispatch: `src/channels/dispatch.rs:343-372` — key
  `{channel}_{sender}_{msg_id}` (built at `dispatch.rs:27-29`), stored with
  `MemoryCategory::Conversation`
- interactive agent (TUI/CLI): `src/agent/agent.rs:985-997` — key
  `user_msg_<uuid>`, stored with `MemoryCategory::Conversation`

The injection filter `should_skip` (`src/memory/context.rs:74-90`) knows
nothing about category. It guesses from **key shape**: it drops
`assistant_resp*` (legacy) and `*_history` keys. Channel auto-save keys
(`telegram_..._256`) and `user_msg_*` keys match neither pattern, so raw
conversation turns enter the prompt as `[Memory context]` — and because
`normalize_entry_scores` rescales relative to the best hit, the top-ranked
row always clears `min_relevance_score`, so *something* is injected nearly
every turn.

This also broke an invariant RantaiClaw inherited from ZeroClaw: auto-saved
message rows are stored for retention/explicit search but are **excluded from
context assembly**. ZeroClaw enforced it by key prefix; RantaiClaw's channel
auto-save introduced a new key scheme and the prefix filter silently stopped
covering it. None of the surveyed runtimes (OpenClaw, Hermes, Claude Code,
ZeroClaw) auto-injects raw conversation turns into prompts.

## The fix

Filter on the signal that is already explicit instead of guessing from key
shape: **entries with `category == MemoryCategory::Conversation` never enter
the `[Memory context]` block.**

Why this is the right cut:

- Every auto-save write site already uses this category; no write-side change
  and no migration — legacy rows (including the two repro rows above) are
  excluded at read time.
- Category survives every backend (sqlite, postgres, markdown routes files by
  category; verified `src/memory/markdown.rs:239-266` preserves category), so
  the guarantee is backend-independent.
- Conversation continuity is not this block's job and is not lost: channel
  history persists via `src/channels/history_store.rs` and the agent keeps
  in-memory + session-store history. The header comment of `context.rs`
  already states conversation history "duplicates what the transcript already
  carries".
- The rows stay in `brain.db`, still reachable via the explicit
  `memory_recall` tool and still pruned by `src/memory/hygiene.rs` retention.

### Steps

1. **`src/memory/context.rs`** — change `should_skip` to take the entry (or
   add the category check at its call site in `build_memory_context`,
   line 166): return `true` when
   `entry.category == MemoryCategory::Conversation`. Keep the existing
   key-shape checks (they still catch legacy rows stored under other
   categories). Update the function's doc comment: the primary rule is now
   category-based; key-shape rules are legacy backstops.
2. **`src/tools/memory_store.rs`** — the model can choose a category when
   storing. Update the tool's `description`/schema text so the model knows
   `conversation`-category entries are never auto-injected (stored for
   explicit recall only). No behaviour change in the tool itself.
3. **Docs** — `docs/reference/config.md` memory section (and
   `docs/reference/commands.md` if it documents `/memory`): one sentence —
   auto-saved conversation turns are retained and searchable but never
   auto-injected into prompts.

### Tests (write first, watch them fail)

In `src/memory/context.rs` tests (fixtures exist — `entry()` helper at
line 256 currently hard-codes `MemoryCategory::Core`, so existing tests keep
passing untouched; add a category parameter or a second helper):

- `a_conversation_category_entry_never_reaches_the_prompt`: entry with
  category `Conversation`, score 1.0, innocuous key (`telegram_chat_42`) →
  block must not contain it; `ctx.keys` must not name it.
- Control: identical content/score under `Core` → survives. (Without the
  control the test passes vacuously if recall returns nothing.)
- Repro-shaped test: two `Conversation` rows (one `user_msg_<uuid>`, one
  `telegram_user_a_telegram_123_7`) at scores 1.0/0.9 plus one `Core` fact at
  0.5 → only the `Core` fact is injected, and re-ranking (the existing
  echo-removal renormalize at `context.rs:146-151`) does not resurrect them.
- **Mutation check**: comment out the new category condition — all three new
  tests must go red. If any stays green, the test is vacuous; fix the test.

### Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
cargo test --lib agent::
cargo test --lib channels::
```

4. **Live drive (before AND after)**: run the built binary's TUI against the
   operator profile (or a copy of its `brain.db` under a sandbox
   `RANTAICLAW_CONFIG_DIR`). Type a short greeting. Before the fix: the
   `↺ recalled …` line names `user_msg_*`/`telegram_*` rows and the model may
   answer a stale request. After: those rows never appear in the recalled
   list; curated `Core` facts still can. Verify you are driving the freshly
   built binary, not a stale install.

## STOP conditions

- A write site stores conversation turns under a category **other than**
  `Conversation` (grep `MemoryCategory::` in `src/channels/` and
  `src/agent/` first) — the filter would miss it; report before proceeding.
- Any existing test depends on conversation-category entries being injected
  (that would be a test encoding the bug — report it, do not adapt the fix
  around it).

## Rollback

Single revert of the PR restores prior behaviour; no data was migrated.

## Non-goals

- Conversation scoping for the TUI (plan 154), threshold semantics
  (plan 155), `memory_recall` scoping (plan 156).
