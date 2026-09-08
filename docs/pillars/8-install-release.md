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
| Idle RAM | **27.4 MiB** TUI · **26.7 MiB** gateway · **30.3 MiB** daemon (all idle, flat) | TBD | TBD |
| Binary size | **20.5–34.5 MiB** depending on target; **34.5 MiB** on x86_64 Linux | TBD | TBD |
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

### All six targets, measured 2026-09-08

The earlier pass measured one target because cross-compiling six on the development machine did
not fit on disk. It did not have to: the artefacts are published. These are the **shipped**
binaries of `v0.30.0-alpha`, downloaded from the release and measured after extraction.

| Target | Bytes | MiB | Headroom to the 35 MB cap |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | 36,159,376 | **34.48** | **0.52 MiB** |
| `x86_64-apple-darwin` | 30,118,616 | 28.72 | 6.28 MiB |
| `x86_64-pc-windows-msvc` | 29,429,760 | 28.07 | 6.93 MiB |
| `aarch64-unknown-linux-gnu` | 28,295,856 | 26.99 | 8.01 MiB |
| `armv7-unknown-linux-gnueabihf` | 26,862,132 | 25.62 | 9.38 MiB |
| `aarch64-apple-darwin` | 21,476,752 | 20.48 | 14.52 MiB |

The spread is the finding. **Size pressure is one target's problem, not the platform's**: the
same source is 14 MiB clear of the cap on aarch64-darwin and 0.52 MiB from it on x86_64-linux.
A future "the binary is too big" conversation should start there rather than treating 34.5 MiB
as what RantaiClaw weighs.

### Idle RSS for the processes an operator leaves running

Measured 2026-09-08 against the released `v0.30.0-alpha` binary, `VmRSS` from
`/proc/<pid>/status` sampled every 5 s from t+20 s to t+45 s on a scratch `HOME`:

| Process | Idle RSS | Behaviour over the window |
|---|---|---|
| `rantaiclaw chat` (TUI) | 27.4 MiB | flat |
| `rantaiclaw gateway` | **26.7 MiB** (27,308 kB) | flat — identical at every sample |
| `rantaiclaw daemon` | **30.3 MiB** (31,032 kB) | settles: 29,768 kB → 31,032 kB over the first 30 s, then flat |

The daemon's extra ~3.6 MiB over the gateway is the channel runtime, scheduler and heartbeat it
starts on top; its rise-then-flat shape is those subsystems coming up, not a leak.

**What is still not measured**: the `TBD` columns for OpenClaw and Hermes-agent. Nobody has run
them, and that is a blank rather than a zero.

**Against the gate**: the safeguard is 35 MB (36,700,160 bytes), so the largest shipped binary
has **0.52 MiB of headroom**. `check_binary_size.sh` previously emitted a warning for *every*
target of every release — its 5 MB aspiration is below all six — which is why that one is now
reported rather than warned on. The 30 MB advisory fires on exactly one of the six, which makes
it a signal worth reading.

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
