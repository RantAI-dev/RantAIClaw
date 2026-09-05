# 076 — Model-probe error path leaks `api_url` verbatim

Written against `93c7511`. Risk tier: **HIGH** (`src/gateway/**`, secret handling).

Found by running `rantaiclaw doctor models` on a real profile. The
`[llamacpp]` line printed a live OpenRouter API key in plaintext:

```
❌ error: failed to refresh models for provider 'llamacpp':
   model fetch failed: GET sk-or-v1-<redacted>/models: buil…
```

The key was **not transmitted** — `buil…` is reqwest's builder error for a
relative URL with no scheme, which fails at parse time before any socket
opens. The leak is to stdout, and therefore to terminal scrollback, CI logs,
and any harness that persists command output.

Four independent links produced it. Each is separately fixable and each
should be fixed; the chain is only as safe as its weakest link.

## S1 — error context interpolates a config value that can hold a secret

`fetch_live_models_for_provider` → `fetch_openai_compatible_models` wraps the
failure with `.context("model fetch failed: GET {endpoint}")`. `endpoint` is
derived from config and is echoed raw.

Redact at the source: log scheme + host + path only, never the full string,
and never a string that failed URL parsing (an unparseable "endpoint" is by
definition not a URL and may be anything). Same treatment for the sibling
contexts in `fetch_openrouter_models` (`wizard.rs:1516`), `fetch_anthropic_models`
(`:1543`), and the gemini/ollama fetchers.

Fix this one even if S2–S4 land — it is the only link that bounds the blast
radius of every future mistake upstream of it.

## S2 — llamacpp consumes the *global* `config.api_url`

`run_models_refresh` (`wizard.rs:2024`) passes `config.api_url` into every
provider probe:

```rust
fetch_live_models_for_provider(&provider_name, &api_key, config.api_url.as_deref())
```

`resolve_live_models_endpoint` (`wizard.rs:1625-1636`) is the only branch that
trusts that argument unconditionally:

```rust
if canonical_provider_name(provider_name) == "llamacpp" {
    if let Some(url) = provider_api_url... {
        return Some(format!("{normalized}/models"));   // whatever it holds
    }
}
models_endpoint_for_provider(provider_name).map(str::to_string)
```

Every other provider ignores it — the repo's own test
`resolve_live_models_endpoint_falls_back_to_provider_defaults` (`:6383`) asserts
venice + a custom URL still resolves to venice's constant.

So a *global* field, set while some other provider was active, is consumed as
llamacpp's base URL. Gate it: only honour `api_url` when the active provider
*is* llamacpp (`config.default_provider`), else fall through to the constant.

## S3 — `PUT /secrets` stores `api_url` with zero validation

`src/gateway/config_api.rs:762-770`:

```rust
if let Some(u) = body.api_url.as_ref() {
    let u = u.trim();
    cfg.api_url = if u.is_empty() { None } else { Some(u.to_string()) };
}
```

Any non-empty string is accepted. The `api_key` branch immediately above it
routes into `provider_api_keys` and is **encrypted at rest** via `Config::save`;
`api_url` is written to `config.toml` in plaintext. A credential that lands in
the wrong field is therefore stored unencrypted.

Parse with `reqwest::Url::parse` and reject non-`http`/`https` schemes, matching
what the interactive ollama path already does (`wizard.rs:2343-2347`). Return
400 with a field-specific message rather than storing the value.

Add the same validation to any CLI/config-set path that writes `api_url`.

## S4 — reject secret-shaped values on the way in

Defence in depth for the field-confusion case that produced this: when a value
destined for `api_url` matches a known key prefix (`sk-`, `sk-or-v1-`,
`sk-ant-`, `AIza`, …), fail closed with "that looks like an API key, not a URL"
instead of persisting it. Cheap, and it catches the paste-into-wrong-box error
that no amount of downstream redaction prevents.

## Open question — which surface wrote it

Confirmed from code that **no setup path can produce this**: the custom-provider
prompt (`provision/provider.rs:203`), the ollama prompt (`wizard.rs:2349`,
URL-parsed and scheme-checked) and the llama.cpp prompt (`:2390`) all write a
value the operator entered as a URL.

That leaves a hand-edited `config.toml` or the web console's Secrets form. If
claw-ui's form can submit the key to both fields — or mislabels them — that is a
separate bug in the other repo and needs its own issue. Check before closing
this plan; S3+S4 make it harmless either way, but a form that mis-maps a
credential is worth knowing about.

## Validation

- Unit: `resolve_live_models_endpoint("llamacpp", Some("sk-or-v1-abc"))` with a
  non-llamacpp `default_provider` must **not** return an endpoint built from it.
- Unit: every fetch error context, given an endpoint containing `sk-test-secret`,
  must not contain that substring. Mirror the existing shell-tool assertion
  (`src/tools/shell.rs:854`) which already proves this pattern for env vars.
- Integration: `PUT /secrets {api_url: "sk-or-v1-abc"}` → 400, and `config.toml`
  unchanged.
- Manual: `rantaiclaw doctor models` on a profile with a bad `api_url` must print
  no secret material.

## Rollback

Each link is an independent small commit; revert individually. S3 is the only
one with a user-visible contract change (a previously-accepted value now 400s) —
call it out in the PR body.
