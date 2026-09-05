# Plan 157: curated memory — teach the agent to save facts, not echoes

> **Executor instructions**: One PR, one concern: making the *curated* memory
> tier actually fill up, so `/memory` reads like a fact sheet instead of an
> echo log. This is a product feature, not a bugfix — read "Design borrowed
> from the survey" first; the two mechanisms below are deliberately the small
> subset of what other runtimes do. If anything under "STOP conditions"
> occurs, stop and report. When done, add this plan's row in
> `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat d0089a4..HEAD -- src/agent/agent.rs src/agent/loop_.rs src/tools/memory_store.rs src/agent/prompt*.rs src/memory/context.rs`
> Line anchors below are from `d0089a4`. If this diff is non-empty, re-verify
> each cited line before editing. (If plans 153-155 merged first — expected —
> the context.rs anchors will have moved; the mechanisms here do not overlap
> with those diffs.)

> **EXECUTION NOTE (2026-08-17)**: recon at execution time found the
> pre-compaction flush ALREADY BUILT — `Agent::flush_durable_memory`
> (agent.rs, dedicated flush prompts in compaction.rs, restricted tool set,
> bounded iterations, best-effort), wired into `compact_streaming` (covers
> `/compress` and TUI auto-compaction). Step 3 was therefore dropped. The
> shared-loop `auto_compact_history` (channel path) deliberately does NOT
> flush — guest content must not be promoted to durable memory (the plan's
> own taint boundary). Shipped scope = standing nudge (`MemorySection`,
> gated to PromptSurface::Agent + memory_store present) + docs. See the PR.

## Status

- **DONE 2026-08-17 (reduced scope)** — PR #559 (merged c76bac9), released **v0.22.0-alpha**; flush step dropped as already built, see execution note above

- **Priority**: P2 — after plans 153/154 stop the echo injection, the store
  is *clean but empty*: the operator's live `brain.db` held 55 rows, 100%
  auto-saved echoes, **zero** curated facts. Nothing ever prompts the model
  to distill one
- **Effort**: M
- **Risk**: LOW-MEDIUM (prompt-surface change affects every turn's behaviour;
  no schema change)
- **Depends on**: plan 153 merged (defines the contract this plan fills:
  injection = curated tier only). Best after 154
- **Category**: feature (memory / prompts)
- **Planned at**: `d0089a4`, 2026-08-17

## Why this matters

Live evidence (operator profile, 2026-08-17): 55 memory rows — 34 TUI echoes
(`"hello"` three times), 20 Telegram echoes, 1 legacy stray, **0 facts**. The
injection pipeline, the `/memory` view, and `memory_recall` all had nothing
of value to work with, because nothing in the product ever *creates* value:
auto-save copies raw turns, and the system prompt never tells the model when
to call `memory_store`.

## Design borrowed from the survey (2026-08-17, plans 153-156 context)

Every runtime with "smooth" memory fills its curated tier deliberately:
Hermes nudges the model to curate two small files; OpenClaw adds a
**pre-compaction flush** (a silent turn: "you are about to lose context —
write what matters to memory now") plus offline promotion; Claude Code has
the model write topic files as it works. RantaiClaw already has the storage
(`memory_store` → `Core` category, shared tier) — this plan adds the two
cheapest triggers and explicitly skips the expensive third:

1. **Standing nudge** (Hermes-style) — system-prompt guidance.
2. **Pre-compaction flush** (OpenClaw-style) — save before history is
   summarized away.
3. ~~Offline distillation/"dreaming"~~ — **rejected for now** (YAGNI): needs
   a scheduled job, a raw-turn source (auto-save may be off), and a real
   taint-gating design for channel content. Revisit only with a concrete
   operator ask.

## Steps

1. **Standing nudge** — extend the system prompt built by
   `SystemPromptBuilder` (`src/agent/agent.rs:544` wires it; find the
   builder's template): a short section telling the agent to save durable
   facts (user preferences, stable project context, standing corrections)
   via `memory_store` with a descriptive `snake_case` key and category
   `core`, to update the existing key instead of duplicating (store upserts
   on key conflict — `agent.rs:986-989` comment documents this), and NOT to
   save one-off conversational detail. Keep it under ~6 lines; this text
   rides every request on every surface. The prompt must not instruct
   saving anything a *non-owner* says as fact (see taint note below).
2. **Sharpen the tool contract** — `src/tools/memory_store.rs`: the tool
   `description` gains the same key/category guidance plus (from plan 153)
   the note that `conversation`-category entries are never auto-injected.
   The model reads this schema on every call; it is the cheapest place to
   shape behaviour.
3. **Pre-compaction flush** — the auto-compaction path
   (`src/agent/loop_.rs:195-235`: `compact_history` → summarize →
   `apply_compaction_summary`) currently distills turns into a *history*
   summary only. Before compacting, run one silent side-request (same
   provider, same pattern as the summarizer call at `loop_.rs:220`) asking
   the model to emit durable facts from the turns about to be compacted as
   `(key, content)` pairs; store each via the memory handle with category
   `Core` and the active conversation scope's shared tier (unscoped — these
   are facts, not transcript). Cap: at most 5 facts per flush, each ≤ 300
   chars; drop the rest. A flush failure must not block compaction
   (log + continue — same degradation contract as the summarizer fallback at
   `loop_.rs:222-228`).
4. **Taint boundary** (required, small): curation writes happen only on
   owner-driven agents. Concretely: the flush and nudge run in the
   interactive agent and gateway; the **channel** dispatch path (guests
   possible) keeps the nudge for owners only if the prompt path
   distinguishes sender role (it does — sender ownership is computed at
   `src/channels/dispatch.rs:415-419`), otherwise omit the nudge on guest
   turns. Guests already cannot call `memory_store` directly
   (`guest_allowed_tools` empty by default) — do not weaken that.
5. **Docs** — `docs/reference/config.md` memory section: describe the
   curated tier, the nudge, the flush, and that `auto_save` (raw echoes) is
   now optional context for explicit search only, safe to disable.

## What `/memory` should look like after a week of use

```
[core] user_language     Prefers Bahasa Indonesia; English for code
[core] user_timezone     WIB (UTC+7)
[core] project_context   Developing RantaiClaw (Rust agent runtime)
[core] answer_style      Concise answers, tables for comparisons
```

## Tests (write first, watch them fail)

- Prompt: builder output contains the nudge section (and a guest-path prompt
  does not, if step 4 takes the role-aware branch).
- Flush: with a mock provider scripted to return two facts, compaction
  stores exactly those two `Core` entries and still compacts; with a
  provider that errors, compaction proceeds and stores nothing (degradation
  contract). Cap test: 7 returned facts → 5 stored.
- Tool description: schema text mentions `core` guidance (pins step 2
  against silent regression).
- **Mutation check**: disable the flush call — the two-facts test must go
  red.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib agent::
cargo test --lib tools::memory_store
```

Live drive: fresh sandbox profile (`RANTAICLAW_CONFIG_DIR`), tell the agent
two facts about yourself, chat past the compaction threshold (or trigger
`/compress`), then `rantaiclaw memory list --category core` — the facts are
there with sane keys; `rantaiclaw memory list --category conversation` shows
nothing new when `auto_save = false`.

## STOP conditions

- The flush's extra provider call per compaction is deemed too costly for a
  configured provider (compaction is rare — roughly one call per long
  conversation — but if review pushes back, gate the flush behind a
  `memory.precompaction_flush = true` default-on config key and bump the
  schema version, since that changes a fingerprinted default).
- The system-prompt builder has no seam for surface/role-aware sections and
  adding one would touch every surface's prompt assembly — report the
  refactor cost before doing it.

## Rollback

Single revert removes nudge + flush; stored facts remain (they are ordinary
`Core` entries the operator can `memory clear --category core` if unwanted).
