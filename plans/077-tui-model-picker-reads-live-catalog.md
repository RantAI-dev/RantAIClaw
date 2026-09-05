# 077 — TUI `/model` never reads the model cache

Written against `93c7511`. Risk tier: medium.

`src/tui/commands/model.rs:50` builds the picker from curated data only:

```rust
for (id, desc) in crate::onboard::wizard::curated_models_for_provider(provider) {
```

So `rantaiclaw models refresh` and `doctor models` — both of which write
`models_cache.json` — have **no effect on the TUI**. Confirmed on a real
profile: cache held 400 live OpenRouter models; the TUI picker still offered 12.

This is worse than "stale". Five of those 12 curated OpenRouter IDs are not in
OpenRouter's live catalog at all (see plan 078), so the TUI offers 7 working
options and 5 that fail at call time.

## T1 — read the same catalog every other surface reads

`provider_model_catalog(workspace_dir, provider)` (`wizard.rs:1874`) already
exists for exactly this and is what the CLI (`models list`) and the gateway
(`GET /providers/{id}/models`, `api_v1.rs:2056`) both use. Its doc comment
claims it "mirrors the TUI picker which overlays live onto the curated base"
(`wizard.rs:1884`) — that sentence is false today and becomes true with this fix.

The picker needs a description per entry and the catalog returns bare IDs, so
keep the curated `(id, label)` map as a **label lookup**: catalog order wins,
curated supplies a description when it has one, otherwise show the ID alone.

`TuiContext` must carry `workspace_dir` (or reach it via config) for the call.

## T2 — drop the duplicate cache reader in `channels/`

`src/channels/mod.rs` declares its own `MODEL_CACHE_FILE` (`:117`), its own
`MODEL_CACHE_PREVIEW_LIMIT` (`:118`), and `load_cached_model_preview` (`:942`)
which re-implements the path join and `ModelCacheState` deserialization.

That is a fourth reader of one file with its own copy of the parsing rules —
the same duplication shape that let the catalog surfaces drift apart in the
first place. Route it through `provider_model_catalog` and take the first N,
then delete both consts and the function.

Rule-of-three is satisfied: wizard, gateway, channels, TUI — four call sites for
one resolver.

## Non-goals

- Not adding a refresh trigger to the TUI. `/model` should render what is
  cached; refreshing is plan 079's concern.
- Not changing the free-form path — `/model <provider>:<id>` (`model.rs:24-33`)
  stays as-is; it is the current workaround and remains useful.

## Validation

- Unit: with a seeded `models_cache.json`, the picker's item list contains a
  cache-only ID that appears in no curated list.
- Unit: with no cache present, the picker still renders the curated list
  (no regression for a fresh install).
- Manual (tmux): `models refresh --force --provider openrouter`, then open
  `/model` in the TUI and confirm the count matches `models list`.

## Rollback

Single-commit revert per finding; T1 and T2 are independent.
