# Plan 016: Resolve the past-sunset `legacy-providers` drift (reroute ungated call sites, then gate/remove)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/providers/ src/onboard/wizard.rs Cargo.toml`
> If any changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.
>
> **This plan changes live provider routing (MED risk). Do the safe reversible
> part (Stage 1). STOP for maintainer approval before deleting anything (Stage 2).**
>
> **REVISED after cold review**: (1) rig-core 0.37 DOES support a custom base URL
> — the earlier "rig can't do custom base URL" premise was wrong; the real fix is
> ~3 lines in `rig_native.rs`, now IN SCOPE. (2) The proposed provider-type
> assertions can't be written (the `Provider` trait has no name/base_url getter
> and `Box<dyn Provider>` isn't downcastable) — corrected test approach below.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: migration
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

`Cargo.toml` documents `legacy-providers` as "Will be removed in v0.7.0 once rig
has soaked." The version is `0.7.15-alpha` — that milestone passed. Yet the
hand-rolled modules `anthropic.rs` (1318), `openai.rs` (677), `gemini.rs` (880)
are declared `pub mod` **unconditionally** and three **ungated** call sites depend
on them, so they compile into default builds and can't be gated/removed as
documented. This is doc/code drift plus two coexisting OpenAI-compatible provider
impls. Goal: make the code match the documented intent (or consciously re-decide
the sunset), shedding legacy code from default builds.

## Current state (verified at 4d35107 — no drift)

- `Cargo.toml:7` = `0.7.15-alpha`; `:242` `default = [...]`; `:243-247` comment;
  `:248` `legacy-providers = []`.
- `src/providers/mod.rs:19/23/25` — unconditional `pub mod anthropic; pub mod gemini; pub mod openai;`.
  The default-provider factory arms for `"anthropic"`/`"openai"`/`"gemini"`
  (`:993/999/1012`) ARE `#[cfg(feature = "legacy-providers")]`-gated (pairs at
  `:988-1013`); the non-legacy default routes through `RigProvider`.
  `openai_codex` (`:26`) is INDEPENDENT of the `openai` module (only references
  the `"openai-codex"` auth-profile string) — do NOT gate it.
- **Ungated call sites keeping the legacy modules reachable**:
  - `src/providers/mod.rs:1173` — `"ovhcloud" | "ovh"` → `openai::OpenAiProvider::with_base_url(<hardcoded ovh url>, key)`
    (base URL is the literal `"https://oai.endpoints.kepler.ai.cloud.ovh.net/v1"` at `:1174`, not `api_url`).
  - `src/providers/mod.rs:1202` — `"anthropic-custom:"` → `anthropic::AnthropicProvider::with_base_url(key, Some(&base_url))`.
  - `src/onboard/wizard.rs:2372` — `providers::gemini::GeminiProvider::has_cli_credentials()`.
- **Modern targets**:
  - `OpenAiCompatibleProvider::new(name: &str, base_url: &str, credential: Option<&str>, auth_style: AuthStyle)`
    (`src/providers/compatible.rs:47-52`); astrai uses it at `mod.rs:1168` with
    `AuthStyle::Bearer`. `key` is already `Option<&str>`.
  - `RigProvider::for_provider_with_url("anthropic", key, Some(&base_url))` is the
    modern Anthropic path. **rig-core 0.37 supports a custom base URL** (the
    builder has `pub fn base_url(...)`), BUT `src/providers/rig_native.rs:124-131`
    (the `"anthropic"` branch of `for_provider_with_url`) currently NEVER calls
    `.base_url(url)`, and its doc comment at `rig_native.rs:116-117` ("Ignored for
    Anthropic + Gemini — rig's clients don't support arbitrary base URLs there")
    is STALE. Wiring `.base_url(url)` there is ~3 lines.
  - Modern Gemini has NO CLI-credential check; `has_cli_credentials` exists only
    at `src/providers/gemini.rs:240` → `try_load_gemini_cli_token` (`:208`) →
    `gemini_cli_dir` (`:235`) + `GeminiCliOAuthCreds` serde struct (`~:156`) +
    chrono RFC3339 expiry — ~40 self-contained lines (no `&self`).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (default) | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Lint (legacy on) | `cargo clippy --features legacy-providers --all-targets -- -D warnings` | exit 0 |
| Provider tests | `cargo test providers` | all pass |
| Default build excludes legacy | `cargo build 2>&1 | tail -5` | compiles |

## Scope

**In scope (Stage 1)**:
- `src/providers/rig_native.rs` — wire `.base_url(url)` into the `"anthropic"`
  branch of `for_provider_with_url` (`:124-131`) and fix the stale comment (`:116-117`).
- `src/providers/mod.rs` — reroute `ovhcloud` (`:1173`) and `anthropic-custom`
  (`:1202`) onto the modern providers; gate the three module declarations.
- `src/onboard/wizard.rs` (`:2372`) — call an extracted/relocated Gemini
  CLI-credential check instead of the legacy struct.
- `Cargo.toml` — the `legacy-providers` comment / sunset note.
- Tests in `src/providers/`.

**Out of scope (Stage 2 — needs explicit approval)**:
- Deleting `anthropic.rs`/`openai.rs`/`gemini.rs` and removing the feature.

## Git workflow

- Branch: `advisor/016-legacy-providers-sunset`
- Commit per stage. Messages e.g.
  `refactor(providers): reroute ovhcloud/anthropic-custom to modern path; gate legacy modules`.
- Do NOT push or open a PR unless instructed. Open for review; do not self-merge.

## Steps

### Step 1: Reroute ovhcloud onto `OpenAiCompatibleProvider`

At `mod.rs:1173`, replace `openai::OpenAiProvider::with_base_url(<ovh url>, key)`
with `OpenAiCompatibleProvider::new("ovhcloud", "https://oai.endpoints.kepler.ai.cloud.ovh.net/v1", key, AuthStyle::Bearer)`
(mirror the astrai arm at `:1168`; keep the hardcoded base URL; `key` is already
`Option<&str>`).

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 2: Wire rig anthropic base_url, then reroute anthropic-custom

- In `src/providers/rig_native.rs`, in the `"anthropic"` branch of
  `for_provider_with_url` (`:124-131`), call `.base_url(url)` on the Anthropic
  client builder when a custom `url` is provided (read the branch; the builder is
  a type alias over rig's generic builder which has `base_url`). Update the stale
  comment at `:116-117`.
- At `mod.rs:1202`, replace `anthropic::AnthropicProvider::with_base_url(key, Some(&base_url))`
  with `RigProvider::for_provider_with_url("anthropic", key, Some(&base_url))`
  (match the real signature — read `for_provider_with_url`).

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 3: Extract the Gemini CLI-credential check off the legacy struct

Move `has_cli_credentials` + its helpers (`try_load_gemini_cli_token`,
`gemini_cli_dir`, `GeminiCliOAuthCreds`, the expiry check — ~40 lines, all
`&self`-free) into a free function reachable without the legacy `GeminiProvider`
struct (e.g. a `pub fn gemini_cli_has_credentials() -> bool` in a small
`src/providers/gemini_cli.rs`, or a free fn in the providers module root). Update
`wizard.rs:2372` to call it. Deps (`directories`/`serde`/`chrono`/`std::fs`) are
all crate-wide.

**Verify**: `cargo build 2>&1 | tail -5` → compiles;
`grep -n "gemini::GeminiProvider" src/onboard/wizard.rs` → no matches.

### Step 4: Gate the three legacy module declarations

Once no ungated caller references them, gate `mod.rs:19/23/25`:
```rust
#[cfg(feature = "legacy-providers")] pub mod anthropic;
#[cfg(feature = "legacy-providers")] pub mod gemini;
#[cfg(feature = "legacy-providers")] pub mod openai;
```
Do NOT gate `openai_codex` (`:26`).

**Verify**:
- `cargo build 2>&1 | tail -5` (default) → compiles with modules gated out.
- `cargo build --features legacy-providers 2>&1 | tail -5` → compiles.
- `cargo clippy --all-targets -- -D warnings` AND
  `cargo clippy --features legacy-providers --all-targets -- -D warnings` → both 0.
- `grep -rn "providers::anthropic\|providers::openai\|providers::gemini" src/` →
  references only inside `#[cfg(feature = "legacy-providers")]` code (the default
  factory arms). Note: `create_provider("anthropic"/"openai"/"gemini", …)` factory
  tests at `mod.rs:1981/1986/2005/2011` route through the factory string, not the
  module path — they stay fine in default builds.

### Step 5 (STOP for approval): Delete legacy modules + feature

ONLY after maintainer approval: delete the three modules, remove the
`legacy-providers` feature and the now-dead gated factory arms. Re-run the full
matrix. If NOT approved, update the `Cargo.toml` comment to reflect the re-decided
sunset (e.g. "retained behind `legacy-providers`; default builds use rig") so the
doc stops claiming a removed-by-v0.7.0 that didn't happen.

## Test plan

- Existing provider tests guard routing (run `cargo test providers`, default and
  `--features legacy-providers`). The `anthropic-custom` factory tests at
  `mod.rs:2276-2321` currently assert `create_provider(...).is_ok()` — a naive
  reroute still returns `Ok` while silently dropping the base URL, so `is_ok()`
  is NOT sufficient to prove the reroute threads the custom URL.
- Add a FOCUSED test at the point where base_url is observable — in
  `rig_native.rs`: a unit test that `for_provider_with_url("anthropic", Some("k"), Some("https://example.test/v1"))`
  produces a client whose configured base URL is the custom one. If rig's client
  exposes no base-URL getter, add a minimal test-only accessor OR assert via a
  `wiremock` server: point the custom base URL at a mock, issue a request through
  the rerouted provider, and assert the mock received it (proves the URL was
  honored). Prefer the `wiremock` behavior test — it needs no getter.
- `ovhcloud`: assert `create_provider("ovhcloud", Some("k")).is_ok()` still holds;
  if feasible, a `wiremock`-backed test that the ovh base URL is used.

**Verify**: `cargo test providers` (default and with the feature) → all pass.

## Done criteria (Stage 1)

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0 (default) AND with `--features legacy-providers`
- [ ] `grep -n "openai::OpenAiProvider\|anthropic::AnthropicProvider\|gemini::GeminiProvider" src/providers/mod.rs src/onboard/wizard.rs`
      → references only inside `#[cfg(feature = "legacy-providers")]` code
- [ ] Default `cargo build` compiles with `anthropic`/`openai`/`gemini` modules gated out
- [ ] The anthropic-custom reroute demonstrably threads the custom base URL (wiremock or accessor test), not just `is_ok()`
- [ ] `cargo test providers` passes (default and with the feature)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- Wiring `.base_url(url)` into rig's anthropic branch doesn't compile against the
  pinned rig-core 0.37 (the builder method is named/shaped differently than
  expected) — report the actual rig API; do NOT bump rig-core here.
- Extracting the Gemini CLI check pulls in `&self`-bound state you didn't expect
  (it should be self-free) — report before refactoring broadly.
- Gating the modules breaks a caller `grep -rn "providers::anthropic\|providers::openai\|providers::gemini" src/`
  didn't surface — enumerate and report.
- You reach Step 5 — always stop for explicit maintainer approval before deleting.

## Maintenance notes

- Line-shed reality: with all three modules gated, default builds shed ~2875
  lines. If for any reason `anthropic` can't be gated (e.g. the rig base_url wiring
  is blocked), you still shed `openai.rs` + `gemini.rs` ≈ 1557 lines — a partial
  Stage 1; report which modules gated.
- Reviewer should scrutinize request/response parity: ovhcloud/anthropic-custom
  must behave the same through the modern path (headers, auth, base URL, streaming)
  as through the legacy structs — the legacy `OpenAiProvider::with_base_url` may
  differ from `OpenAiCompatibleProvider` on the `/v1/responses` fallback/auth
  header (note in the PR).
- Interacts with plan 012 (shared HTTP client) — note ordering in the PR if both
  are in flight.
