# Plan 097: Expand the capability block; implement or drop `fuzzy` resolution

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/axi/api.rs src/kb/intelligence/ src/kb/config.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: S–M
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Two related honesty gaps on the graph surface.

**1. The capability block cannot explain an empty graph.**
`Capability` carries two fields — `src/kb/axi/api.rs:576-580`:

```rust
struct Capability {
    intelligence_enabled: bool,
    extraction_model: String,
}
```

So the console cannot tell apart:

- intelligence is off,
- intelligence is on but no credential resolves (extraction fails per chunk and
  is swallowed — `extract/llm.rs:145-197`),
- GraphRAG is off so the graph never influences search,
- the document genuinely has no entities.

All four render the same empty state. `graph-lens.tsx:147-161` guesses
"disabled" from `intelligence_enabled` alone, and both it and
`doc-intelligence-drawer.tsx:99-101` tell the operator to set
`KB_INTELLIGENCE_ENABLED` — an instruction with no UI, TUI or config path
behind it.

**2. `fuzzy` resolution is advertised and does nothing.**
`src/kb/config.rs:49-51` documents `KB_INTELLIGENCE_RESOLUTION` as `exact` or
`fuzzy`. The parameter reaches `extract_document_intelligence` as
`_resolution: &str` — underscore-prefixed, never read
(`intelligence/mod.rs:34`), and the doc comment at `:28` admits "currently only
exact". `resolve.rs` is 16 lines and implements exactly one strategy. Setting
`fuzzy` is a silent no-op.

## Current state (verified at 2ca7e59)

- `Capability::from_cfg` — `api.rs:582-588`
- Set on both handlers — `api.rs:950`, `:968`
- `api_test.rs:684 graph_exposes_capability` asserts only the two existing
  fields, so **adding fields will not break it**
- `KbConfig.intelligence_resolution` — `config.rs:51`, `:115`
- Threaded to three call sites — `api.rs:1010`, `:1425`, `cli.rs:375`

## Scope

**In scope**: widen `Capability`; decide `fuzzy`; make both console empty
states use the richer signal.

**Out of scope**: adding a config surface for `KB_INTELLIGENCE_ENABLED`
(plan 102 owns config shape) — but this plan must stop telling operators to set
an env var if plan 102 lands a real toggle. Coordinate the wording.

## Git workflow

```bash
git switch -c feat/kb-capability-signal
```

## Steps

### Step 1: Widen `Capability`

```rust
#[derive(Debug, Serialize, Default)]
struct Capability {
    intelligence_enabled: bool,
    extraction_model: String,
    /// Whether a credential resolves for the extraction endpoint. The key
    /// itself is never returned — this is presence only, like
    /// `GET /config/knowledge`.
    credential_configured: bool,
    /// Whether retrieval augments results through the entity graph.
    graphrag_enabled: bool,
    /// Entity-resolution strategy actually in effect.
    resolution: String,
}
```

`from_cfg` fills them from `KbConfig`; use
`!KbConfig::resolve_key(&cfg.embedding_api_key).is_empty()` for
`credential_configured` — that is the same resolution
`build_intelligence_extractor` uses (`api.rs:737-742`), so the flag cannot lie.

**Verify**: `cargo test --features kb --test kb api_test` — `:684` passes
unchanged.

### Step 2: Decide `fuzzy`

**Option A — drop it (recommended).** Remove `fuzzy` from the doc comment
(`config.rs:49`), from `docs/reference/kb.md`, and reject an unknown value at
config load with a clear `KbError::Config` rather than accepting it silently.
Report `resolution: "exact"` in the capability block. Keep the parameter
threaded — it is the seam a real implementation would use.

**Option B — implement it.** Add a normalized-similarity merge in `resolve.rs`
behind the strategy string. That is a real feature with real risk (wrong merges
corrupt the graph irreversibly) and deserves its own plan; do not do it inside
this one.

Take **A**. Advertising a strategy that does nothing is worse than not offering
it.

**Verify**: `KB_INTELLIGENCE_RESOLUTION=fuzzy rantaiclaw kb graph` errors with
an actionable message instead of silently behaving as `exact`.

### Step 3: Use the richer signal in the console

`claw-ui`:

- `graph-lens-helpers.ts:15-24` — add a `"no-credential"` state, returned when
  `cap.intelligence_enabled && !cap.credential_configured`.
- `graph-lens.tsx:147-175` — render a distinct hint for it: extraction is on
  but no key resolves, add one under Knowledge Base settings.
- `doc-intelligence-drawer.tsx:93-104` — consume `capability` (the type already
  carries it, `types.ts:242`) instead of the fixed "may not have run yet" text.

**Verify**: with `KB_INTELLIGENCE_ENABLED=true` and no key, the graph tab says
the credential is missing rather than "No graph yet".

### Step 4: Add the state test

`graph-lens-helpers.ts` has no test file today. Add one covering all five
states of `deriveGraphState` — loading, disabled, no-credential, empty, ready.
It is a pure function; the test is cheap and it is the only thing pinning the
console's honesty here.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb api_test
cd ../claw-ui && npx vitest run && npx next build
```

## Done criteria

- The capability block answers "why is this empty" for every case.
- `fuzzy` either works or is refused; it is never silently ignored.
- Both console empty states distinguish disabled from missing-credential.

## STOP conditions

- Plan 102 has landed and `KB_INTELLIGENCE_ENABLED` is no longer the way to
  enable extraction — update the hint text to match the real control rather
  than shipping a second wrong instruction.
