# Pillar 9 — Documentation and Adoption

> **Maturity:** Active · **Modules:** `docs/`

The docs are a product surface. They have to convince a new visitor in 30 seconds that RantaiClaw is **feature-complete and stays lean** alongside OpenClaw and Hermes-agent — and they have to mirror the ClickUp PM ladder so that what's planned for the current release is the same answer in repo, in docs, and in PM.

## What this pillar covers

- Entry hub (`docs/README.md`) — 30-second decision tree
- Pillar docs (this directory) — one per product pillar, mirroring the ClickUp `[Product]` task
- Runtime contracts (`docs/reference/`) — what users build against
- Operations / security / hardware / contributing — domain trees
- Project snapshots (`docs/project/`) — date-stamped, immutable

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Entry-point clarity (30-sec decision tree) | ✅ | TBD | TBD |
| Pillar-level docs (single source of truth) | ✅ 9 pillars | TBD | TBD |
| Competitor-parity matrix per pillar | ✅ this template | TBD | TBD |
| ClickUp / PM linkage | ✅ every pillar + every release | n/a | n/a |
| Runtime-contract refs (commands / config / providers / channels) | ✅ | TBD | TBD |
| Project snapshots (immutable, date-stamped) | ✅ | TBD | TBD |
| Multilingual mirrors | Dropped — not yet committed to | TBD | TBD |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Entry hub | Implemented · needs further slimming |
| Pillar docs (this set) | Implemented (2026-05-06) · needs validation |
| Runtime contracts | Stable |
| Operations runbook | Stable |
| Security model | Implemented · needs merge cleanup |
| Hardware tree | Implemented · niche |
| Contributing | Stable |
| Project snapshots | Stable |
| Multilingual parity | **Dropped** — was claimed, never implemented |

## Architecture

```
docs/
├── README.md                  ← entry hub
├── SUMMARY.md                 ← TOC
├── start/                     ← getting-started
├── reference/                 ← runtime contracts
├── pillars/                   ← this directory (1-9)
├── operations/                ← day-2 ops
├── security/                  ← model + threats
├── hardware/                  ← niche but supported
├── contributing/              ← contributor flow
└── project/                   ← time-bound snapshots
    └── archive/               ← superseded planning artifacts
```

## Doc governance

- Project snapshots are **date-stamped and immutable** once superseded by a newer date. Superseded snapshots live under `docs/project/archive/` for design-rationale history.
- Runtime-contract references (commands / config / providers / channels) **must track behavior changes** — every code PR that affects a CLI flag or config key updates the corresponding ref doc.
- Pillar docs link to ClickUp release tasks; when a release ships, the pillar's "Current state by maturity" table is updated to reflect what moved.
- Plans / specs that have shipped are **archived** under `docs/project/archive/<topic>/`, not deleted — git history alone isn't enough for design rationale.

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — **Deliverable: Feature Parity Audit v1** produces a per-pillar matrix vs OpenClaw + Hermes-agent (replaces every TBD in this pillar set). **Deliverable: Lightness Scoreboard Baseline** captures the lightness-gap numbers. Phase B of the docs cleanup also lands here (file moves into new IA).
