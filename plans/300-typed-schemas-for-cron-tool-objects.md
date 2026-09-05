# Plan 300: Declare real schemas for the cron tools' object parameters, and tolerate stringified numbers

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat d5a1bba..HEAD -- src/tools/cron_add.rs src/tools/cron_update.rs src/cron/types.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P2 — reported from a live agent run, reproducible by reading
- **Effort**: S–M
- **Risk**: LOW
- **Category**: bug / tool contract
- **Planned at**: commit `d5a1bba`, 2026-09-05
- **Origin**: an agent reported `cron_add` rejecting its schedule because `every_ms` arrived
  as `"600000"` instead of `600000`, and concluded it could not proceed "without the schema
  being fixed". The conclusion is right for a reason the agent did not state: there **is** no
  schema for that field.

## Why this matters

`cron_add` advertises its `schedule` parameter as a bare object whose shape exists only in a
prose description. A model has no machine-readable type for `every_ms`, so a provider doing
constrained or structured decoding has nothing to constrain against — emitting a string is
not the model ignoring the contract, it is the model guessing in the absence of one.

The tool then deserialises straight into a strongly-typed enum, where serde rejects a string
for `u64` and returns its own error text. The result is an agent that can see the field name,
cannot see its type, and gets an error that does not say what to send instead. Scheduling is
one of the product's headline capabilities; this makes it unreachable for a model that
stringifies numbers — which models routinely do.

## Current state (verified at `d5a1bba`)

```rust
// src/tools/cron_add.rs:82-85 — the shape lives in a description string
"schedule": {
    "type": "object",
    "description": "Schedule object: {kind:'cron',expr,tz?} | {kind:'at',at} | {kind:'every',every_ms}"
},
```

```rust
// src/tools/cron_add.rs:107-108 — straight into the typed enum
let schedule = match args.get("schedule") {
    Some(v) => match serde_json::from_value::<Schedule>(v.clone()) {
```

```rust
// src/cron/types.rs:61-75 — internally tagged, every_ms is u64
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Cron { expr: String, #[serde(default)] tz: Option<String> },
    At { at: DateTime<Utc> },
    Every { every_ms: u64 },
}
```

The same class, found by a precise sweep of `src/tools/*.rs` — object parameters declared
with no `properties`: `cron_add.schedule`, `cron_add.delivery` (no description at all),
`cron_update.patch` (no description), and `composio.params` (legitimately dynamic — the shape
depends on the remote action; leave it).

## Steps

1. **Declare the schedule schema for real.** Replace the bare object with a `oneOf` over the
   three variants, each with `kind` as a `const`/`enum` and its fields typed —
   `every_ms` as `{"type":"integer","minimum":1}`, `expr` and `tz` as strings, `at` as
   `{"type":"string","format":"date-time"}`. Keep it faithful to `Schedule`: the enum is
   internally tagged on `kind`, so the discriminator belongs inside each branch.
   **Verify**: every field name and type matches `src/cron/types.rs`; a mismatch here teaches
   the model a wrong contract, which is worse than teaching it nothing.

2. **Do the same for `cron_add.delivery` and `cron_update.patch`.** Both are undocumented
   bare objects today. Derive their shape from the types they deserialise into, not from
   guesswork. Leave `composio.params` alone and add a one-line comment saying why.
   **Verify**: `rg -n '"type": "object"' src/tools/cron_*.rs` shows no property-less object.

3. **Tolerate a stringified number, deliberately and visibly.** Models stringify integers
   regardless of schema. Accept `"600000"` for `every_ms` via a `deserialize_with` helper on
   the field (or a normalisation pass before `from_value`). This is an intentional,
   documented tolerance at an input boundary — per CLAUDE.md §3.5, fallbacks are allowed when
   intentional and safe, and must be documented. Add a doc comment saying so.
   **Verify**: it accepts `600000` and `"600000"`, and still rejects `"ten minutes"`, `-1`
   and `0`.

4. **Make the error actionable when it does reject.** Serde's raw message is the wrong
   output for a model. On failure, return the expected shape and one concrete example, so the
   next attempt can succeed without a human.
   **Verify**: the failure text names the field, the expected type, and shows a valid object.

5. **Tests.** (a) integer accepted; (b) numeric string accepted and produces the identical
   `Schedule`; (c) a nonsense string rejected with a message containing an example;
   (d) the advertised schema declares `every_ms` as an integer — assert on
   `parameters_schema()` itself so the contract cannot silently regress.
   **Verify**: `cargo test --lib tools::cron_add` and `cargo test --lib cron` pass; test (d)
   fails if step 1 is reverted.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib tools::cron_add`, `cargo test --lib tools::cron_update`,
  `cargo test --lib cron` pass with the four new tests.
- `cron_add` succeeds for both `600000` and `"600000"`.

## STOP conditions

- The `delivery` or `patch` shapes turn out to be genuinely open-ended (like
  `composio.params`) → STOP for those two and document why, rather than inventing a schema
  that will drift from the code.
- Coercion would need to spread beyond these tools to be consistent → STOP and report; a
  crate-wide argument-normalisation layer is a separate design decision, not this plan.

## Test plan

Four tests in `cron_add`'s test module. Test (d) — asserting on the emitted schema — is the
one that keeps the contract honest as the enum evolves.

## Maintenance note

The rule: a tool parameter's schema is the only contract a model can actually read. A shape
described in prose is not a contract. When `Schedule` gains a variant, `parameters_schema`
must change in the same commit — test (d) is what forces that.

## Rollback

One commit across two tool files plus tests. No schema-version or storage change.
