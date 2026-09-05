# 081 — Newest Opus silently gets half the `max_tokens` of its predecessors

Written against `93c7511`. Risk tier: medium (`src/providers/**`, runtime path).

`with_anthropic_max_tokens`, `src/providers/rig_native.rs:253-263`:

```rust
req.max_tokens = Some(match model {
    m if m.starts_with("claude-opus-4-7") || m.starts_with("claude-opus-4-6") => 128_000,
    m if m.starts_with("claude-opus-4")
        || m.starts_with("claude-sonnet-4")
        || m.starts_with("claude-haiku-4-5") => 64_000,
    _ => 4_096,
});
```

The first arm enumerates two specific Opus versions. `claude-opus-4-8` — listed
as **"Claude Opus 4.8 (best quality)"** in the anthropic curated table
(`wizard.rs:851`) and used in `src/cron/mod.rs:530` — matches neither, falls to
the second arm, and gets **64k instead of 128k**.

So the newest flagship is capped at half its predecessor's output budget, with
no error and no log line. Long generations truncate and the operator sees a
short answer, not a failure.

The bug is structural, not a typo: an allowlist of exact versions in a codebase
where new versions arrive constantly will silently mis-tier every future model.
`claude-opus-4-9`, `claude-opus-5` and anything after will hit the same arm.

## A1 — fix the tier and make the shape resistant

Two parts, both needed:

1. Include `claude-opus-4-8` (and Opus 5, which the live OpenRouter and Venice
   catalogs already serve — see plan 080 D1) in the 128k tier.
2. Restructure so the *newest* model is not the one that falls through. Prefer
   ordering the match so unknown `claude-opus-*` lands in the **higher** tier
   with the older, known-lower versions enumerated explicitly — the failure mode
   of guessing too high is an API error the operator can see, versus silent
   truncation which they cannot.

If overshooting is unacceptable for a model whose real cap is lower, then log at
`warn` when a model matches no explicit arm, so the fallback is at least visible.
Do not leave a path where an unrecognized model is silently down-tiered.

## A2 — the same shape may exist elsewhere

`grep` for other `starts_with` version allowlists on model names before closing
this. `format_model_name` / `format_internal_model_name` in
`src/providers/gemini.rs:591-604` are the obvious neighbours to check.

## Validation

- Unit: `with_anthropic_max_tokens(req, "claude-opus-4-8")` returns 128_000.
- Unit: a made-up future version (`claude-opus-4-99`) does not land in the 4_096
  fallback. This is the assertion that actually encodes the lesson; the
  `4-8` case alone would pass again the day `4-9` ships.
- Unit: `claude-sonnet-4-6` and `claude-haiku-4-5-*` keep their current values —
  this fix must not move anything else.
- Do not accept a green suite as proof. Mutate the new arm and confirm the test
  fails; a tier test that passes against both values is vacuous.

## Rollback

Single small commit in one function. Revert restores current tiering exactly.
