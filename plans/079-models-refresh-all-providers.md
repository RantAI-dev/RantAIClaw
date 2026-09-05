# 079 — `models refresh` does one provider; the all-provider path is hidden

Written against `93c7511`. Risk tier: low.

`rantaiclaw models refresh` refreshes exactly one provider — the `--provider`
override, else `config.default_provider`, else `openrouter`
(`wizard.rs:1987-1991`). An operator with several providers configured has to
run it once per provider and has no way to learn the list.

The all-provider loop already exists. It is `rantaiclaw doctor models`
(`src/doctor/legacy.rs:148`), which defaults to **every** known provider, calls
the same `run_models_refresh` per target (`:177`), writes the same cache, and
buckets failures so one bad provider does not abort the batch:

```rust
fn doctor_model_targets(provider_override: Option<&str>) -> Vec<String> {
    if let Some(provider) = ... { return vec![provider.to_string()] }
    crate::providers::list_providers().into_iter().map(|p| p.name.to_string()).collect()
}
```

`classify_model_probe_error` (`:110`) sorts errors into Skipped / AuthOrAccess /
Error and the loop continues. That is the correct batch semantic and it is
already written and already shipping.

So this is not a missing feature. It is a discoverability defect: the
obviously-named command does the narrow thing, and the command that does the
useful thing is named after a diagnostic nobody greps for when they want to
update a model list.

## M1 — add `models refresh --all`

Wire `--all` to the existing enumerator and loop. Do not write a second loop.

Two call sites exist (`doctor::run_models` and `models refresh`), and this is
the third consumer, so rule-of-three is satisfied for extracting the shared
piece. Suggested shape: move `doctor_model_targets` + the classify/count loop
into `onboard::wizard` next to `run_models_refresh`, expose one
`refresh_providers(config, targets, force) -> RefreshSummary`, and have both
`doctor models` and `models refresh --all` call it. `doctor` keeps its own
presentation (the emoji-per-provider report); `models refresh --all` prints a
terser summary.

Do **not** make `--all` the default for a bare `models refresh` — that would turn
a single-provider command into a fan-out that sends credentials to ~30
endpoints, which is a surprise the current name does not carry.

## M2 — the fan-out sends credentials broadly; say so

`fetch_live_models_for_provider` falls back to `std::env::var(provider_env_var(…))`
when config has no key (`wizard.rs:1653`). So an all-provider refresh sends
whatever provider keys are in the environment to each provider's own endpoint.

That is the correct destination in each case, and it is what a catalog probe
must do — but it is a wider action than "refresh my model list" sounds like.
Note it in `--help` for both `--all` and `doctor models`.

## M3 — gateway parity (optional, decide before implementing)

`POST /providers/{id}/models/refresh` (`api_v1.rs:2071`) is per-provider only, so
the web console's refresh button has the same limitation. If the console grows a
"refresh all" control it needs a route; if not, skip this — YAGNI. Do not add
the route speculatively.

## Validation

- Unit: `refresh_providers` with a mix of supported/unsupported providers returns
  a summary with the right per-bucket counts and does not return `Err` for the
  batch.
- Unit: one provider erroring does not stop later providers (regression guard —
  this is the failure shape that has bitten batch operations here before).
- Manual: `models refresh --all` then `models list` for two different providers,
  both served from cache.

## Rollback

Additive flag plus a pure extraction. Revert the extraction commit and both
callers return to their current inline paths.
