# RantaiClaw Docs — Unified TOC

> Canonical table of contents. English-only today. See [`README.md`](README.md) for the entry hub and 30-second decision tree.

## 0) Project entry

- Root README: [../README.md](../README.md)
- Docs hub: [README.md](README.md)
- This TOC: [SUMMARY.md](SUMMARY.md)

## 1) Pillars (one doc per product surface)

- [1 — Setup and First-Run Experience](pillars/1-setup.md)
- [2 — Provider and Model Runtime](pillars/2-providers.md)
- [3 — Tools, Approvals, and Security](pillars/3-tools-approvals.md)
- [4 — Skills and MCP Ecosystem](pillars/4-skills-mcp.md)
- [5 — Multi-Channel Runtime](pillars/5-channels.md)
- [6 — Memory, Profiles, and Persistence](pillars/6-memory-profiles.md)
- [7 — Gateway, Daemon, and Operations](pillars/7-gateway-daemon.md)
- [8 — Install, Packaging, and Release](pillars/8-install-release.md)
- [9 — Documentation and Adoption](pillars/9-docs-adoption.md)

## 2) Getting started

- [install.md](install.md) (will merge with [one-click-bootstrap.md](one-click-bootstrap.md) into `start/install.md`)
- [troubleshooting.md](troubleshooting.md)

## 3) Reference (runtime contracts)

- [commands-reference.md](commands-reference.md)
- [config-reference.md](config-reference.md)
- [providers-reference.md](providers-reference.md)
- [channels-reference.md](channels-reference.md)
- [custom-providers.md](custom-providers.md)

## 4) Operations & deployment

- [operations-runbook.md](operations-runbook.md)
- [network-deployment.md](network-deployment.md)
- [proxy-agent-playbook.md](proxy-agent-playbook.md)
- [resource-limits.md](resource-limits.md)
- [release-process.md](release-process.md)
- [matrix-e2ee-guide.md](matrix-e2ee-guide.md)
- [mattermost-setup.md](mattermost-setup.md)
- [nextcloud-talk-setup.md](nextcloud-talk-setup.md)
- [zai-glm-setup.md](zai-glm-setup.md)
- [langgraph-integration.md](langgraph-integration.md)

## 5) Security

- [agnostic-security.md](agnostic-security.md)
- [frictionless-security.md](frictionless-security.md)
- [sandboxing.md](sandboxing.md)
- [audit-logging.md](audit-logging.md)
- [security/http-request-ssrf-threat-model.md](security/http-request-ssrf-threat-model.md)
- [security/shell-execution-security-note.md](security/shell-execution-security-note.md)
- Roadmap: tracked in the ClickUp v0.6.0 release task · pointer in [pillar 3](pillars/3-tools-approvals.md)

## 6) Hardware (niche but supported)

- [hardware-peripherals-design.md](hardware-peripherals-design.md)
- [adding-boards-and-tools.md](adding-boards-and-tools.md)
- [arduino-uno-q-setup.md](arduino-uno-q-setup.md)
- [nucleo-setup.md](nucleo-setup.md)
- [datasheets/arduino-uno-q.md](datasheets/arduino-uno-q.md)
- [datasheets/arduino-uno.md](datasheets/arduino-uno.md)
- [datasheets/nucleo-f401re.md](datasheets/nucleo-f401re.md)
- [datasheets/esp32.md](datasheets/esp32.md)

## 7) Contributing & CI

- [../CONTRIBUTING.md](../CONTRIBUTING.md)
- [pr-workflow.md](pr-workflow.md)
- [reviewer-playbook.md](reviewer-playbook.md)
- [ci-map.md](ci-map.md)
- [actions-source-policy.md](actions-source-policy.md)

## 8) Project — snapshots, immutable

- [project/README.md](project/README.md)
- [project/codebase-bloat-audit-2026-05-06.md](project/codebase-bloat-audit-2026-05-06.md)
- [project/docs-cleanup-plan-2026-05-06.md](project/docs-cleanup-plan-2026-05-06.md)
- [project/archive/superpowers/](project/archive/superpowers/) — superseded design plans/specs (2026-04-16 → 2026-05-04)

## 9) Forthcoming structure (Phase B will move existing files here)

```
docs/
├── start/         install · first-run · troubleshooting
├── reference/     commands · config · providers · channels · tools · extending
├── pillars/       1..9 (already here)
├── operations/    runbook · deployment · proxy · resource-limits
├── security/      model · audit · sandboxing · threats/
├── hardware/      README · boards/ · adding-boards · datasheets/
├── contributing/  pr-workflow · reviewer · release · ci · actions-policy
└── project/       README · snapshots · archive/
```
