# Pillar 1 — Setup and First-Run Experience

> **ClickUp:** [v0.6.0 — Product Completeness Beta (TUI full)](https://app.clickup.com/t/86exgu406) → Feature: TUI → Sub-feature: Setup (8 items) · **Maturity:** Implemented · needs validation · **Modules:** `src/onboard/`, `src/tui/`, `src/doctor/`

The first thing a user sees. RantaiClaw's setup story is **fullscreen TUI wizard + non-interactive headless** — re-runnable, policy-aware, idempotent. One binary, no installer scripts, no Python, no containers.

## What this pillar covers

- First-run wizard (`rantaiclaw setup [full]`)
- Section drill-down (`rantaiclaw setup <section>`)
- Non-interactive bootstrap (`rantaiclaw setup --non-interactive`)
- `rantaiclaw doctor` diagnostics (text / json / brief)
- Profile creation, scaffolding, and migration from OpenClaw / ZeroClaw

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| First-run UX | Fullscreen TUI wizard, alt-screen | Shell prompts | TBD |
| Headless mode | `--non-interactive` (CI-safe) | Partial | TBD |
| Re-runnable per section | ✅ 6 sections | Partial | TBD |
| Policy-aware (L1-L4) | ✅ | ❌ | TBD |
| Diagnostic suite | `doctor` (text/json/brief) | Partial | TBD |
| Setup time, fresh box → working agent | < 5 min | ~5-15 min | TBD |
| Migration import (OpenClaw / ZeroClaw) | ✅ `rantaiclaw migrate` | n/a | n/a |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Wizard interactive flow | Implemented · needs UX validation |
| Wizard non-interactive | Implemented · stable |
| `setup` section drilldown | Implemented · needs validation |
| `setup` provisioners (provider / approvals / channels / persona / skills / mcp) | Implemented |
| `doctor` text / json / brief | Implemented · stable |
| Hot-reload after wizard | Implemented · needs validation |
| OpenClaw / ZeroClaw migration | Implemented · needs validation |
| Wizard splash + brand polish | Implemented · stable |

## Architecture

```
rantaiclaw setup
  → src/onboard/wizard.rs        (interactive)
  → src/onboard/quick_setup.rs   (headless; planned split)
  → src/onboard/provisioners/    (one per section)
      ├── provider.rs
      ├── approvals.rs
      ├── channels.rs
      ├── persona.rs
      ├── skills.rs
      └── mcp.rs
  → writes <profile>/config.toml + autonomy.toml + persona.toml + SYSTEM.md
```

## Trait extension point

- `TuiProvisioner` — `src/onboard/provisioners/traits.rs` — implement to add a new setup section
- `Provider` / `Channel` / `Tool` factories — see Pillar 2/3/5

## CLI / config

```bash
rantaiclaw setup                  # interactive wizard
rantaiclaw setup full             # all 6 sections
rantaiclaw setup provider         # one section
rantaiclaw setup --non-interactive  # CI / scripted bootstrap
rantaiclaw doctor                 # full diagnostic
rantaiclaw doctor --brief         # one-line health
rantaiclaw doctor --json          # machine-readable
rantaiclaw migrate --from openclaw  # import legacy install
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Feature: TUI hosts the wizard chrome, hot-reload, slash commands, and the 8-item Setup sub-feature. Each setup section gets its own end-to-end TEST. Resilience: Settings verifies all setup-written files survive a server restart.
