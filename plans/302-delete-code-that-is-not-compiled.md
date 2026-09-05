# Plan 302: Delete the modules that are not compiled, and the docs that advertise them

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/runtime/ src/observability/ src/skillforge/ README.md`

## Status

- **Priority**: P2 (ledger W2-1, part a) · **Effort**: S · **Risk**: LOW
- **Category**: tech-debt / honesty
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

Three bodies of code are advertised to users and cannot run, because nothing declares them as
modules. They are not "unfinished" — they are unreachable, and the compiler never sees them,
so they cannot even be trusted to build. Meanwhile the README tells operators that `wasm` is a
runtime choice and that `broadcast` is an observability backend, and both the README and the
pillar doc describe `skillforge` as implemented.

Deleting them raises the honesty of the product more than the tests attached to them suggest:
tests over uncompiled code prove nothing, and `runtime/wasm.rs` carries 36 of them.

## Current state (verified at `bf77d26`)

- `src/runtime/mod.rs` — no `mod wasm;` (687 lines, 36 tests, never built).
- `src/observability/mod.rs` — no `mod broadcast;`.
- `src/skillforge/` — 1,118 lines; the only reference anywhere is `mod skillforge;` at
  `src/main.rs:101`. No CLI verb, no config key, no caller.

## Steps

1. **Confirm zero reachability for each, one at a time.** `rg -n '<symbol>' src/` for the
   public types of each module. A single production reference means STOP for that module.
2. **Delete each module and its `mod` line where one exists.** Separate commits per module so
   any one can be reverted alone.
3. **Remove what advertises them**: the README's runtime list (`wasm`) and observability
   backends (`broadcast`), and the README + pillar 4 claims about `skillforge`. Replace, do
   not silently drop — say what the supported set actually is.
4. **Check the feature flags.** `Cargo.toml` may carry aliases that only these modules used.
   Remove flags with no remaining consumer; a feature flag with no consumer is itself the
   YAGNI violation CLAUDE.md §3.2 names.
5. **Verify the build is unchanged**: `cargo build -p rantaiclaw --lib` and
   `cargo build -p rantaiclaw --bin rantaiclaw` both clean. Nothing should have depended on
   these, and if something did, step 1 was wrong.

## Done criteria

- `cargo fmt --all -- --check`, `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness`
  clean; library and binary both build.
- `rg -n 'wasm|broadcast|skillforge' README.md docs/pillars/` returns only accurate statements.
- No orphaned feature flag remains.

## STOP conditions

- Any module turns out to be referenced from a feature-gated path that CI does not build →
  STOP; that is a "wire it or gate it honestly" decision, not a deletion.
- Deleting `skillforge` would remove a design others are building on → STOP and ask.

## Maintenance note

A module the compiler never sees cannot be maintained, only accumulated. If one of these is
wanted later, it comes back with a `mod` line, a caller, and CI coverage in the same PR.

## Rollback

Three independent commits; revert any one.
