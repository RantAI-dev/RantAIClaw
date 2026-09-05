# Plan 295: Stop release signature verification failing open

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/lifecycle/artifact.rs src/lifecycle/update.rs src/webui.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-5, part c)
- **Effort**: S–M
- **Risk**: MED — can block updates on machines without `cosign`
- **Category**: security (supply chain)
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

The project signs its releases with keyless cosign and advertises them as cosign-verified. The
verifier accepts two outcomes that skip verification entirely: `cosign` missing from `PATH`,
and the signature bundle returning 404. Both continue with a SHA-256 check against a checksum
file fetched from the same origin as the artefact — which an attacker who can serve the
artefact can also serve.

So the strongest supply-chain control the project has is optional in practice, on both the
self-update path and the `ui install` path.

## Current state (verified at `4b8f61e`)

```rust
// src/lifecycle/artifact.rs:90-91
    CosignNotInstalled,
    BundleMissing,
// :98-100 — the contract, as documented
/// * `Ok(CosignOutcome::CosignNotInstalled)` — `cosign` not on PATH; SHA-only
/// * `Ok(CosignOutcome::BundleMissing)`      — bundle file 404
```

Both outcomes are accepted by the update path and by `ui install`. The `BundleMissing`
tolerance exists for releases published before signing was introduced.

## Steps

1. **Make a missing bundle fatal for versions that are supposed to have one.** Releases from
   the signing cutover onward always publish a bundle; a 404 there means something is wrong.
   Keep the tolerance only for genuinely older versions, keyed on the version being installed.
   **Verify**: find the cutover version in `docs/contributing/release-process.md` or the
   workflow history rather than guessing.

2. **Decide the missing-`cosign` policy explicitly, and make it loud.** Options: refuse and
   tell the operator how to install cosign; or require an explicit
   `--allow-unverified` opt-in. Silence is not an option. Whichever is chosen, the message
   must state that the artefact was **not** verified.
   **Verify**: no code path continues after a skipped verification without the operator
   having asked for it.

3. **Tests for the negative paths.** Bundle 404 on a modern version → refuses; cosign absent
   without opt-in → refuses; both succeed with the opt-in or on a legacy version.
   **Verify**: `cargo test --lib lifecycle` and `cargo test --lib webui` pass.

4. **Correct the claim.** Pillar 10 advertises "cosign-verified". Once this lands it is true;
   if step 2 lands the opt-in variant, say so in the same doc.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib lifecycle`, `cargo test --lib webui` pass with the new negatives.
- Verification cannot be skipped silently on either the update or `ui install` path.

## STOP conditions

- Making it mandatory would break the documented install flow on a platform where cosign is
  not readily available → STOP and report; that is a product decision about which platforms
  get verified updates.

## Test plan

Four tests across the two paths, driving the outcome enum directly rather than the network.

## Maintenance note

A verification control that can be skipped by an attacker-influenceable condition (a 404) is
not a control. Any future outcome added to this enum needs the same question asked of it.

## Rollback

One commit. If it blocks updates in the field, reverting restores the permissive behaviour —
so land it early in a release cycle, not just before a release.
