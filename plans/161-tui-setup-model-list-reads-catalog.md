# Plan 161: TUI setup's model step must read the shared catalog, not the curated list alone

> **Executor instructions**: Follow this plan step by step. One PR. Run every
> verification command including the live drive. If anything in "STOP
> conditions" occurs, stop and report. When done, add/update this plan's row
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 621d3cd..HEAD -- src/onboard/provision/provider.rs src/onboard/wizard.rs`
> All line numbers below are from `621d3cd` (v0.22.2-alpha). If this diff is
> non-empty, re-verify each cited line before editing.

## Status

- **Priority**: P2 — the TUI setup offers ~10 openrouter models while the same
  binary's `/model` picker offers 400 from the on-disk cache; operators think
  the models "aren't supported"
- **Effort**: S
- **Risk**: LOW (swaps a data source for one three other surfaces already use)
- **Depends on**: none (lands on v0.22.2-alpha)
- **Category**: bugfix
- **Planned at**: commit `621d3cd` (v0.22.2-alpha), 2026-08-18

## Why this matters

Operator report: after picking openrouter in TUI `/setup provider`, the model
list does not show all models. Verified on a live box:

- `models_cache.json` (`profiles/<name>/workspace/state/`) holds
  openrouter=400, venice=105, nvidia=100 — written by `models refresh` and the
  CLI wizard's live fetch.
- The `/model` picker, the gateway (`api_v1.rs:2040`), and channel routing
  (`routing.rs:637`) all read `provider_model_catalog` — the read-through
  view (fresh-ignoring cache → fallback curated) built exactly so "no surface
  drifts" (its own doc comment, `wizard.rs:1940-1954`).
- The TUI provisioner's model step (`provider.rs:481`) reads
  `curated_models_for_provider` alone: ~10 rows for openrouter.
- The CLI wizard's model step (`wizard.rs:2775-2889`) offers curated + cache +
  optional live fetch, capped at `LIVE_MODEL_MAX_OPTIONS` (120).

Third instance of the hand-copy drift class in this exact file pair
(endpoints v0.16.1-alpha, provider table #566, now models). A fourth copy
rides along: `provider.rs:546` has a private `default_model_for_provider`
whose openrouter default is `anthropic/claude-sonnet-4-20250514` — an id the
curated list no longer even contains — drifted from the canonical
`wizard.rs::default_model_for_provider` (`wizard.rs:814`).

## Step 1 — Model options come from the catalog

In `src/onboard/provision/provider.rs`, replace the model-options block
(`provider.rs:477-498`) with:

```rust
        // Read the same catalog every other surface uses (`/model` picker,
        // gateway, channel routing): cached live models when `models
        // refresh`/the wizard has written them, curated otherwise. This
        // used to read the curated list alone — ~10 rows for openrouter
        // while the same binary's /model picker offered 400 from the cache.
        // Capped like the CLI wizard's Select: the setup Choose overlay has
        // no filter box, and the full openrouter list is 400 rows.
        let catalog = crate::onboard::wizard::provider_model_catalog(
            &config.workspace_dir,
            provider_name,
        );
        let curated = crate::onboard::wizard::curated_models_for_provider(provider_name);
        let describe = |id: &str| {
            curated
                .iter()
                .find(|(curated_id, _)| curated_id == id)
                .map(|(_, description)| description.clone())
        };
        let (model_ids, model_labels): (Vec<String>, Vec<String>) = if catalog.models.is_empty() {
            // No cache and no curated list — fall back to a single
            // "default" option so the user still has something to pick.
            let fallback = crate::onboard::wizard::default_model_for_provider(provider_name);
            (
                vec![fallback.clone()],
                vec![format!("{fallback} (default)")],
            )
        } else {
            catalog
                .models
                .into_iter()
                .take(crate::onboard::wizard::LIVE_MODEL_MAX_OPTIONS)
                .map(|id| {
                    let label = match describe(&id) {
                        Some(desc) => format!("{id}  —  {desc}"),
                        None => format!("{id}  ({})", catalog.source),
                    };
                    (id, label)
                })
                .unzip()
        };
```

Notes:
- `provider_model_catalog` and `curated_models_for_provider` are already
  `pub(crate)`/`pub` in `wizard.rs`; `LIVE_MODEL_MAX_OPTIONS`
  (`wizard.rs:59`) must become `pub(crate)` (it is a private const today).
- `catalog.source` is `"cache"` or `"curated"` — reused as the label suffix
  for rows the curated list has no description for, matching the wizard's
  `build_model_options` convention.
- `config.workspace_dir` is in scope (the `run` signature's `config`).
- Borrow note: `describe` borrows `curated`, and the `unzip` consumes
  `catalog.models` — but `catalog.source` is read inside the closure after
  the struct is partially moved. Bind `let source = catalog.source;` before
  the `into_iter()` if the borrow checker objects.

## Step 2 — Delete the drifted `default_model_for_provider` copy

Delete the private fn at `provider.rs:546` entirely and point its two callers
(`provider.rs:485` — replaced by Step 1 — and `provider.rs:516`, the
out-of-range selection fallback) at
`crate::onboard::wizard::default_model_for_provider`. Check the wizard fn's
visibility (`wizard.rs:814`) — raise to `pub(crate)` if private. Its defaults
are the maintained set (the local copy's openrouter default names a model the
curated list no longer contains).

## Step 3 — Tests

In `provider.rs`'s test module (the smoke harness from plan 150 is already
there):

```rust
    /// The model step must offer what `models refresh` cached — the
    /// curated-only regression showed ~10 openrouter rows while /model
    /// offered 400 from the same cache.
    #[tokio::test]
    async fn model_step_offers_cached_models_not_just_curated() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();

        let cache_only = "openrouter/some-model-only-the-live-catalog-has";
        crate::onboard::wizard::cache_live_models_for_provider(
            tmp.path(),
            "openrouter",
            &[cache_only.to_string()],
        )
        .expect("write cache");

        let t = drive(
            &ProviderProvisioner::new(),
            &mut config,
            &profile,
            vec![
                Answer::Pick(PICK_TIER_RECOMMENDED),
                Answer::Pick(pick_provider("openrouter")),
                Answer::Text(""),  // keyless — openrouter builds without a key
                Answer::Pick(0),   // first (and only) model row
            ],
        )
        .await;

        assert!(
            t.events.iter().any(|e| matches!(
                e,
                super::ProvisionEvent::Choose { id, options, .. }
                    if id == "model" && options.iter().any(|o| o.contains(cache_only))
            )),
            "the model Choose must surface what `models refresh` wrote"
        );
        assert_eq!(config.default_model.as_deref(), Some(cache_only));
    }
```

(Adapt `drive`/`Answer`/`scratch_profile` names to the harness as found —
they exist in this file's tests from plan 150.) Verify the cache write path:
`cache_live_models_for_provider` takes the workspace dir; if the harness's
`scratch_profile` derives a different workspace, write the cache to whatever
dir the provisioner will read (`config.workspace_dir`).

**Mutation-proof**: revert Step 1's data source (put
`curated_models_for_provider` back as the only source) — this test must FAIL
on the `cache_only` assert. Restore.

## Step 4 — Validation

```bash
cargo fmt --all -- --check
cargo test --lib onboard::
cargo test --lib tui::
```

Clippy: diff the error set against clean main (local 1.97 emits ~169
pre-existing diagnostics; the set must not grow).

## Step 5 — Live drive (the exit condition)

Full-`HOME` sandbox (copy `~/.rantaiclaw`, strip `[gateway.login]` from both
configs — the technique from the 2026-08-18 drives; the profile cache already
holds openrouter=400):

1. TUI → `/setup provider` → Recommended → OpenRouter → Enter on empty key.
2. **Expected**: the model Choose shows 120 rows (the cap) of live-cache ids —
   not the ~10 curated rows. Rows present in the curated list keep their
   descriptions; the rest are labelled `(cache)`.
3. Esc out (don't save). Then on a FRESH empty sandbox (no cache): the same
   flow must show the curated list — the fallback still works.

## Non-goals

- No live fetch from inside the TUI provisioner (`fetch_live_models_for_provider`
  is blocking; plumbing it through the async overlay is its own change). The
  cache is populated by `models refresh` and the CLI wizard; the TUI now
  reads it. A fresh install with no cache sees curated — same as before.
- No filter box for the setup Choose overlay (UX effort; the 120 cap matches
  the CLI wizard's Select).
- `/model` picker untouched — it already reads the catalog uncapped and has a
  filter.

## Risk and rollback

- Risk: LOW — the catalog read-through is the exact function three other
  surfaces already call; the curated fallback is the catalog's own.
- No schema change.
- Rollback: revert the single commit; setup degrades to curated-only.

## STOP conditions

- `provider_model_catalog` turns out to need a workspace dir the provisioner's
  `config.workspace_dir` does not point at in real runs (check what the TUI
  passes into the provisioner) — stop and verify with a live drive before
  guessing at paths.
- The smoke-test harness cannot express the cache-backed drive (missing
  helper) — stop and report rather than weakening the test to a unit test of
  the label closure.
- The mutation in Step 3 leaves the test green — fix the test, not the code.
