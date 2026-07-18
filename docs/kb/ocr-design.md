# KB OCR Ingestion — Design Note (Spike, 2026-07-18)

> **Status**: SPIKE / proposal, not a runtime-contract doc. Answers the five
> questions from `plans/018-kb-ocr-pipeline-spike.md` Step 1 and scopes a
> follow-up production plan. Do not treat this as a shipped feature guide —
> `kb-ocr` is a non-default, gated prototype (see [Prototype scope](#prototype-scope-what-actually-shipped)).

## Why this exists

`src/kb/file/mod.rs` (`process_pdf`) and `src/kb/file/image.rs`
(`process_image`) both carry a `TODO(kb-ocr)`: when a caller sets
`ProcessingOptions.use_ocr_pipeline = true`, the Rust KB port fails fast with
a typed error instead of running OCR, because the Node/TS origin's
Ollama-backed OCR pipeline (`src/lib/ocr`) was never ported. That TS source
does not exist in this repository (confirmed: `ls src/lib/` — no such
directory) — the TODO points at *behavior* to reproduce, not a file to port
verbatim, so this note derives the design from the comments/call sites in
the Rust code (`src/kb/file/mod.rs:266-278`, `src/kb/file/image.rs:63-78`)
rather than from TS source we don't have.

This spike answers: what should the OCR path *be*, and prototypes the
smallest slice that proves it end-to-end, before anyone commits to a full
production build-out.

## 1. Pre-router or alternative extractor?

**Pre-router.** The existing doc comment on `process_pdf` is explicit:

> "the TS source optionally **pre-routes** through an Ollama OCR pipeline
> when `use_ocr_pipeline=true`"

`use_ocr_pipeline` is a boolean on `ProcessingOptions`, orthogonal to
`cfg.extract_primary` / `cfg.extract_fallback` (the sentinel strings
`build_extractor` dispatches on — `"unpdf" | "mineru" | "hybrid" | "smart" |
<model-id>`). It is not selected by those sentinels and does not interact
with `SmartRouterExtractor`'s own internal OCR-style fallback (which routes
to whatever `extract_smart_fallback` names, typically a vision-LLM model —
see `src/kb/extract/smart_router.rs` and `text_layer_signals.rs`). Today's
code structure already encodes this as a pre-router: `process_pdf` checks
`opts.use_ocr_pipeline` **before** touching `build_extractor` at all, and
only falls into the primary/fallback chain when the flag is unset. This
spike keeps that shape — OCR is a short-circuit at the top of
`process_pdf`/`process_image`, not a new `build_extractor` sentinel. Two
independent knobs (`extract_primary`/`extract_fallback` sentinels for
text-layer-vs-structural routing, and `use_ocr_pipeline` as an orthogonal
"always OCR this document" override) stay independent, matching what the
call sites already imply and avoiding a broader refactor of
`build_extractor`'s dispatch (out of scope per the plan's STOP conditions).

## 2. Backend choice: Ollama HTTP vs. Rust-native OCR

Two real options were compared:

### Option A — Ollama HTTP (chosen for the prototype)

A local/self-hosted [Ollama](https://ollama.com) server exposes a vision
model (e.g. `llava`, `moondream`, `llama3.2-vision`) over HTTP. The
extractor base64-encodes the image and POSTs it to `/api/generate` with an
`images: [...]` field; Ollama returns `{"response": "<text>"}`.

- **Dependency cost**: **zero new Cargo dependencies.** The extractor reuses
  `reqwest`, `serde`, `serde_json`, and `base64` — all already unconditional
  dependencies of this crate (used today by `kb::extract::mineru` and
  `kb::file::image` for the OpenRouter vision path). This is the same
  "HTTP sidecar" shape as `MineruExtractor`, just pointed at a different
  local service.
- **Binary-size cost**: effectively none — no new code paths compiled into
  binaries that don't opt into `kb-ocr`, and the code that *does* compile
  under `kb-ocr` is a ~150-line HTTP client, comparable to `mineru.rs`.
- **System-lib requirement**: **none at build time.** At *runtime*, the
  operator must have Ollama installed and running locally (or reachable at
  `KB_EXTRACT_OCR_BASE_URL`) with a vision-capable model already pulled
  (`ollama pull llava`). This prototype does **not** bundle, download, or
  auto-pull any model — that would violate CLAUDE.md §10 ("do not add heavy
  dependencies for minor convenience") and the plan's explicit
  out-of-scope item ("Bundling/downloading Ollama models or making OCR a
  default").
- **Latency**: one HTTP round-trip per image to a model the operator
  chose and sized for their hardware (from a tiny CPU-only model to a large
  GPU-backed one) — cost is entirely the operator's choice, not baked into
  the binary.
- **Fidelity to origin**: matches the TS source's own description
  ("Ollama models") most directly.

### Option B — Rust-native OCR (tesseract/leptonica bindings)

Crates like `leptess` or `rusty-tesseract` bind to `libtesseract` +
`libleptonica` via FFI (`tesseract-sys`, `leptonica-sys`).

- **Dependency cost**: a new `-sys` crate pair, pulling `cc`/`bindgen`-style
  build-time codegen against system headers.
- **System-lib requirement**: **hard build-time requirement.** The build
  host needs `libtesseract-dev` + `libleptonica-dev` (Debian/Ubuntu package
  names) installed, or the build fails outright — this is not optional at
  compile time the way an HTTP client is. That breaks reproducible builds
  on any CI runner / cross-compilation target (the project explicitly
  supports Raspberry Pi builds — `Cargo.toml`'s release profile comments
  call out low-RAM ARM targets) that doesn't already carry those libs, and
  widens the "things a contributor must apt-get before `cargo build
  --features kb-ocr` works" surface non-trivially.
- **Binary-size / runtime cost**: no network dependency, works fully
  offline, and is typically faster for printed-text OCR than a vision LLM
  round-trip — a real advantage for a production path.
- **Fidelity to origin**: does not match the TS source's Ollama-based
  design; would be a deliberate divergence.

### Decision

The prototype uses **Option A (Ollama HTTP)** because it (a) needs no new
Cargo dependency and adds zero binary-size cost to the `kb` build (only
`kb-ocr` builds see the new module, and even then it's ~150 lines of code,
no new crates), (b) has no system-lib build requirement, which matters for
this project's CI/cross-compile matrix, and (c) matches the original
design's intent most directly. **Option B remains a legitimate production
candidate** — it should be listed as an alternative in any follow-up
production plan, with its system-lib requirement (`libtesseract-dev`,
`libleptonica-dev`) called out explicitly in that plan's Cargo feature
description, exactly as this note does, rather than assumed to be present
in build/CI environments.

## 3. Does this slot behind the existing `Extractor` trait?

**Yes, without modification.** `Extractor` (`src/kb/extract/mod.rs`) is:

```rust
pub trait Extractor: Send + Sync {
    fn name(&self) -> &str;
    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult>;
}
```

`OllamaOcrExtractor` (`src/kb/extract/ocr_ollama.rs`, gated behind
`kb-ocr`) implements this trait directly, mirroring `MineruExtractor`'s
shape (thin HTTP client, `new(base_url, ...)` + `with_client` for test
injection, maps a JSON response into `ExtractionResult`). One naming note:
the trait's parameter is `pdf_bytes: &[u8]` and its module doc says "Run
extraction against an in-memory PDF" — `OllamaOcrExtractor::extract`
instead expects **image bytes** (PNG/JPEG/etc.), since Ollama's `images`
field takes raw image data, not PDF pages. This is a soft type-level
mismatch (the trait takes bytes, not a tagged `Pdf`/`Image` enum), but it is
consistent with how the trait is actually used at the call site — nothing
in `Extractor` enforces "this must be a PDF" beyond the parameter name and
doc comment, and `process_pdf`/`process_image` already dispatch by file
type before calling into the trait. No trait change was needed or made.

The OCR extractor is **not** wired into `build_extractor`'s sentinel
dispatch (`"unpdf" | "mineru" | "hybrid" | "smart" | <model-id>`) — see
§1: `use_ocr_pipeline` is a pre-router, not a sentinel choice, so
`process_image`/`process_pdf` call `OllamaOcrExtractor` directly rather than
through `build_extractor`. This keeps `build_extractor` untouched (no
change to the non-OCR extract path, as the plan requires).

## 4. Feature-flag plan

```toml
# kb-ocr = OCR ingestion prototype (SPIKE, non-default).
kb-ocr = ["kb"]
```

Declared in `Cargo.toml` immediately after `kb-office`, mirroring its shape
(`kb-office = ["kb", "dep:calamine", "dep:docx-rs"]`). Unlike `kb-office`,
`kb-ocr` pulls **no `dep:` entries** — per §2, the prototype needs no new
Cargo dependency, so there is nothing to mark `optional = true`. The flag
still exists and still gates real behavior: it is not a build-time
dependency gate here, it's a **behavior gate** — without it,
`use_ocr_pipeline=true` always returns the typed "not implemented" error
regardless of what's installed at runtime; with it, image ingestion routes
to a live HTTP call. `kb-ocr` is **not** added to the `default` feature list
(`default = ["tui", "whatsapp-web", "remote-install", "kb"]` is unchanged).

If a future production backend *does* need new deps (e.g. Option B's
`leptess`/`tesseract-sys`), they should be added as `optional = true` and
pulled only by `kb-ocr` (or a new, more specific flag if the two backends
need to coexist), exactly as `kb-office` pulls `calamine`/`docx-rs` — never
into the base `kb` feature.

## 5. Open questions / risks (for the production plan, not this spike)

- **PDF-page OCR is not implemented.** The prototype wires OCR for a single
  **image** only (`kb::file::image::process_image`). `process_pdf` still
  returns a typed error under `kb-ocr` (message updated to say so
  explicitly) because turning a PDF page into an image requires a
  rasterization step (e.g. `pdfium-render`, which downloads/links a
  prebuilt `pdfium` binary, or shelling out to `pdftoppm`/Ghostscript,
  which are runtime system-binary dependencies of their own). That's a
  second binary-size/system-dependency decision, deliberately deferred —
  "do not chase every format" per the plan. The production plan must decide
  a rasterization backend and repeat the same system-lib-honesty exercise
  as §2.
- **Model availability is entirely the operator's responsibility.** No
  model is bundled, downloaded, or pinned by version. If Ollama isn't
  running or the named model (default `llava`) isn't pulled, every OCR call
  fails with a `KbError::Extraction` — there is no fallback to the
  vision-LLM path (fallback was explicitly rejected upstream: "fail-fast
  rather than silently fall back to vision-LLM"). A production version
  should surface a clearer "is Ollama reachable" pre-flight check (perhaps
  in `doctor`), not something this spike adds.
- **Offline behavior.** Because Ollama typically runs on `localhost`, this
  backend is "offline" in the sense of not calling an external cloud API —
  but it is not offline in the sense of "zero runtime dependencies"; the
  agent process still makes an HTTP call to a separate service that must be
  up. Document this distinction for operators who assume `kb-ocr` means
  "fully local, no moving parts."
- **Per-page / per-call token-or-time budget.** `VisionLlmExtractor` splits
  large PDFs into page segments with a token budget
  (`with_options(..., segment_pages, ...)`, see `vision_llm.rs`). The OCR
  prototype has no equivalent because it only ever processes one image; a
  production PDF-OCR path will need the same segmentation/budget thinking
  `vision_llm.rs` already has, likely reusing `pdf_splitter.rs`.
- **Image formats.** The prototype base64-encodes whatever bytes it's
  given and trusts Ollama to sniff the format; it does not validate
  extension/mime the way `image.rs::mime_for_path` does for the OpenRouter
  path (Ollama's `/api/generate` doesn't take a mime field). Malformed
  input just surfaces as an Ollama-side error, which is acceptable for a
  spike but should be tightened (explicit extension allowlist, matching
  `IMAGE_EXTENSIONS` in `kb::file::mod`) before production.
- **Config location.** `OllamaOcrExtractor::from_env()` reads
  `KB_EXTRACT_OCR_BASE_URL` / `KB_EXTRACT_OCR_MODEL` directly, bypassing
  `KbConfig` (`src/kb/config.rs`), to keep this spike's diff inside the
  files the plan declared in scope. A production port should promote these
  to real `KbConfig` fields (`extract_ocr_base_url`, `extract_ocr_model`),
  consistent with `extract_vision_base_url` / `extract_mineru_base_url`,
  and thread them through `KbConfig::from_env()` / `from_env_with_keys()`
  like every other extractor endpoint. That's a config-schema change and
  should go through the normal "config keys are public contract" review
  (CLAUDE.md §6.4), not be improvised here.
- **Auth.** Ollama's local HTTP API is normally unauthenticated. No API-key
  handling was added. If a production deployment fronts Ollama behind auth
  (e.g. a reverse proxy), that's a config-schema addition for the follow-up
  plan, not this spike.

## Effort estimate (sharpened)

Original plan estimate: M-L, coarse. Post-spike:

- **Image-only OCR, Ollama backend, as prototyped here**: effort was **S**
  in practice — no new dependencies, ~150 lines for the extractor, ~30
  lines of wiring across two call sites, and a wiremock-based test suite
  that mirrors an existing pattern (`mineru_extractor_*` tests) almost
  line for line.
- **Full production version** (PDF-page rasterization + segmentation +
  `KbConfig` promotion + doctor pre-flight + format validation + decide on
  Option A vs. B for real, possibly both behind separate flags): still
  **M**, driven almost entirely by the PDF-rasterization decision (§5,
  first bullet) — that's the one piece with real new-dependency /
  system-lib tradeoffs left to resolve, not by the OCR call itself.

## Prototype scope (what actually shipped)

In scope (this spike, behind `--features kb-ocr`, non-default):

- `src/kb/extract/ocr_ollama.rs` — `OllamaOcrExtractor`, implements
  `Extractor`, POSTs to a configurable Ollama endpoint.
- `src/kb/file/image.rs` — `process_image` routes
  `use_ocr_pipeline=true` through `OllamaOcrExtractor` when `kb-ocr` is on;
  unchanged (still the typed error) when it's off.
- `Cargo.toml` — `kb-ocr = ["kb"]`, no new deps, not in `default`.
- Tests: `tests/kb/extract_test.rs` (`OllamaOcrExtractor` against a
  `wiremock` mock — success + 5xx-surfaces-as-`KbError::Extraction`),
  `tests/kb/file_test.rs` (the pre-existing
  `process_pdf_with_ocr_pipeline_flag_returns_not_implemented_error` test
  split into an unguarded non-`kb-ocr` variant and a `kb-ocr` variant
  asserting the updated PDF message).

Explicitly NOT in scope (see plan §"Out of scope" — none of this shipped):

- PDF ingestion through OCR (`process_pdf` still errors under `kb-ocr`).
- Any change to the non-OCR extract path (`build_extractor`,
  `extract_with_fallback`, `SmartRouterExtractor`, `HybridExtractor`,
  `VisionLlmExtractor`, `MineruExtractor`) — all untouched.
- Bundling, downloading, or auto-pulling any Ollama model.
- Making `kb-ocr` part of `default`, or making `--features kb` (without
  `kb-ocr`) pull any OCR-related dependency (it pulls none — see §4).
- A `KbConfig`/`from_env()` change (deferred — see §5 "Config location").
- Rust-native (tesseract/leptonica) backend — discussed in §2 as the
  documented alternative, not implemented.
