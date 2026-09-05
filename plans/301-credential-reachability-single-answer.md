# Plan 301: One honest answer to "can this provider actually send?", used by every surface

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat a7fbaca..HEAD -- src/providers/mod.rs src/doctor/checks/provider.rs src/doctor/checks/config.rs src/gateway/config_api.rs src/main.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW–MED (changes what three surfaces report, not what the agent does)
- **Category**: bug / operator honesty
- **Planned at**: commit `a7fbaca`, 2026-09-05
- **Origin**: found while executing plan 298. The headless gate needed the question
  "is a credential reachable at send time?" and discovered the function that answers it is
  blind to at least one auth mode, so the gate had to carve that mode out by hand.

## Why this matters

Three surfaces ask the same question and give three different answers.

`has_usable_credential` is the intended authority. It knows about per-provider env vars, the
generic fallback, MiniMax OAuth and the Gemini CLI — but not about auth modes that resolve
credentials inside the provider itself. Bedrock is the confirmed case: it resolves AWS access
keys, profiles or instance roles inside `BedrockProvider`, so the resolver returns `None` and
a perfectly working install reads as unconfigured.

`doctor` does not use that function at all. It still asks `resolve_key_for_provider`, which
only ever looks at config, so an operator authenticated by environment or OAuth is told the
agent cannot send — while the agent sends fine. That was recorded during the audit and never
fixed.

The cost compounds: plan 298 had to hard-code a Bedrock exclusion to avoid blocking a valid
install, and `config_api` shows a "no API key" warning for the same false negative. Every new
consumer either repeats the carve-out or inherits the bug.

## Current state (verified at `a7fbaca`)

```rust
// src/providers/mod.rs:855-865 — one special case, no general notion of non-key auth
pub fn has_usable_credential(name: &str, config_key: Option<&str>) -> bool {
    if resolve_provider_credential(name, config_key).is_some() { return true; }
    matches!(name, "gemini" | "google" | "google-gemini") && gemini_cli::gemini_cli_has_credentials()
}
```

```rust
// src/doctor/checks/provider.rs:75 — a different, narrower question
let api_key = ctx.config.resolve_key_for_provider(provider);
```

The workaround this plan removes, and its own note that the gap is real:

```rust
// src/main.rs — inside unusable_provider_after_headless_setup
if matches!(name, "bedrock" | "aws-bedrock") { return None; }
// doc comment: "That gap is real and also affects `doctor` and the config API;
//               fixing it is a separate task."
```

Candidate non-key auth modes to consider while scoping: Bedrock (AWS AKSK / profile / role),
Anthropic setup-tokens, GitHub Copilot and OpenAI Codex OAuth, Qwen OAuth
(`.qwen/oauth_creds.json`), MiniMax OAuth (already handled), Gemini CLI (already handled),
and local providers, which need no credential at all (`provider_is_local`).

## Steps

1. **Enumerate the auth modes from the code, not from this list.** For each provider in the
   factory, determine how `create_provider` actually obtains a credential. The output of this
   step is a table in the PR description: provider → auth mode → is it reachable by
   `resolve_provider_credential`?
   **Verify**: every provider in the factory appears in the table.

2. **Extend `has_usable_credential` to cover the modes that exist**, keeping each branch
   traceable to the code that consumes the credential — the Gemini branch is the pattern:
   a named condition with a comment saying which path uses it. Local providers should answer
   `true` (nothing is missing) or be excluded by the callers; pick one and be consistent.
   **Verify**: no caller needs its own carve-out afterwards.

3. **Point `doctor` at it.** Replace the `resolve_key_for_provider` check in
   `src/doctor/checks/provider.rs` (and the equivalent in `checks/config.rs`) so `doctor`
   and the send path agree. Keep the message specific: "no credential found for X, tried
   config, `<ENV_VAR>`, and `<mode>`" beats "no API key".
   **Verify**: `doctor` and `doctor models` give the same verdict for the same install —
   the audit found they disagreed.

4. **Delete the hand-rolled exclusion in `main.rs`** once step 2 makes it redundant. Leave
   `provider_is_local` if that remains the honest way to express "needs no key".
   **Verify**: `rg -n 'bedrock' src/main.rs` returns nothing in the headless gate.

5. **Tests, one per auth mode.** Each asserts `has_usable_credential` is true when only that
   mode's credential is present. Bedrock is the one that must not regress: with only AWS env
   vars set, the answer is true. Use `ENV_LOCK` for anything env-mutating.
   **Verify**: `cargo test --lib providers`, `cargo test --lib doctor` pass; each test fails
   if its branch is removed.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib providers`, `cargo test --lib doctor`, `cargo test --test setup_e2e` pass.
- A Bedrock-only install reports configured in `doctor`, in the config API, and in headless
  setup — with no per-caller carve-out anywhere.

## STOP conditions

- A provider's credential can only be resolved by attempting a network call → STOP. This
  function must stay offline and cheap; `doctor` calls it on a fast path.
- Step 1's table shows an auth mode nobody can determine statically → STOP and report it;
  documenting an honest "unknown" is better than a guess that flips either way.

## Test plan

One test per auth mode in `providers`, plus one `doctor` test asserting agreement with the
send path. Never put a real credential in a fixture — placeholder shapes only.

## Maintenance note

This function is the single answer to a question three surfaces ask. Any new provider whose
auth is not a plain API key must add its branch here in the same PR — otherwise the next
consumer writes another carve-out, which is how this drifted in the first place.

## Rollback

One commit. It changes reporting only; the send path is untouched, so a revert cannot break
a working install.
