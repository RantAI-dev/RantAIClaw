# Plan 304: Decide Matrix — remove it, or make it build

> **Executor instructions**: this plan asks for a decision with evidence, then executes it.
> Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/channels/matrix.rs Cargo.toml docs/reference/matrix-e2ee-guide.md`

## Status

- **Priority**: P2 (ledger W2-1, part c) · **Effort**: M (remove) / L (fix) · **Risk**: LOW
- **Category**: direction / tech-debt
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

`src/channels/matrix.rs` is 1,194 lines that no CI job builds — `matrix-sdk 0.16` overflows
the compiler's recursion budget, and the file says so in its own comment. Its tests contain a
type error, so they have never run. The channel is documented, has a dedicated E2EE guide that
never mentions the feature gate, and the audit scored it 18 — the lowest of any surface.

An unbuildable module cannot ship in a stable release, and "under development" labelling does
not cover code that does not compile. This is the one channel decision that cannot be deferred
by relabelling.

## Steps

1. **Establish the real cost of fixing.** Check whether a newer `matrix-sdk` builds within the
   recursion budget, and what the dependency weight is against the release binary's size
   headroom (the audit measured ~1 MiB of headroom under a hard cap). Record the finding.
2. **Decide, and write the reasoning in the PR body.** Remove is the recommended default:
   nobody can be using a channel that has never been built into any release binary.
3. **If removing**: delete the module, its feature flag, its catalog entry, its config
   section, the E2EE guide, and its rows in `channels.md` and pillar 5. A `CHANGELOG` entry
   under Removed, naming what an affected operator should do.
4. **If fixing**: pin or fork the SDK, add a CI job that builds it (a channel with no CI job
   is how this happened), fix the test type error, and drive it live before calling it
   supported — the tier rules in plan 280 apply.
5. **Either way, remove the false claims.** Pillar 5 calls Matrix "feature-gated";
   `channels.md` calls it "unbuildable". Both cannot be true.

## Done criteria

- Library and binary build; `cargo build --features channel-matrix` either works or the
  feature no longer exists.
- No document describes Matrix as available when it is not.

## STOP conditions

- Anyone is known to run a custom build with this channel → STOP and ask; removal then needs a
  deprecation path.

## Rollback

Removal is one commit and recoverable from git history; note the commit in the CHANGELOG.
