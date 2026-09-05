# Plan 159: TUI setup offers 11 fewer providers than the CLI wizard — single-source the table

> **Executor instructions**: Follow this plan step by step. This is one PR.
> Run every verification command. If anything in "STOP conditions" occurs,
> stop and report. When done, add/update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 8735d9e..HEAD -- src/onboard/wizard.rs src/onboard/provision/provider.rs`
> All line numbers below are from `8735d9e` (post-v0.22.0-alpha; verified
> identical to `d0089a4` for both files). If this diff is non-empty,
> re-verify each cited line before editing.

## Status

- **Priority**: P2 — operators picking a provider in the TUI cannot find 11
  providers the binary fully supports; the workaround (CLI `rantaiclaw setup`)
  is not discoverable from inside the TUI
- **Effort**: S
- **Risk**: LOW (moves a static table; no flow logic changes)
- **Depends on**: none
- **Category**: bugfix
- **Planned at**: commit `8735d9e` (post-v0.22.0-alpha), 2026-08-18. (Numbered
  159: a concurrent session took 157 for `curated-memory-nudge-and-flush`.)

## Why this matters

Reported by an operator: the TUI's `/setup` provider picker does not show all
providers. Verified in source: the provider table exists **twice**, hand-copied,
and the copies have drifted.

- CLI wizard: `src/onboard/wizard.rs:2321` (`setup_provider`) — 33 providers
- TUI provisioner: `src/onboard/provision/provider.rs:102` — 26 providers,
  an older snapshot of the same table

Missing from the TUI (all present in the CLI list AND registered in the
provider factory):

| Tier | Missing ids |
|------|-------------|
| Recommended | `openai-codex` |
| Gateway / proxy | `astrai` |
| Specialized | `kimi-code`, `qwen-code`, `glm-cn`, `minimax-cn`, `qwen-intl`, `qwen-us`, `zai-cn`, `synthetic`, `opencode` |

This is the repo's known "one contract implemented N times" defect shape
(channels deepscan 2026-08-12) — the same shape that produced the provisioning
endpoint drift fixed in v0.16.1-alpha, where a stale hand-copied URL table sent
API keys to unregistered domains. The fix is the same: one table, two readers.

Precedent already in this file pair: the *model* list is shared
(`provider.rs:516` calls `crate::onboard::wizard::curated_models_for_provider`)
— only the *provider* list was left duplicated.

## Current state

**CLI wizard** — `src/onboard/wizard.rs:2306-2397`: `tiers` vec (6 labels with
emoji prefixes) + `providers: Vec<(&str, &str)>` matched on `tier_idx`. Tier 5
(`_ =>`) is the Custom/BYOP sentinel (empty vec → custom flow).

**TUI provisioner** — `src/onboard/provision/provider.rs:69-140`: same
structure, no emoji, stale entries. `providers.is_empty()` at
`provider.rs:142` triggers the custom flow — same sentinel convention.

Post-selection flow in the TUI is **generic and already safe for the new
ids**:

- `needs_key` (`provider.rs:269`) is `!matches!(provider_name, "ollama" | "llamacpp")` — correct for every added id (all are key- or OAuth-based).
- The empty-key branch (`provider.rs:314-374`, shipped in plan 150) gates on
  `create_provider(provider_name, None)` — the factory oracle. OAuth-flavoured
  providers (`openai-codex`, `kimi-code`, `qwen-code`) that can build from
  cached CLI credentials sail through on an empty key; ones that cannot get the
  warn + re-enter/abort choice. No per-provider branches needed.
- Key validation resolves endpoints via
  `crate::doctor::checks::provider::resolve_endpoint` (`provider.rs:398-415`);
  an unknown endpoint saves the key unchecked with a message. No table to
  extend.

## Step 1 — Extract the shared table into `wizard.rs`

In `src/onboard/wizard.rs`, directly above `setup_provider`
(`wizard.rs:2302`), add:

```rust
/// One tier of the provider-setup catalog.
///
/// Shared verbatim by the CLI wizard (`setup_provider`) and the TUI
/// provisioner (`onboard::provision::provider`). These were two
/// hand-maintained copies and drifted 11 providers apart — the same
/// defect shape as the endpoint table replaced in v0.16.1-alpha.
/// `providers` empty marks the Custom/BYOP tier; both drivers already
/// treat an empty list as "run the custom flow".
pub(crate) struct ProviderTier {
    pub label: &'static str,
    /// `(factory_key, human_label)` pairs, in display order.
    pub providers: &'static [(&'static str, &'static str)],
}

pub(crate) const PROVIDER_SETUP_TIERS: &[ProviderTier] = &[
    ProviderTier {
        label: "⭐ Recommended (OpenRouter, Venice, Anthropic, OpenAI, Gemini)",
        providers: &[
            ("openrouter", "OpenRouter — 200+ models, 1 API key (recommended)"),
            ("venice", "Venice AI — privacy-first (Llama, Opus)"),
            ("anthropic", "Anthropic — Claude Sonnet & Opus (direct)"),
            ("openai", "OpenAI — GPT-4o, o1, GPT-5 (direct)"),
            ("openai-codex", "OpenAI Codex (ChatGPT subscription OAuth, no API key)"),
            ("deepseek", "DeepSeek — V3 & R1 (affordable)"),
            ("mistral", "Mistral — Large & Codestral"),
            ("xai", "xAI — Grok 3 & 4"),
            ("perplexity", "Perplexity — search-augmented AI"),
            ("gemini", "Google Gemini — Gemini 2.0 Flash & Pro (supports CLI auth)"),
        ],
    },
    ProviderTier {
        label: "⚡ Fast inference (Groq, Fireworks, Together AI, NVIDIA NIM)",
        providers: &[
            ("groq", "Groq — ultra-fast LPU inference"),
            ("fireworks", "Fireworks AI — fast open-source inference"),
            ("together-ai", "Together AI — open-source model hosting"),
            ("nvidia", "NVIDIA NIM — DeepSeek, Llama, & more"),
        ],
    },
    ProviderTier {
        label: "🌐 Gateway / proxy (Vercel AI, Cloudflare AI, Amazon Bedrock)",
        providers: &[
            ("vercel", "Vercel AI Gateway"),
            ("cloudflare", "Cloudflare AI Gateway"),
            ("astrai", "Astrai — compliant AI routing (PII stripping, cost optimization)"),
            ("bedrock", "Amazon Bedrock — AWS managed models"),
        ],
    },
    ProviderTier {
        label: "🔬 Specialized (Moonshot/Kimi, GLM/Zhipu, MiniMax, Qwen/DashScope, Qianfan, Z.AI, Synthetic, OpenCode Zen, Cohere)",
        providers: &[
            ("kimi-code", "Kimi Code — coding-optimized Kimi API (KimiCLI)"),
            ("qwen-code", "Qwen Code — OAuth tokens reused from ~/.qwen/oauth_creds.json"),
            ("moonshot", "Moonshot — Kimi API (China endpoint)"),
            ("moonshot-intl", "Moonshot — Kimi API (international endpoint)"),
            ("glm", "GLM — ChatGLM / Zhipu (international endpoint)"),
            ("glm-cn", "GLM — ChatGLM / Zhipu (China endpoint)"),
            ("minimax", "MiniMax — international endpoint (api.minimax.io)"),
            ("minimax-cn", "MiniMax — China endpoint (api.minimaxi.com)"),
            ("qwen", "Qwen — DashScope China endpoint"),
            ("qwen-intl", "Qwen — DashScope international endpoint"),
            ("qwen-us", "Qwen — DashScope US endpoint"),
            ("qianfan", "Qianfan — Baidu AI models (China endpoint)"),
            ("zai", "Z.AI — global coding endpoint"),
            ("zai-cn", "Z.AI — China coding endpoint (open.bigmodel.cn)"),
            ("synthetic", "Synthetic — Synthetic AI models"),
            ("opencode", "OpenCode Zen — code-focused AI"),
            ("cohere", "Cohere — Command R+ & embeddings"),
        ],
    },
    ProviderTier {
        label: "🏠 Local / private (Ollama, llama.cpp server — no API key needed)",
        providers: &[
            ("ollama", "Ollama — local models (Llama, Mistral, Phi)"),
            ("llamacpp", "llama.cpp server — local OpenAI-compatible endpoint"),
        ],
    },
    ProviderTier {
        label: "🔧 Custom — bring your own OpenAI-compatible API",
        providers: &[],
    },
];
```

Content is the CLI wizard's list **verbatim** (it is a strict superset of the
TUI's). Labels keep the emoji — the TUI list picker already renders non-ASCII
row text (★/✓/↓ in ClawHub rows, `src/tui/app.rs:136-145`).

## Step 2 — CLI wizard consumes the table

In `setup_provider` (`wizard.rs:2302`), replace the inline `tiers` vec
(`wizard.rs:2306-2313`) and the `providers` match (`wizard.rs:2321-2397`) with:

```rust
    let tiers: Vec<&str> = PROVIDER_SETUP_TIERS.iter().map(|t| t.label).collect();

    let tier_idx = Select::new()
        .with_prompt("  Select provider category")
        .items(&tiers)
        .default(0)
        .interact()?;

    let providers: Vec<(&str, &str)> = PROVIDER_SETUP_TIERS
        .get(tier_idx)
        .map(|t| t.providers.to_vec())
        .unwrap_or_default();
```

The `if providers.is_empty()` custom branch below (`wizard.rs:2400`) is
unchanged — the Custom tier's empty slice reproduces the old `_ => vec![]`
sentinel.

## Step 3 — TUI provisioner consumes the table

In `src/onboard/provision/provider.rs`, replace the inline `tiers` vec
(`provider.rs:69-76`) with:

```rust
        let tiers: Vec<String> = crate::onboard::wizard::PROVIDER_SETUP_TIERS
            .iter()
            .map(|t| t.label.to_string())
            .collect();
```

(`ProvisionEvent::Choose.options` is `Vec<String>` — hence the `to_string`.)

Replace the `providers` match (`provider.rs:102-140`) with:

```rust
        let providers: Vec<(&str, &str)> = crate::onboard::wizard::PROVIDER_SETUP_TIERS
            .get(tier_idx)
            .map(|t| t.providers.to_vec())
            .unwrap_or_default();
```

The `if providers.is_empty()` custom branch (`provider.rs:142`) is unchanged.
Do not touch `needs_key` (`provider.rs:269`), the gemini prompt label
(`provider.rs:284-288`), or the empty-key factory gate (`provider.rs:314-374`).

## Step 4 — Tests

Add to the existing `#[cfg(test)]` module in `wizard.rs`:

```rust
    #[test]
    fn provider_setup_tiers_have_no_duplicate_ids() {
        let mut seen = std::collections::HashSet::new();
        for tier in PROVIDER_SETUP_TIERS {
            for (id, _) in tier.providers {
                assert!(seen.insert(*id), "duplicate provider id in setup tiers: {id}");
            }
        }
    }

    /// Regression pin for the 2026-08-17 drift: the TUI copy of this table
    /// was missing these 11 ids. Now both surfaces read one table; this
    /// asserts the table itself never silently loses them again.
    #[test]
    fn provider_setup_tiers_offer_the_once_missing_providers() {
        let all: Vec<&str> = PROVIDER_SETUP_TIERS
            .iter()
            .flat_map(|t| t.providers.iter().map(|(id, _)| *id))
            .collect();
        for id in [
            "openai-codex", "astrai", "kimi-code", "qwen-code", "glm-cn",
            "minimax-cn", "qwen-intl", "qwen-us", "zai-cn", "synthetic",
            "opencode",
        ] {
            assert!(all.contains(&id), "provider {id} missing from setup tiers");
        }
    }

    /// Both drivers treat an empty provider list as "run the custom flow".
    /// Exactly one tier — the last — may be that sentinel.
    #[test]
    fn only_the_last_tier_is_the_custom_sentinel() {
        let (last, rest) = PROVIDER_SETUP_TIERS.split_last().expect("tiers non-empty");
        assert!(last.providers.is_empty(), "last tier must be Custom");
        for tier in rest {
            assert!(!tier.providers.is_empty(), "non-last tier {} is empty", tier.label);
        }
    }
```

**Index-sensitive tests**: `provider.rs` has smoke tests that select providers
by tier/row index — see the comment "Tier 0 picker indices (see the
`providers` table in `run`)" at `provider.rs:649` and the `empty_key_retry`
driving around `provider.rs:690`. Adding `openai-codex` at tier-0 index 4
shifts every later index. Re-derive each index in those tests from the new
table (`PROVIDER_SETUP_TIERS[0].providers.iter().position(...)` in the test is
acceptable and self-healing). Run them before and after: any test that passes
unchanged despite the shifted indices was vacuous — fix it, don't celebrate it.

## Step 5 — Validation

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib onboard::
```

Expected: the three new tests pass; every pre-existing `onboard::` test passes
(after the index re-derivation in Step 4).

Live drive (tmux, optional but recommended): launch the TUI, `/setup` →
provider, walk each tier; the Specialized tier must show 17 rows including
`kimi-code`, `qwen-code`, `synthetic`, `opencode`; Recommended must show
`openai-codex`; Gateway must show `astrai`. Pick `openai-codex`, press Enter on
an empty key: either it builds (cached Codex OAuth on this machine) or the
plan-150 warn + re-enter/abort choice appears. Both are correct; a silent save
of a provider that then fails to boot is not.

## Non-goals

- No per-provider OAuth guidance flows in the TUI (the CLI wizard's
  per-provider key-URL bullets, `wizard.rs:2657-2695`). The TUI's generic
  prompt + factory keyless-oracle already yields correct behaviour; guidance
  text parity is a separate UX effort.
- No change to the CLI wizard's `qwen-code`/`openai-codex` special key
  handling (`wizard.rs:2622-2656`) — CLI-only, stays where it is.
- No new providers. This is table unification, not catalog expansion.

## Risk and rollback

- Risk: LOW. The table is data; both consumers keep their existing control
  flow. Blast radius is the two setup surfaces.
- Rollback: revert the single commit. No config-schema change, no defaults
  widened — schema version untouched.

## STOP conditions

- Any pre-existing `provider.rs` smoke test passes **unchanged** despite the
  index shift — that test pins nothing; stop and fix the test, then continue.
- `PROVIDER_SETUP_TIERS` cannot be `pub(crate)`-reached from
  `provision/provider.rs` (module visibility) — stop and report rather than
  widening visibility beyond `pub(crate)`.
- The live drive shows a newly-added provider whose setup flow dead-ends
  (e.g. a provider the factory cannot build even WITH a key) — stop, report,
  and drop that id from the table in this PR rather than shipping a broken row.
