# Pillar 1 — Setup and First-Run Experience

> **ClickUp:** [v0.6.0 — Product Completeness Beta (TUI full)](https://app.clickup.com/t/86exgu406) → Feature: TUI → Sub-feature: Setup (8 items) · **Maturity:** Implemented · needs validation · **Modules:** `src/onboard/`, `src/tui/`, `src/doctor/`

The first thing a user sees. RantaiClaw's setup story is **fullscreen TUI wizard + non-interactive headless** — re-runnable, policy-aware, idempotent. One binary, no installer scripts, no Python, no containers.

## What this pillar covers

- First-run wizard (`rantaiclaw setup [full]`)
- Section drill-down (`rantaiclaw setup <section>`)
- Non-interactive bootstrap (`rantaiclaw setup --non-interactive`)
- `rantaiclaw doctor` diagnostics (text / json / brief)
- Profile creation, scaffolding, and per-section re-runs

## What a fresh install sees

`rantaiclaw chat` — and `rantaiclaw` with no subcommand — opens one of three
doors before the chat appears:

| The install | What opens |
|---|---|
| The agent could not be built, and the config stores no provider credential | The first-run wizard: provider, approvals, login, persona, skills, then channels and integrations |
| The agent could not be built, and the config stores a provider credential | Provider setup — the profile was set up and its provider broke |
| The agent built | Nothing; the chat starts |

A provider counts as set up when `default_provider` names one **and** a key
resolves for it. The onboarding provider section and the TUI boot read that one
rule, so the two surfaces cannot disagree about whether a profile is fresh.

A setup surface offers a choice only when it can act on it. `setup approvals`
keeps an approval preset that is already on disk: it names the preset that is
active and points at `rantaiclaw setup approvals --force`, which is what offers
the picker and writes the preset you pick. `--force` regenerates the preset's
policy files — `<profile>/policy/autonomy.toml` and
`<profile>/policy/command_allowlist.toml` — so hand edits to them are lost. A
profile with no preset yet is offered the picker either way.

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| First-run UX | Fullscreen TUI wizard, alt-screen | Shell prompts | TBD |
| Headless mode | `--non-interactive` (CI-safe) | Partial | TBD |
| Re-runnable per section | ✅ 7 sections | Partial | TBD |
| Policy-aware (Manual / Smart / Strict / Off presets) | ✅ | ❌ | TBD |
| Diagnostic suite | `doctor` (text/json/brief) | Partial | TBD |
| Setup time, fresh box → working agent | < 5 min | ~5-15 min | TBD |

## Current state by maturity

_**Not re-read on 2026-09-07.** The truth pass of that date covered pillars 2, 4, 8 and 9
only; these rows still carry whatever date the section they came from carried. Treat them as
unverified rather than current._

| Surface | Maturity |
|---|---|
| Wizard interactive flow | Implemented · needs UX validation |
| Wizard non-interactive | Implemented · stable |
| `setup` section drilldown | Implemented · needs validation |
| `setup` sections (provider / approvals / channels / persona / skills / mcp / login) | Implemented |
| `doctor` text / json / brief | Implemented · stable |
| Hot-reload after wizard | Implemented · needs validation |
| Wizard splash + brand polish | Implemented · stable |

## Architecture

```
rantaiclaw setup
  → src/onboard/wizard.rs        (canonical_section_order + run_setup; interactive + headless)
  → src/onboard/section/         (one SetupSection per section)
      ├── provider.rs
      ├── approvals.rs
      ├── channels.rs
      ├── persona.rs
      ├── skills.rs
      ├── mcp.rs
      └── login.rs
  → writes <profile>/config.toml + autonomy.toml + persona.toml + SYSTEM.md
```

## Trait extension point

- `SetupSection` — `src/onboard/section/mod.rs` — implement + register in `canonical_sections()` to add a new setup section
- `Provider` / `Channel` / `Tool` factories — see Pillar 2/3/5

## CLI / config

```bash
rantaiclaw setup                  # interactive wizard
rantaiclaw setup full             # all 7 sections
rantaiclaw setup provider         # one section
rantaiclaw setup approvals        # pick the approval preset (an existing one is kept unless --force)
rantaiclaw setup --non-interactive  # CI / scripted bootstrap
rantaiclaw doctor                 # full diagnostic
rantaiclaw doctor --brief         # one-line health
rantaiclaw doctor --json          # machine-readable
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Feature: TUI hosts the wizard chrome, hot-reload, slash commands, and the 8-item Setup sub-feature. Each setup section gets its own end-to-end TEST. Resilience: Settings verifies all setup-written files survive a server restart.
