# Plan 018 (design/spike): Scope and prototype the KB OCR ingestion path (close the TODO(kb-ocr) gap)

> **Executor instructions**: This is a **design/spike** plan, not a
> build-everything plan. The deliverable is a written design + a minimal working
> prototype behind a feature sub-flag, plus a list of open questions — NOT a
> fully productionized OCR pipeline. Do not expand scope beyond what's here. If a
> STOP condition occurs, stop and report. When done, update the status row for
> this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/kb/`
> If `src/kb/` changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.
>
> **Feature note**: KB code is behind `--features kb`; build/test with it.

## Status

- **Priority**: P3
- **Effort**: M–L (coarse — this spike sharpens the estimate)
- **Risk**: MED
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The KB subsystem already ingests PDFs and images through its extract pipeline,
but scanned documents and image-embedded text are silently dropped from
retrieval: the OCR path is a stub that returns a typed error. The code explicitly
reserves the slot (`TODO(kb-ocr)`), and the surrounding machinery (extract
pipeline, embedder, file dispatch) already exists — OCR is a leaf that closes the
ingestion gap. This is adjacent-possible product value, but the port effort is
unscoped; this spike defines the API, prototypes the smallest end-to-end slice,
and lists the real open questions before anyone commits to the full build.

## Current state

- `src/kb/file/mod.rs:266-278` — `process_pdf` fails fast on the OCR path
  (verified):
  ```rust
  async fn process_pdf(cfg: &KbConfig, bytes: &[u8], opts: &ProcessingOptions) -> KbResult<String> {
      if opts.use_ocr_pipeline {
          // TODO(kb-ocr): port `src/lib/ocr` (Ollama models) in a later phase.
          // For now fail-fast rather than silently fall back to vision-LLM.
          return Err(KbError::Other("OCR pipeline not yet implemented; set use_ocr_pipeline=false".into()));
      }
      let primary = build_extractor(cfg, &cfg.extract_primary)?;
      ...
  }
  ```
- `src/kb/file/image.rs:74` — the same TODO for images ("same Ollama-OCR TODO as
  `process_pdf`"). Read it.
- The referenced source `src/lib/ocr` is a TypeScript origin ("port `src/lib/ocr`
  (Ollama models)"). Confirm what exists: `ls src/lib/ 2>/dev/null; grep -rn "ocr" src/kb/ | head -40`.
  The port target is the RantaiClaw KB in Rust, not TS — the TODO points at the
  behavior to reproduce, not a file to copy verbatim.
- The existing extract pipeline (the non-OCR path) is the integration point:
  `build_extractor`, `extract_with_fallback`, and the vision-LLM extractor
  (`src/kb/extract/vision_llm.rs`). Read `src/kb/extract/` to see how an extractor
  is defined and dispatched.
- `ProcessingOptions.use_ocr_pipeline` is the existing flag the caller sets. Find
  its definition and callers: `grep -rn "use_ocr_pipeline\|ProcessingOptions" src/kb/`.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (kb) | `cargo clippy --features kb --all-targets -- -D warnings` | exit 0 |
| KB tests | `cargo test --features kb kb::` | all pass |

## Scope

**In scope**:
- A design note: `docs/kb/ocr-design.md` (or `plans/notes/018-kb-ocr-design.md`)
  — the API, the chosen OCR backend, the sub-flag, the integration point, open
  questions, and a sharpened effort estimate.
- A minimal prototype behind a NEW feature sub-flag (e.g. `kb-ocr`), wiring one
  extractor implementation into `process_pdf`/image ingest — enough to prove the
  end-to-end slice works on one sample, gated so text-only KB users don't pay for
  it.
- Tests for the prototype slice (feature-gated, ideally against a mocked OCR
  endpoint via `wiremock`).

**Out of scope** (do NOT build in this spike):
- A production-grade, all-formats OCR pipeline.
- Bundling/downloading Ollama models or making OCR a default.
- Changing the non-OCR extract path.
- Any change that makes `--features kb` (without the new sub-flag) pull OCR deps.

## Git workflow

- Branch: `advisor/018-kb-ocr-pipeline-spike`
- Commit the design note first, then the gated prototype + tests. Messages e.g.
  `feat(kb): prototype OCR ingestion behind kb-ocr sub-flag (spike)`.
- Do NOT push or open a PR unless instructed. Open for review; do not self-merge.

## Steps

### Step 1: Investigate and write the design note

Read the extract pipeline and the two TODO sites. Produce `docs/kb/ocr-design.md`
answering:
1. What does "OCR pipeline" mean here vs the existing vision-LLM extractor — is
   OCR a *pre-router* (image → text, then normal ingest) or an *alternative
   extractor*? (The comment says "pre-routes through an Ollama OCR pipeline when
   `use_ocr_pipeline=true`".)
2. Which OCR backend: the referenced Ollama-model approach, or a Rust-native OCR?
   What dependency does each add, and what's the binary-size/latency cost
   (the project optimizes for size — this MUST be a sub-flag)?
3. The API: does it slot behind the existing `Extractor` trait
   (`src/kb/extract/`), so `build_extractor` can return an OCR extractor when
   `use_ocr_pipeline`? Prefer reusing the existing trait over a parallel path.
4. Feature-flag plan: a new `kb-office`-style sub-flag `kb-ocr = ["kb", ...]` in
   `Cargo.toml`, so OCR deps are opt-in.
5. Open questions / risks (model availability, offline behavior, per-page token
   budget, image formats).

**Verify**: the design note exists and answers all five points. This note is the
primary deliverable — a reviewer should be able to approve the *approach* from it.

### Step 2: Add the `kb-ocr` sub-flag

Add to `Cargo.toml` a `kb-ocr = ["kb", <deps>]` feature mirroring how `kb-office`
is declared (`Cargo.toml:277`). Add the OCR dependency(ies) as `optional = true`
pulled only by this feature.

**Verify**: `cargo build --features kb 2>&1 | tail -5` (WITHOUT kb-ocr) → compiles
and does NOT pull the OCR deps; `cargo build --features kb-ocr 2>&1 | tail -5` →
compiles.

### Step 3: Prototype one end-to-end slice

Behind `#[cfg(feature = "kb-ocr")]`, implement the smallest OCR extractor that
turns an image (or one image-PDF page) into text and returns it where
`process_pdf`/image ingest expects text. Replace the fail-fast `Err` with the
real path ONLY when `use_ocr_pipeline` AND the `kb-ocr` feature is on; keep the
typed error when the feature is off (so the behavior is honest without the flag).

Keep it to ONE working slice (e.g. a single image → OCR text). Do not chase every
format.

**Verify**: `cargo build --features kb-ocr 2>&1 | tail -5` → compiles;
`grep -n "OCR pipeline not yet implemented" src/kb/file/mod.rs` → still present
for the non-`kb-ocr` build (guarded), replaced under the feature.

### Step 4: Test the prototype slice

Feature-gated test (`#[cfg(feature = "kb-ocr")]`): feed a small known image (a
tiny fixture) or mock the OCR backend with `wiremock`, run the OCR extractor,
assert it returns the expected text. Model after existing KB extract tests
(`grep -rln "wiremock\|#\[cfg(test)\]" src/kb/extract/ tests/kb/`).

**Verify**: `cargo test --features kb-ocr ocr` → the prototype test passes.

## Test plan

- One prototype-slice test (above), feature-gated. Do NOT require a real Ollama
  server in CI — mock it, or gate the live test with `#[ignore]` like the existing
  live KB tests (`tests/kb/*`).
- Verification: `cargo test --features kb kb::` (OCR off) still passes;
  `cargo test --features kb-ocr ocr` (OCR on) passes.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `docs/kb/ocr-design.md` exists and answers the five Step-1 questions
- [ ] `cargo build --features kb` compiles WITHOUT pulling OCR deps
- [ ] `cargo build --features kb-ocr` compiles
- [ ] `cargo clippy --features kb-ocr --all-targets -- -D warnings` exits 0
- [ ] `cargo test --features kb kb::` passes (OCR off) AND `cargo test --features kb-ocr ocr` passes (prototype slice)
- [ ] The fail-fast typed error remains for `use_ocr_pipeline` builds WITHOUT `kb-ocr`
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The referenced `src/lib/ocr` / Ollama approach implies a heavy always-on
  dependency that can't be cleanly feature-gated — report the size/latency
  tradeoff for a maintainer decision before adding it.
- The existing `Extractor` trait can't accommodate an OCR extractor without a
  broad refactor — report the mismatch; do not refactor the pipeline in a spike.
- Making OCR work requires a running local model with no mockable seam for tests
  — report so the test strategy is decided.

## Maintenance notes

- This is a spike: the follow-up production plan (all formats, batching, per-page
  token budget) should be written from the design note + open questions, not
  improvised here.
- Reviewer should focus on the *design note* and the feature-gating (no OCR cost
  leaks into the default `kb` build), not on prototype polish.
- Keep the honest fail-fast error for un-flagged builds — silently downgrading to
  vision-LLM was explicitly rejected in the current code; don't reintroduce it.
