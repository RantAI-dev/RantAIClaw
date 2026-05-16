# Codebase Bloat Audit — 2026-05-06

> **Status:** snapshot, immutable. Audit performed against `main` at commit `87c8b11` (v0.5.3).
>
> **Frame:** RantaiClaw is positioned as the *lightest* of three direct competitors (vs OpenClaw and Hermes-agent). This audit measures every bloat finding against that thesis, not against general "best practices."

## Headline numbers

| Metric | Value | Notes |
|---|---|---|
| Total Rust LoC (`src/`) | 152,400 | 41 module subdirs |
| Direct Cargo deps | 132 | Many always-compiled despite niche use |
| Workspace members | 2 | `.` + `crates/robot-kit` |
| Top file | `src/onboard/wizard.rs` | 6,492 LoC, 5+ concerns mashed |
| 2nd file | `src/config/schema.rs` | 6,291 LoC |
| 3rd file | `src/channels/mod.rs` | 5,870 LoC, god module |
| Files >2K LoC | 10 | Unhealthy concentration |
| Firmware projects | 5 | arduino, esp32, esp32-ui, nucleo, uno-q-bridge |
| `cfg(feature)` blocks | 112 | All resolved against the populated `[features]` table |
| Active feature flags | 14 | tui, hardware, channel-matrix, channel-lark, memory-postgres, observability-otel, peripheral-rpi, browser-native, fantoccini (alias), sandbox-landlock (no-op), sandbox-bubblewrap, landlock (alias), probe, rag-pdf, whatsapp-web |

## Top 10 highest-impact findings

### 1. `src/onboard/wizard.rs` — 6,492 LoC, two parallel flows + provider tables + scaffold + tests

`run_wizard` (interactive, lines ~277–407) and `run_quick_setup` (headless, lines ~510–714) duplicate config construction, workspace scaffolding, and telemetry. Same file also hosts `setup_provider`, `curated_models_for_provider` (~500 lines of static lookup tables), `canonical_provider_name`, model-cache logic, `setup_channels`, `setup_hardware`, `setup_memory`, `scaffold_workspace`, plus ~280 lines of unit tests.

**Recommendation:** split into `wizard_flow.rs`, `quick_setup.rs`, `provider_picker.rs`, `scaffold.rs`. Move tests to `tests/onboard_*.rs`.

**Blast radius:** Medium — only `src/main.rs` callers via `Commands::Onboard`/`Commands::Setup`. **Effort:** M.

### 2. `src/channels/mod.rs` — 5,870 LoC god module

Channels dispatch loop, supervised listener, conversation-history management, runtime config hot-reload, and 400 lines of unit tests in one file. Mirrors what `src/providers/mod.rs` already does cleanly (~80-line `mod.rs` with re-exports + factory wiring).

**Recommendation:** extract `runtime.rs`, `history.rs`, move tests to `tests/channel_runtime.rs`. **Blast radius:** Low. **Effort:** M.

### 3. `crates/robot-kit` — separate workspace member with own dep tree

`crates/robot-kit/Cargo.toml` is a standalone crate (robotics, ROS2, GPIO, LIDAR, vision) listing `tokio`, `reqwest`, `serde`, `anyhow`, `chrono`, `directories`, optionally `rppal`. Listed as workspace member at `Cargo.toml:2`, so every `cargo build`/`cargo test --workspace` includes it. Its own commented-out `# rantaiclaw = { path = "../..", optional = true }` confirms zero compile coupling.

**Recommendation:** drop from `members = [...]`; publish externally or move outside workspace. **Blast radius:** Low. **Effort:** XS.

### 4. `firmware/` — 5 sketch projects, zero Cargo coupling

Arduino/ESP32/STM32 C/C++ sketches inflate the checkout, confuse cargo tooling, and signal "Arduino tool" instead of "lightweight Rust agent runtime." Hardware is one feature among many per the product thesis.

**Recommendation:** move entire `firmware/` to `rantaiclaw-firmware` sibling repo or `packages/firmware/` outside the workspace. Only `src/peripherals/arduino_flash.rs` and `src/peripherals/nucleo_flash.rs` reference firmware paths at runtime; those are configurable. **Blast radius:** Low. **Effort:** XS.

### 5. `src/integrations/registry.rs` — 32+ `ComingSoon` entries shipped in binary

Lines 100, 118, 124, 130, 136, 287, 293, 492… 665 — entries like Webchat, Zalo, Nostr, Spotify, Home Assistant with `status_fn: |_| IntegrationStatus::ComingSoon`. Pure roadmap baked into compiled output. Direct YAGNI violation. Test at line 868 asserts three of them stay `ComingSoon`.

**Recommendation:** delete all `ComingSoon` entries and the corresponding test. The roadmap belongs in ClickUp + `docs/project/`, not in compiled code. **Blast radius:** Low (only affects `rantaiclaw integrations list`). **Effort:** XS.

### 6. `probe-rs` optional dep — adds ~50 transitive deps for one STM32 memory-read tool

`Cargo.toml:169` lists `probe-rs = { version = "0.31", optional = true }` with comment "adds ~50 deps." A full on-chip debugging framework (USB HID, CMSIS-DAP, OpenOCD-level device trees, USB stack). Even gated, it advertises a 50-dep optional surface that distracts from the lightweight thesis.

**Recommendation:** remove from `Cargo.toml`. Reimplement `hardware_memory_read` tool as a thin shell wrapper invoking external `probe-rs` CLI. Document in `docs/hardware-peripherals-design.md` that users install probe-rs separately. **Blast radius:** Medium (touches `src/peripherals/nucleo_flash.rs` + `src/tools/hardware_memory_read.rs`). **Effort:** S.

### 7. `schemars` always compiled, only used by one CLI subcommand

`Cargo.toml:46` includes `schemars = "1.2"` non-optional. `#[derive(JsonSchema)]` annotations across `src/config/schema.rs:5` chain. Pulls `dyn-clone`, `indexmap`, proc-macro expansions. Used only for `rantaiclaw config schema` developer aid.

**Recommendation:** feature-gate behind `config-schema`. Wrap derives with `#[cfg_attr(feature = "config-schema", derive(JsonSchema))]`. **Blast radius:** Low. **Effort:** S.

### 8. `wa-rs` chain pulls a third HTTP client

`Cargo.toml:180-186`: `wa-rs`, `wa-rs-core`, `wa-rs-binary`, `wa-rs-proto`, `wa-rs-ureq-http`, `wa-rs-tokio-transport` (6 crates) + `serde-big-array` + `prost` under `whatsapp-web` feature. `wa-rs-ureq-http` brings `ureq` as a third HTTP stack alongside `reqwest` and `axum` internals — undermines the single-HTTP-client discipline the rest of the codebase keeps.

**Recommendation:** keep feature gate; flag `wa-rs-ureq-http` for replacement with a reqwest-based transport once `wa-rs` upstream supports one. **Blast radius:** Low (already gated). **Effort:** XS to flag, S to fix.

### 9. `async-stream` unconditional dep — only used inside `cfg(feature = "whatsapp-web")`

`Cargo.toml:29` comment: "used by whatsapp-web pair_once helper." Confirmed: only callsite is `src/channels/whatsapp_web.rs:543`, inside `#[cfg(feature = "whatsapp-web")] pub fn pair_once(...)`.

**Recommendation:** make optional, gate on `whatsapp-web`:

```toml
async-stream = { version = "0.3", default-features = false, optional = true }
# in [features]:
whatsapp-web = [..., "dep:async-stream"]
```

**Blast radius:** Low. **Effort:** XS.

### 10. `src/rag/` — hardware datasheet RAG always compiled

`src/rag/mod.rs` is hardware-only (datasheet chunking, pin aliases, semantic search for STM32/RPi). Callers are in `src/agent/loop_.rs:254, 1727, 1732, 1734, 2175, 2180, 2182` — NOT just peripherals as initially assumed. Gating it requires conditional compilation of the agent loop too.

**Recommendation:** more invasive than initially scoped. Defer to a planned hardware-isolation pass; for now, just track. **Blast radius:** Medium (requires `cfg` in `agent/loop_.rs`). **Effort:** M.

## Dependency hit list

| Dep | Issue | Action | Effort |
|---|---|---|---|
| `probe-rs` | ~50 transitive deps for one STM32 tool | Remove; shell out to external binary | S |
| `schemars` | Always compiled; only for `config schema` CLI | Feature-gate `config-schema` | S |
| `async-stream` | Only used inside `whatsapp-web` cfg | Make optional, add to whatsapp-web feature | XS |
| `ring` | HMAC-SHA256 only for GLM JWT; `hmac`+`sha2` already in tree | Replace GLM signer (~30 lines), drop `ring` | S |
| `hostname` | Single-function dep; 2 call sites | Replace with `std::env::var("HOSTNAME").or_else(\|_\| std::fs::read_to_string("/etc/hostname"))` | XS |
| `prometheus` | Always compiled; few deploys ship a Prometheus scrape endpoint | Gate behind `observability-prometheus` | S |
| `lettre` + `mail-parser` + `async-imap` | Email channel always compiled despite niche use | Gate behind `channel-email` | S |
| `chrono-tz` | Full IANA tz DB compiled in | Audit if more than UTC + local offset is needed | XS |
| `wa-rs-ureq-http` | Pulls `ureq` as third HTTP client | Replace once upstream supports reqwest transport | S |
| `image` | jpeg+png decoders for screenshots | Audit call sites; if only base64-encoding, replace with raw passthrough | S |

## Module split candidates

| File | Current LoC | Proposed splits |
|---|---|---|
| `src/onboard/wizard.rs` | 6,492 | `wizard_flow.rs`, `quick_setup.rs`, `provider_picker.rs`, `scaffold.rs` |
| `src/channels/mod.rs` | 5,870 | `mod.rs` (re-exports + factory, ~80 lines), `runtime.rs`, `history.rs` |
| `src/config/schema.rs` | 6,291 | `schema/top.rs`, `schema/channels.rs`, `schema/hardware.rs`, `schema/memory.rs`, `schema/network.rs` |
| `src/providers/mod.rs` | 2,563 | Extract alias resolvers (`is_minimax_alias`, `is_glm_alias`, etc.) to `providers/aliases.rs` |

## Niche features to feature-gate

| Surface | Current | Proposed feature |
|---|---|---|
| Email channel (lettre + mail-parser + async-imap) | Always compiled | `channel-email` |
| Prometheus metrics | Always compiled | `observability-prometheus` |
| `schemars` JSON schema export | Always compiled | `config-schema` |
| `src/rag/` hardware datasheet RAG | Always compiled | `hardware` |
| `async-stream` | Always compiled | `whatsapp-web` |
| iMessage channel (rusqlite vs macOS Messages DB) | Always compiled, macOS-only at runtime | `channel-imessage` |

## Move out of main repo

**Move now (zero compile coupling):**

- `firmware/` — 5 sketch projects, no Cargo coupling. Move to sibling repo.
- `crates/robot-kit` — drop from workspace `members`. Standalone crate or external repo.

**Move with minor wiring:**

- `docs/superpowers/specs/` — dated design specs (e.g., `2026-04-27-onboarding-depth-v2-design.md`). Per CLAUDE.md §4.1: snapshots should be immutable in `docs/project/`. Currently in a `superpowers/` path that's not part of the canonical doc IA.

## Items needing a human call

1. `src/channels/imessage.rs` — macOS-only at runtime, always compiled. Linux/Windows builds carry dead code. Feature-gate `channel-imessage` or accept the dead-compile?
2. `src/providers/openai_codex.rs` and `copilot.rs` — non-public OAuth client IDs. The copilot module's own docstring warns GitHub may revoke. Keep, gate behind `provider-copilot`, or remove?
3. `src/rag/mod.rs` vs `src/memory/` — two chunking implementations. Merge or keep separate?
4. `docs/superpowers/` — actively referenced as a working directory; archive or keep?
5. `IntegrationStatus::ComingSoon` registry entries — if `rantaiclaw integrations` is a shipped user-facing CLI, removing entries is a UX regression. If it's internal/hidden, delete.

## Easy wins summary

These six XS-effort items together drop ~3 deps and remove dead surface area, with low blast radius:

1. Drop `crates/robot-kit` from workspace members
2. Make `async-stream` optional and gate on `whatsapp-web`
3. Replace `hostname` crate with std-lib equivalent (3 lines)
4. Drop `ring`; reimplement GLM JWT with `hmac`+`sha2` already present
5. Delete `IntegrationStatus::ComingSoon` registry entries (+ test)
6. Move `firmware/` outside the Rust workspace

Combined effort: ~half a day. Combined effect: visibly leaner Cargo.toml + smaller binary + cleaner module discoverability.

## Methodology

Audit performed in two passes:

1. **Mechanical survey** — `find`/`wc -l`/`grep` over `src/`, `Cargo.toml`, `docs/`, `firmware/`, `crates/` to gather LoC distribution, dep count, feature flags, TODO/dead_code markers.
2. **Focused deep review** by `feature-dev:code-reviewer` agent over 91 tool uses (~5 min wall clock), reading suspect files in detail.

All findings cite specific file paths and (where relevant) line numbers. No web sources used. No code modifications performed during the audit pass.
