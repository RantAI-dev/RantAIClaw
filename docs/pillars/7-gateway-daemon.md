# Pillar 7 — Gateway, Daemon, and Operations

> **ClickUp:** [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) → Resilience: Profile + sessions, Resilience: Audit log · **Maturity:** Stable · **Modules:** `src/gateway/`, `src/daemon/`, `src/service/`, `src/observability/`

Day-2 operations. The gateway lets you reconfigure a running agent over HTTP. The daemon keeps it alive. Observability (logging, metrics, traces) keeps it accountable.

## What this pillar covers

- Live config API (PATCH `/config/<key>`)
- Daemon lifecycle (`service install / start / stop / restart / status / uninstall`)
- Per-platform service unit (systemd / launchd / Windows Service)
- Health endpoint (`/health`)
- Metrics (Prometheus, gated)
- Distributed tracing (OpenTelemetry, gated)
- Heartbeat + cron + scheduled tasks
- Tunnel for remote-access (e.g., Cloudflare / ngrok-style)

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Live config API | ✅ PATCH any key | TBD | TBD |
| Daemon installer (3 OSes) | ✅ | TBD | TBD |
| Health endpoint | ✅ `/health` | TBD | TBD |
| Prometheus metrics | ✅ (gated) | TBD | TBD |
| OpenTelemetry traces | ✅ (gated) | TBD | TBD |
| Cron + delayed tasks | ✅ 5-field cron + RFC3339 + interval + delay | TBD | TBD |
| Daemon hot-reload on profile switch | ✅ sentinel-file flow | TBD | TBD |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Gateway HTTP API | Stable |
| Live config PATCH | Stable |
| Daemon lifecycle | Stable |
| Health endpoint | Stable |
| Prometheus metrics | Stable — one registry per process, fed by the gateway, channels, cron and the heartbeat worker (always-compiled today; planned feature gate) |
| OpenTelemetry | Feature-gated (`--features observability-otel`) |
| Cron / scheduled tasks | Stable |
| Tunnel | Implemented · needs validation |
| Heartbeat | Stable |

## Architecture

```
              ┌────────────┐
HTTP / WS ──▶ │  gateway   │ ──┐
              └────────────┘   │
                               │  PATCH config / WS events
                               ▼
              ┌──────────────────────┐
              │       daemon         │ ──▶ supervises agent + channels
              └──────────────────────┘
                               │
                               ▼
              ┌──────────────────────┐
              │   observability      │ → Prometheus / OTLP / file logs
              └──────────────────────┘
```

## Trait extension point

- `RuntimeAdapter` — `src/runtime/traits.rs` (currently: native)
- `Observer` — `src/observability/traits.rs`

## CLI / config

```bash
rantaiclaw service install
rantaiclaw service start | stop | restart | status | uninstall

rantaiclaw cron add '0 9 * * 1-5' 'Good morning' --tz America/New_York
rantaiclaw cron add-at 2026-12-31T23:59:00Z 'Happy New Year'
rantaiclaw cron once 30m 'Run backup'
rantaiclaw cron pause <id> | resume <id> | remove <id>
```

```toml
[gateway]
host = "127.0.0.1"
port = 8080
require_pairing = true

[observability]
backend = "prometheus"          # "none" | "log" | "prometheus" | "otel"
otel_endpoint = "http://localhost:4318"   # backend = "otel" only
otel_service_name = "rantaiclaw"
```

Every key above exists. The block this doc carried until schema v29 did not:
`[gateway] bind` / `auth_token` and `[observability] log_dir` / `prometheus` are
not fields of anything, so a config written from it loaded as defaults and warned
`unknown config key`.

Live config:

```bash
curl -X PATCH http://localhost:8080/config/model \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"provider": "anthropic", "model": "claude-sonnet-4-6"}'
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Resilience: Profile + sessions and Resilience: Audit log verify the daemon side of state persistence across server restart
