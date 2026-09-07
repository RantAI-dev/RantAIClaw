# Pillar 8 — Install, Packaging, and Release

> **ClickUp:** [v0.4.1 binary-first installer shipped](https://app.clickup.com/t/86exgrnwk) · [v0.5.1 GLIBC baseline](https://app.clickup.com/t/86exgrp36) · **Maturity:** Stable · **Modules:** `bootstrap.sh`, `scripts/install.sh`, `.github/workflows/`

Three numbers define this pillar: cold start, RAM, binary size. The install pipeline exists to defend those numbers across every supported platform.

## What this pillar covers

- One-line installer (`curl | bash`)
- Docker image (`ghcr.io/RantAI-dev/RantAIClaw:<tag>`)
- Multi-target release matrix (Linux x86_64 / aarch64 / armv7 · macOS x86_64 / aarch64 · Windows x86_64)
- GLIBC 2.35 baseline (Ubuntu 22.04 LTS, Debian 12, RHEL 9 supported)
- cosign keyless signing
- SHA256SUMS published with every release
- Verified release flow (`pub-release.yml`)
- Per-release CHANGELOG + version-string consistency

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Cold start | < 200ms (binary) | TBD | TBD |
| Idle RAM | ~15 MB | TBD | TBD |
| Binary size | ~12 MB (release) / 3.4 MB (size-optimized profile) | TBD | TBD |
| One-line install | ✅ `curl \| bash` | TBD | TBD |
| Docker image | ✅ GHCR, signed | TBD | TBD |
| Cross-platform release matrix | 6 targets | TBD | TBD |
| GLIBC LTS-distro coverage | 2.35 (Ubuntu 22.04+) | TBD | TBD |
| Cosign keyless signing | ✅ | TBD | TBD |
| SHA256SUMS | ✅ | TBD | TBD |
| No Python / no JVM / no GC | ✅ | TBD | TBD |

## Current state by maturity

_Rows below were read against the code on **2026-09-07**. A row with no evidence behind it is
a claim, not a status; where a row could not be settled by reading, it says so._

| Surface | Maturity |
|---|---|
| `curl \| bash` one-liner | Stable (v0.4.1) |
| PATH UX (auto-amends shell rc) | Stable (v0.4.2) |
| Docker image publish | Stable |
| GHCR + cosign | Stable |
| Multi-target build matrix | Stable (v0.5.1) |
| Verify-artifacts gate before publish | Stable |
| Weekly verification-only run | Stable |
| `--all-features` build | Still omitted from CI, but **no longer because of matrix-sdk** — 0.18 type-checks at the default recursion limit and `channel-matrix` has its own job. What `--all-features` would add now is the hardware/probe/postgres set (`rppal`, `probe-rs` + ~50 deps, `tokio-postgres`); that is a build-cost question, not a compile failure (`.github/workflows/ci-run.yml:236-239`) |

## Architecture

```
          source
            │
            ▼
   .github/workflows/pub-release.yml
            │
            ├── matrix(linux x86_64 / aarch64 / armv7)  ← ubuntu-22.04 (GLIBC 2.35 baseline)
            ├── matrix(macos x86_64 / aarch64)
            └── matrix(windows x86_64)
            │
            ▼
   verify-artifacts (sha256, archive-shape, smoke)
            │
            ▼
   pub-docker-img → ghcr.io/RantAI-dev/RantAIClaw:<tag>
            │
            ▼
   GitHub Release publish (binaries + cosign bundle + SHA256SUMS)
```

## Release cadence

- Patch / minor: weekly or bi-weekly
- Emergency security: out-of-band
- Release tags must be reachable from `origin/main`

## CLI / config

```bash
# One-liner install
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.sh | bash

# Docker
docker run --rm -it ghcr.io/RantAI-dev/RantAIClaw:latest

# Self-update (future)
rantaiclaw update
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — **Deliverable: Lightness Scoreboard Baseline** captures cold-start / RAM / binary / dep-count / LoC for RantaiClaw + OpenClaw + Hermes-agent on a clean Ubuntu 22.04 box. Output script: `dev/bench/lightness.sh`.
