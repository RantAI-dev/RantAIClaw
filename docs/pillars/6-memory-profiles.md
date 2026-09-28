# Pillar 6 — Memory, Profiles, and Persistence

> **ClickUp:** [v0.5.0 Wave 1 shipped](https://app.clickup.com/t/86exgrp1n) · **Maturity:** Stable · **Modules:** `src/memory/`, `src/profile/`, `src/sessions/`

State that survives restarts. Multi-profile workspace layout, pluggable memory backends, embeddings-aware retrieval, and session continuity.

## What this pillar covers

- Multi-profile storage (`~/.rantaiclaw/profiles/<name>/`)
- Memory backends: sqlite (default)
- Embeddings + vector merge for retrieval
- Session auto-titling from first user message
- Profile lifecycle: list / create / use / clone / delete / current
- Daemon handoff on profile switch (drain + relaunch)

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Multi-profile layout | ✅ | ❌ (single layout) | TBD |
| Pluggable backends | sqlite | TBD | TBD |
| Embeddings + vector merge | ✅ | TBD | TBD |
| Session auto-titling | ✅ | TBD | TBD |
| Daemon handoff on profile switch | ✅ sentinel-file flow | TBD | TBD |

## Current state by maturity

_**Not re-read on 2026-09-07.** The truth pass of that date covered pillars 2, 4, 8 and 9
only; these rows still carry whatever date the section they came from carried. Treat them as
unverified rather than current._

| Surface | Maturity |
|---|---|
| Profile system | Stable (v0.5.0 Wave 1) |
| SQLite backend | Stable |
| Embeddings + vector merge | Stable |
| Session auto-titling | Stable |
| Daemon handoff on profile switch | Stable (v0.5.0 Wave 4B) |
| Compat-symlinks for v0.4.x flat layout | Stable for ≥1 release per v0.5.0 |

## Architecture

```
~/.rantaiclaw/
├── profiles/
│   ├── default/
│   │   ├── config.toml
│   │   ├── workspace/
│   │   ├── memory/         ← sqlite
│   │   ├── audit/
│   │   ├── persona.toml
│   │   ├── SYSTEM.md
│   │   └── skills/
│   └── work/
│       └── ...
├── active_profile          ← name of profile in use
└── .secret_key             ← shared secret store

src/memory/
├── traits.rs               ← Memory trait
├── mod.rs                  ← factory
├── sqlite.rs
├── embeddings.rs
└── chunker.rs              ← shared chunker (also used by RAG)

src/profile/                ← lifecycle commands
src/sessions/               ← per-conversation state
```

## Trait extension point

- `Memory` — `src/memory/traits.rs`
- Register backend in `src/memory/mod.rs` factory
- Tests: roundtrip + concurrent-write safety

## CLI / config

```bash
rantaiclaw profile list
rantaiclaw profile create <name>
rantaiclaw profile use <name>
rantaiclaw profile clone <src> <dst>
rantaiclaw profile current

rantaiclaw memory list --category core
rantaiclaw memory get <key>
rantaiclaw memory stats
rantaiclaw memory clear --category daily
```

```toml
# Precedence: --profile flag > RANTAICLAW_PROFILE env > active_profile file > "default"
[memory]
backend = "sqlite"   # sqlite | none
embeddings = true
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Setup: Memory validates backend picker (sqlite / none). Resilience: Profile + sessions confirms active profile + history resume; Resilience: Settings confirms `config.toml` / `autonomy.toml` / `persona.toml` / `.secret_key` reload identically.
