# Plan 025: Restore RigProvider auth/feature parity (Option X — route special auth to legacy)

> **Context**: The default (non-`legacy-providers`) provider path routes
> `anthropic`/`openai`/`gemini` through `RigProvider` (rig-core). That migration
> silently DROPPED several auth modes + features the legacy hand-rolled providers
> had. This plan restores parity. Decision (approved by maintainer): **Option X**
> — for auth modes rig-core cannot serve (Gemini CLI OAuth / cloudcode-pa,
> Anthropic setup-token), ROUTE to the legacy provider (keep it compiled), rather
> than reimplement in rig. Consequence: **legacy providers become permanent; 016
> Stage-2 (delete the files) is CANCELLED.**
>
> **Branch**: fold onto `advisor/016-legacy-providers-sunset` (same files 016
> already touches). One commit per gap. Do NOT delete any legacy file.
>
> **Verification is central** (disk-constrained; see the disk memory): touch +
> `CARGO_TARGET_DIR=<shared> cargo test --lib <filter>` + `--features
> legacy-providers` compile + fmt + clippy-delta. Every gap ships with a repro
> test that FAILS before the fix and passes after.

## Baseline evidence (all confirmed on main, default build)
- resolve_provider_credential (src/providers/mod.rs:838-874) has **no `gemini` arm** → `GEMINI_API_KEY`/`GOOGLE_API_KEY` ignored (empirically reproduced: `create_provider("gemini", None)` errs with the env var set).
- rig-core gemini client hardcodes `generativelanguage.googleapis.com` (public) → Gemini CLI OAuth tokens (scoped for `cloudcode-pa`) get 400.
- resolve_provider_credential reads `ANTHROPIC_OAUTH_TOKEN` (setup-token) but RigProvider sends it as `x-api-key`; setup-tokens need `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`.
- rig_native has zero cache_control/oat01/anthropic-beta/cloudcode handling (grep = 0).
- `gemini_cli.rs` (ungated) exposes `try_load_gemini_cli_token()` + `gemini_cli_has_credentials()`.
- Legacy `AnthropicProvider`/`GeminiProvider` impl `chat`/`chat_with_history` but NOT `chat_stream` → routing to them loses streaming for those auth modes (acceptable, documented).

## Scope
**In scope**: `src/providers/mod.rs` (resolver + factory arms), `src/providers/rig_native.rs` (caching + reasoning), un-gate `pub mod anthropic;`/`pub mod gemini;`, and the two legacy files ONLY if a tiny helper needs exposing (prefer not). Tests in the same files + `tests/provider_resolution.rs` if present.
**Out of scope**: deleting any legacy file (016 Stage-2 — CANCELLED). Rewriting rig-core. Touching `compatible.rs` providers (unaffected).

## Steps (ordered by severity; hard auth-breaks first)

### Step 1 — Gemini env-key resolution (#1, HARD break, easy)
In `resolve_provider_credential` (mod.rs:838), add before the `_ =>` arm:
```rust
"gemini" | "google" | "google-gemini" => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
```
This makes `create_provider("gemini")` resolve an env API key → RigProvider gemini (keeps native tools + streaming).
**Repro test** (flip the existing `repro`): with `GEMINI_API_KEY` set, `resolve_provider_credential("gemini", None)` == `Some(..)` and `create_provider("gemini", None).is_ok()`. Must fail before, pass after. Use `crate::test_env::ENV_LOCK`.

### Step 2 — Un-gate legacy anthropic + gemini modules (prereq for Steps 3-4)
In mod.rs, remove the `#[cfg(feature = "legacy-providers")]` on `pub mod anthropic;` and `pub mod gemini;` (they must compile in the default build to serve as the special-auth backend). Leave `pub mod openai;` gated (Step 5 handles openai in rig; openai has no special-auth mode). Update the module-level comment to state legacy anthropic/gemini are the OAuth/setup-token backends, not dead code.
**Verify**: default `cargo build --lib` compiles; `--features legacy-providers` still compiles (no double-definition).

### Step 3 — Anthropic setup-token → legacy (#3, HARD break)
In the default (`not(legacy-providers)`) `"anthropic"` arm (mod.rs:993-996), branch on the resolved credential:
```rust
"anthropic" => {
    // Setup-tokens (sk-ant-oat01-...) need Bearer + anthropic-beta, which the
    // rig client can't send; route them to the legacy provider (which also
    // gives prompt caching). API keys keep the rig path (streaming + native).
    if key.map(|k| k.trim().starts_with("sk-ant-oat01-")).unwrap_or(false) {
        Ok(Box::new(anthropic::AnthropicProvider::new(key)))
    } else {
        Ok(Box::new(rig_native::RigProvider::for_provider("anthropic", key)?))
    }
}
```
**Repro test**: `create_provider("anthropic", Some("sk-ant-oat01-xyz"))` returns a provider whose behavior matches legacy (assert via a downcast-free behavior test if possible, or at minimum that construction succeeds and — ideally — a wiremock capturing `Authorization: Bearer` + `anthropic-beta`, not `x-api-key`). If a wiremock header test is impractical, assert the routing decision via a small extracted helper `fn is_anthropic_setup_token(key) -> bool` with unit tests, and a comment that the header behavior is covered by the legacy provider's own tests.

### Step 4 — Gemini CLI OAuth → legacy (#2, HARD break)
In the default `"gemini"` arm (mod.rs:1011-1014):
```rust
"gemini" | "google" | "google-gemini" => {
    // rig's gemini client only speaks the public endpoint; CLI OAuth tokens are
    // scoped for cloudcode-pa. When there's no API key but Gemini CLI creds
    // exist, route to the legacy provider (handles cloudcode-pa). Otherwise rig.
    if key.is_none() && gemini_cli::gemini_cli_has_credentials() {
        Ok(Box::new(gemini::GeminiProvider::new(key)))
    } else {
        Ok(Box::new(rig_native::RigProvider::for_provider("gemini", key)?))
    }
}
```
Update the existing `factory_gemini` test: `create_provider("gemini", None)` is now `is_ok()` **iff** CLI creds exist; keep an env-isolated variant. Add a repro/unit test for the routing predicate (mock the has-creds branch if feasible; else test the resolver + document the CLI-creds branch is environment-dependent).

### Step 5 — OpenAI reasoning_content (#5, verify-first)
Investigate whether rig-core maps the `reasoning_content` JSON field to `AssistantContent::Reasoning` (rig openai completion/mod.rs references reasoning). `rig_native::flatten_assistant` already flattens Reasoning → text.
- If rig captures it → add a test proving a reasoning-only response surfaces as text through RigProvider; **no code change** (gap already closed by flatten).
- If rig drops it → this is a rig-core limitation. Document it in the commit + a `// KNOWN LIMITATION` note; do NOT route `openai` to legacy (that would lose streaming for the common case). Optionally file a follow-up. Do not block the auth fixes on this.

### Step 6 — Anthropic prompt caching for the rig path (#4, cost, medium)
Setup-token users (Step 3, legacy) already get caching. For API-key users on the rig path, add cache_control breakpoints so long system prompts / conversations aren't re-billed. rig-core anthropic supports cache_control (completion.rs). Set it via rig's request `additional_params` (or the documented rig caching hook) mirroring the legacy heuristic (system >3KB; conversation >4 msgs; last tool def).
- If rig's caching API is clean → implement + a serialization test asserting cache_control is present for a large system prompt.
- If rig's API makes this awkward/unsafe → document as a deferred cost-optimization (NOT a correctness break) with a `// TODO` and the heuristic, and note setup-token users already cache via legacy. Do not block on it.

### Step 7 — Diagnostics parity (#6, minor)
Ensure `doctor`/setup report Gemini env/OAuth + Anthropic setup-token as configured. Since `has_usable_credential` uses `resolve_provider_credential`, Step 1 fixes gemini env. For gemini CLI OAuth + anthropic setup-token, make `has_usable_credential("gemini"/"anthropic")` also return true when CLI creds / setup-token are present (add the CLI-creds check to the gemini path). Add a unit test.

## Done criteria (all must hold)
- [ ] Step 1 repro: `create_provider("gemini", None)` ok with `GEMINI_API_KEY` set (env-isolated test) — passes.
- [ ] Step 3 repro: setup-token routes to legacy (predicate unit-tested; construction ok).
- [ ] Step 4: gemini CLI-OAuth routing predicate tested; `factory_gemini` updated.
- [ ] Step 5: reasoning behavior tested OR documented as rig limitation.
- [ ] Step 6: caching implemented+tested OR explicitly deferred with rationale.
- [ ] Step 7: `has_usable_credential` returns true for all three restored auth modes; unit-tested.
- [ ] `cargo test --lib providers::` green (default features).
- [ ] `cargo build --lib` (default) AND `cargo build --lib --features legacy-providers` both compile.
- [ ] `cargo fmt --all -- --check` clean; clippy-delta 0 on changed files.
- [ ] No legacy file deleted. README/016 status note updated to say Stage-2 is cancelled and why.

## STOP conditions
- If un-gating anthropic/gemini causes a double-definition or symbol clash that isn't a trivial cfg fix — stop and report.
- If Step 3/4 routing changes behavior for the COMMON case (api-key anthropic / api-key gemini) — stop; those must stay on the rig path (streaming).
- If caching (Step 6) can't be done without risking wrong cache breakpoints (which corrupt responses) — defer, don't guess.

## Rollback
Each step is its own commit on `advisor/016`. Revert per-commit. Un-gating (Step 2) is the only structural change; reverting it restores the gated state.

---

## EXECUTION RESULT (DONE, 2026-07-18)

Executed on `advisor/016-legacy-providers-sunset` (folded in, per the same-file
convention). Commits: `7eb70c8` (auth routing + resolver + un-gate + diagnostics,
mod.rs) + `884235a` (Anthropic prompt caching, rig_native.rs), on top of the
016 base (`f2cee10` reroute/gate, `c2b08d5` max_tokens fix).

- #1 Gemini env-key: DONE (resolver arm) — `factory_gemini_resolves_env_api_key` proves it.
- #2 Gemini CLI OAuth: DONE (route to legacy when no key + CLI creds; un-gated gemini).
- #3 Anthropic setup-token: DONE (route to legacy; `is_anthropic_setup_token` + test).
- #4 Anthropic caching: DONE (`with_prompt_caching()` on both rig anthropic arms).
- #5 OpenAI reasoning_content: NO CHANGE NEEDED — rig-core parses `reasoning_content`
  (openai completion/mod.rs:164→AssistantContent::Reasoning) and rig_native
  `flatten_assistant` surfaces it as text. Not a gap.
- #6→#7 Diagnostics: DONE (`has_usable_credential` reports Gemini CLI OAuth).
- Stage-2 deletion: CANCELLED (legacy is the routed backend).

Verified: 440 `providers::` tests pass (default), `cargo build --features
legacy-providers` compiles, fmt clean, clippy-delta 0 (un-gate restores the
4d35107 default-clippy scope, so no new warnings). UNMERGED.

Known tradeoff (documented, acceptable): setup-token / CLI-OAuth requests use the
legacy providers, which lack `chat_stream` → no streaming for those auth modes
(they were 100% broken before). API-key path unchanged (streaming + native tools).
