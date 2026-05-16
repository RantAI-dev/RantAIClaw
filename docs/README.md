# RantaiClaw — Documentation

A lightweight, complete, Rust-native AI agent runtime. Direct competitor to OpenClaw and Hermes-agent — positioned as the *lightest* of the three.

> Last refreshed: **2026-05-06** · See [`SUMMARY.md`](SUMMARY.md) for the full TOC · See [project snapshots](project/) for date-stamped state.

## 30-second decision tree

| You want to… | Read |
|---|---|
| Install and run RantaiClaw quickly | [start/install.md](start/install.md) · or root [README](../README.md#quick-start) |
| Understand what the product covers | [pillars/](pillars/) — one doc per surface |
| Look up a CLI command | [reference/commands.md](commands-reference.md) |
| Look up a config key | [reference/config.md](config-reference.md) |
| See provider / model coverage | [reference/providers.md](providers-reference.md) |
| See channel / transport coverage | [reference/channels.md](channels-reference.md) |
| Run a production deployment | [operations/runbook.md](operations-runbook.md) |
| Understand the security model | [security/model.md](security/model.md) (forthcoming · for now: [agnostic-security.md](agnostic-security.md) + [frictionless-security.md](frictionless-security.md)) |
| Add a board / peripheral | [hardware/](hardware/) |
| Contribute a PR | [contributing/](contributing/) |
| See what's planned and shipped | [pillars/](pillars/) (per-pillar maturity table) · [ClickUp](https://app.clickup.com/t/86exe9tdq) |

## The 9 pillars

Each pillar is one product surface. Every pillar doc has the same shape: ClickUp release link · maturity bucket · vs-OpenClaw / vs-Hermes parity matrix · architecture diagram · trait extension point · CLI/config · roadmap.

1. [Setup and First-Run Experience](pillars/1-setup.md) — wizard, doctor, profiles, migration
2. [Provider and Model Runtime](pillars/2-providers.md) — 15+ native adapters · OpenRouter aggregator · streaming
3. [Tools, Approvals, and Security](pillars/3-tools-approvals.md) — approval gate · sandboxing · audit
4. [Skills and MCP Ecosystem](pillars/4-skills-mcp.md) — bundled starter pack · ClawHub · MCP curated picker
5. [Multi-Channel Runtime](pillars/5-channels.md) — 13+ channels with hot add/remove
6. [Memory, Profiles, and Persistence](pillars/6-memory-profiles.md) — multi-profile · pluggable backends
7. [Gateway, Daemon, and Operations](pillars/7-gateway-daemon.md) — live config API · daemon · observability
8. [Install, Packaging, and Release](pillars/8-install-release.md) — one-line installer · multi-target matrix
9. [Documentation and Adoption](pillars/9-docs-adoption.md) — this system

## Reference (runtime contracts)

These tracks behavior. Every PR that affects a CLI flag or config key updates the corresponding doc.

- [commands-reference.md](commands-reference.md)
- [config-reference.md](config-reference.md)
- [providers-reference.md](providers-reference.md)
- [channels-reference.md](channels-reference.md)
- [custom-providers.md](custom-providers.md)

## Operations

- [operations-runbook.md](operations-runbook.md) — day-2 runtime operations
- [troubleshooting.md](troubleshooting.md)
- [network-deployment.md](network-deployment.md)
- [proxy-agent-playbook.md](proxy-agent-playbook.md)
- [resource-limits.md](resource-limits.md)
- [release-process.md](release-process.md)

## Security

- [agnostic-security.md](agnostic-security.md) (will merge into `security/model.md`)
- [frictionless-security.md](frictionless-security.md) (will merge into `security/model.md`)
- [sandboxing.md](sandboxing.md)
- [audit-logging.md](audit-logging.md)
- [security/http-request-ssrf-threat-model.md](security/http-request-ssrf-threat-model.md)
- [security/shell-execution-security-note.md](security/shell-execution-security-note.md)
- Roadmap: now tracked in ClickUp v0.6.0 release task (see [pillar 3](pillars/3-tools-approvals.md))

## Hardware (niche but supported)

- [hardware-peripherals-design.md](hardware-peripherals-design.md)
- [adding-boards-and-tools.md](adding-boards-and-tools.md)
- [arduino-uno-q-setup.md](arduino-uno-q-setup.md)
- [nucleo-setup.md](nucleo-setup.md)
- [datasheets/](datasheets/)

## Contributing

- [pr-workflow.md](pr-workflow.md)
- [reviewer-playbook.md](reviewer-playbook.md)
- [ci-map.md](ci-map.md)
- [actions-source-policy.md](actions-source-policy.md)

## Project (conventions + archive)

- [project/README.md](project/README.md)
- [project/operating-conventions.md](project/operating-conventions.md) — how alpha cuts ship
- [project/archive/](project/archive/) — superseded plans + snapshot audits, kept for design-rationale history

## Documentation governance

- Project snapshots are date-stamped and immutable.
- Runtime-contract references must track behavior changes.
- Pillar docs link to ClickUp release tasks; pillar maturity tables update when releases ship.
- Plans / specs that have shipped are **archived** under `project/archive/<topic>/`, not deleted.
- This is an English-only doc system today. (Multilingual mirrors were claimed but never implemented; we will not promise parity that doesn't exist.)
