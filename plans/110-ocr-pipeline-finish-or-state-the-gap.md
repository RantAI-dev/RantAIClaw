# Plan 110: OCR: finish the wiring or state the gap honestly

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/file/ src/kb/extract/ocr_ollama.rs docs/kb/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: M (wire) / XS (document)
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

`ProcessingOptions.use_ocr_pipeline` exists, is documented, and returns an error
whichever way it is set.

`src/kb/file/mod.rs:280-299`:

```rust
        #[cfg(not(feature = "kb-ocr"))]
        {
            // TODO(kb-ocr): port `src/lib/ocr` (Ollama models) in a later phase.
            return Err(KbError::Other(
                "OCR pipeline not yet implemented; set use_ocr_pipeline=false".into(),
            ));
        }
```

and the same shape at `file/image.rs:88-90`.

The spike **did** land — plan 018 shipped `OllamaOcrExtractor`
(`extract/ocr_ollama.rs`, 155 lines, commit `6347c69`) behind the non-default
`kb-ocr` feature. But it is reachable only from
`file/image.rs:137 process_image_via_ocr`, for single images, and it is **not**
reachable from `build_extractor` (`extract/mod.rs:75-121`), so no PDF path can
use it. The honest state is: images have an OCR option under a non-default
feature; PDFs do not, because PDF-page rasterization was deliberately left out
of the spike (`file/mod.rs:272-279` explains why).

The problem is not the deferral — it is that the deferral is only legible to
someone reading three source files. Operators see a flag they can set and an
error telling them to unset it.

## Current state (verified at 2ca7e59)

- `kb-ocr` is not in `default` (`Cargo.toml:246`)
- `OllamaOcrExtractor::from_env` reads `KB_EXTRACT_OCR_BASE_URL` /
  `KB_EXTRACT_OCR_MODEL` (`ocr_ollama.rs:81-93`) — **not** `KbConfig`, so it
  cannot see a console-entered key
- Design note: `docs/kb/ocr-design.md`
- Tests: `extract_test.rs:772,800`; `file_test.rs:354,383` pin both error paths

## The decision

**Option A — document the gap and narrow the flag (recommended, XS).**
Keep the current behaviour, but make it discoverable:

- `docs/reference/kb.md` gains an OCR section: images only, `--features kb-ocr`,
  the two env vars, and that PDFs route through the vision-LLM extractor
  instead.
- The error messages name the real alternative
  (`KB_EXTRACT_PRIMARY=<vision model>`) rather than telling the operator to
  turn their own flag off.
- `DocumentTypeHint` (`file/mod.rs:41-49`) is consumed by nothing; note that
  in its doc comment or drop it.

**Option B — wire OCR into the extractor factory (M).**
Add an `"ocr"` sentinel to `build_extractor` so `KB_EXTRACT_PRIMARY=ocr` works
for images, and route `OllamaOcrExtractor` through `KbConfig` instead of its own
env reads. PDF rasterization stays out of scope — the sentinel errors for PDFs
with a message that says so.

Take **A** unless there is a live request for local OCR. The spike's own design
note lists PDF rasterization as an open question with a real dependency cost;
shipping half a sentinel invites the same confusion at a different door.

## Scope

**In scope**: one option, plus honest error text either way.

**Out of scope**: PDF page rasterization. That needs pdfium/poppler and is its
own plan with its own dependency decision.

## Git workflow

```bash
git switch -c docs/kb-ocr-state-the-gap
```

## Steps (Option A)

### Step 1: Fix the two error messages

Say what IS available:

```rust
            return Err(KbError::Other(
                "The OCR pipeline is not implemented for PDFs. Set \
                 use_ocr_pipeline=false and use KB_EXTRACT_PRIMARY with a vision \
                 model (e.g. `openai/gpt-4.1-nano`), which reads scanned pages \
                 through the vision extractor. Local OCR is available for single \
                 images with --features kb-ocr."
                    .into(),
            ));
```

Update the matching `kb-ocr` branch too.

**Verify**: `file_test.rs:354,383` assert on message content — update them to
match, keeping the assertions strict.

### Step 2: Document it

`docs/reference/kb.md` — an OCR section covering: what handles scanned PDFs
today (vision LLM), what `kb-ocr` adds (single images, Ollama), its env vars,
and what is not implemented (PDF rasterization), with a link to
`docs/kb/ocr-design.md`.

### Step 3: Note the unused hint

`DocumentTypeHint` is threaded through `ProcessingOptions` and read by nothing.
Either say so in its doc comment or remove it. Do not leave it looking wired.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb file_test
cargo test --features kb,kb-ocr --test kb extract_test
markdownlint docs/reference/kb.md
```

## Done criteria

- The error names a working alternative.
- The docs state what OCR does and does not cover.
- Nothing implies a capability that is not there.

## STOP conditions

- Option B is chosen and `build_extractor`'s catch-all (`extract/mod.rs:116`,
  which treats any unknown sentinel as an OpenRouter model id) swallows the new
  `"ocr"` sentinel — order the match arms so the sentinel wins, and add a test.
