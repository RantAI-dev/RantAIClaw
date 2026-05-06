# Pillar 6 — Memory, Profiles, and Persistence

> **ClickUp:** [v0.5.0 Wave 1 shipped](https://app.clickup.com/t/86exgrp1n) · **Maturity:** Stable · **Modules:** `src/memory/`, `src/profile/`, `src/sessions/`

State that survives restarts. Multi-profile workspace layout, pluggable memory backends, embeddings-aware retrieval, and session continuity.

## What this pillar covers

- Multi-profile storage (`~/.rantaiclaw/profiles/<name>/`)
- Memory backends: markdown (default) · sqlite · postgres (feature-gated)
- Embeddings + vector merge for retrieval
- Session auto-titling from first user message
- Profile lifecycle: list / create / use / clone / delete / current
- Daemon handoff on profile switch (drain + relaunch)
- Migration import from OpenClaw / ZeroClaw

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Multi-profile layout | ✅ | ❌ (single layout) | TBD |
| Pluggable backends | markdown · sqlite · postgres | TBD | TBD |
| Embeddings + vector merge | ✅ | TBD | TBD |
| Session auto-titling | ✅ | TBD | TBD |
| Daemon handoff on profile switch | ✅ sentinel-file flow | TBD | TBD |
| OpenClaw import | ✅ `rantaiclaw migrate` | n/a | n/a |
| ZeroClaw import | ✅ | n/a | n/a |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Profile system | Stable (v0.5.0 Wave 1) |
| Markdown backend | Stable |
| SQLite backend | Stable |
| Postgres backend | Feature-gated (`--features memory-postgres`) |
| Embeddings + vector merge | Stable |
| Session auto-titling | Stable |
| Daemon handoff on profile switch | Stable (v0.5.0 Wave 4B) |
| OpenClaw / ZeroClaw migration | Stable (v0.5.0 Wave 4C) |
| Compat-symlinks for v0.4.x flat layout | Stable for ≥1 release per v0.5.0 |

## Architecture

```
~/.rantaiclaw/
├── profiles/
│   ├── default/
│   │   ├── config.toml
│   │   ├── workspace/
│   │   ├── memory/         ← markdown or sqlite
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
├── markdown.rs / sqlite.rs / postgres.rs
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
backend = "sqlite"   # markdown | sqlite | postgres
embeddings = true
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Setup: Memory validates backend picker (markdown / sqlite / postgres). Resilience: Profile + sessions confirms active profile + history resume; Resilience: Settings confirms `config.toml` / `autonomy.toml` / `persona.toml` / `.secret_key` reload identically.
