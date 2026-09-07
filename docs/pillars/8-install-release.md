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
| Cold start | **< 10 ms** (`--version`), **~10 ms** (`status`, loads config + profile) | TBD | TBD |
| Idle RAM | **27.4 MiB** (TUI, idle, flat over 20 s) | TBD | TBD |
| Binary size | **34.5 MiB** (36,170,960 bytes) | TBD | TBD |
| One-line install | ✅ `curl \| bash` | TBD | TBD |
| Docker image | ✅ GHCR, signed | TBD | TBD |
| Cross-platform release matrix | 6 targets | TBD | TBD |
| GLIBC LTS-distro coverage | 2.35 (Ubuntu 22.04+) | TBD | TBD |
| Cosign keyless signing | ✅ | TBD | TBD |
| SHA256SUMS | ✅ | TBD | TBD |
| No Python / no JVM / no GC | ✅ | TBD | TBD |

### How the footprint numbers were measured

Measured **2026-09-07** at `v0.30.0-alpha`, replacing estimates that were wrong by roughly
3× — the row said `~12 MB (release) / 3.4 MB (size-optimized profile)` against a real 34.5 MiB.

- **Method**: `cargo build --profile release-fast --locked` on `x86_64-unknown-linux-gnu`,
  rustc 1.92.0. That is the profile `pub-release.yml` ships and, as of this change, the one
  the Dockerfile builds too. `release-fast` differs from `release` only in `codegen-units`
  (8 vs 1); both set `opt-level="z"`, `lto="fat"`, `strip=true`, `panic="abort"`.
- **Binary size**: `stat` on the built artefact. 36,170,960 bytes.
- **Idle RAM**: `VmRSS` from `/proc/<pid>/status` for `rantaiclaw chat` on a scratch `HOME`,
  read at 12 s and again at 32 s — identical, so nothing is growing at idle.
- **Cold start**: best of five `--version` runs and best of three `status` runs under
  `/usr/bin/time`. Both below the 10 ms resolution of that measurement.

**Scope, stated rather than implied**: these are **one target and one process**. The release
matrix builds six targets; the other five are not measured here, and neither is the gateway or
daemon at idle. The `TBD` columns for OpenClaw and Hermes-agent are still `TBD` — nobody has
run them.

**Against the gate**: the safeguard is 35 MB (36,700,160 bytes), so the shipped binary has
**~0.5 MiB of headroom**, not the ~1 MiB previously assumed. `check_binary_size.sh` emits its
30 MB advisory warning on every release today.

## Current state by maturity

| Surface | Maturity |
|---|---|
| `curl \| bash` one-liner | Stable (v0.4.1) |
| PATH UX (auto-amends shell rc) | Stable (v0.4.2) |
| Docker image publish | Stable |
| GHCR + cosign | Stable |
| Multi-target build matrix | Stable (v0.5.1) |
| Verify-artifacts gate before publish | Stable |
| Weekly verification-only run | Stable |
| `--all-features` build | All except `channel-matrix` (matrix-sdk recursion-limit) |

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
