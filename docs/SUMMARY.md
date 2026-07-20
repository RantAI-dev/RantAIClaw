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

- [start/install.md](start/install.md) (will merge with [start/one-click-bootstrap.md](start/one-click-bootstrap.md) into `start/install.md`)
- [start/troubleshooting.md](start/troubleshooting.md)

## 3) Reference (runtime contracts)

- [reference/commands.md](reference/commands.md)
- [reference/config.md](reference/config.md)
- [reference/providers.md](reference/providers.md)
- [reference/channels.md](reference/channels.md)
- [reference/custom-providers.md](reference/custom-providers.md)
- [reference/kb.md](reference/kb.md) — Knowledge Base (feature-gated)
- [reference/kb-bench.md](reference/kb-bench.md) — KB retrieval latency baseline
- [reference/kb-tuning.md](reference/kb-tuning.md) — KB retrieval quality knobs
- [reference/api-v1.md](reference/api-v1.md) — `/api/v1` HTTP API full contract
- [reference/api-v1-streaming.md](reference/api-v1-streaming.md) — `/api/v1/agent/chat` SSE streaming

## 4) Operations & deployment

- [operations/runbook.md](operations/runbook.md)
- [operations/network-deployment.md](operations/network-deployment.md)
- [operations/proxy-agent-playbook.md](operations/proxy-agent-playbook.md)
- [operations/resource-limits.md](operations/resource-limits.md)
- [contributing/release-process.md](contributing/release-process.md)
- [reference/matrix-e2ee-guide.md](reference/matrix-e2ee-guide.md)
- [reference/mattermost-setup.md](reference/mattermost-setup.md)
- [reference/nextcloud-talk-setup.md](reference/nextcloud-talk-setup.md)
- [reference/zai-glm-setup.md](reference/zai-glm-setup.md)
- [reference/langgraph-integration.md](reference/langgraph-integration.md)

## 5) Security

- [security/agnostic-security.md](security/agnostic-security.md)
- [security/frictionless-security.md](security/frictionless-security.md)
- [security/sandboxing.md](security/sandboxing.md)
- [security/audit-logging.md](security/audit-logging.md)
- [security/http-request-ssrf-threat-model.md](security/http-request-ssrf-threat-model.md)
- [security/shell-execution-security-note.md](security/shell-execution-security-note.md)
- Roadmap: tracked in the ClickUp v0.6.0 release task · pointer in [pillar 3](pillars/3-tools-approvals.md)

## 6) Hardware (niche but supported)

- [hardware/peripherals-design.md](hardware/peripherals-design.md)
- [hardware/adding-boards-and-tools.md](hardware/adding-boards-and-tools.md)
- [hardware/arduino-uno-q-setup.md](hardware/arduino-uno-q-setup.md)
- [hardware/nucleo-setup.md](hardware/nucleo-setup.md)
- [datasheets/arduino-uno-q.md](datasheets/arduino-uno-q.md)
- [datasheets/arduino-uno.md](datasheets/arduino-uno.md)
- [datasheets/nucleo-f401re.md](datasheets/nucleo-f401re.md)
- [datasheets/esp32.md](datasheets/esp32.md)

## 7) Contributing & CI

- [../CONTRIBUTING.md](../CONTRIBUTING.md)
- [contributing/pr-workflow.md](contributing/pr-workflow.md)
- [contributing/reviewer-playbook.md](contributing/reviewer-playbook.md)
- [contributing/ci-map.md](contributing/ci-map.md)
- [contributing/actions-source-policy.md](contributing/actions-source-policy.md)

## 8) Project — conventions + archived snapshots

- [project/README.md](project/README.md)
- [project/operating-conventions.md](project/operating-conventions.md) — how alpha cuts ship (build target, validation, ship sequence, anti-patterns)
- [project/archive/](project/archive/) — superseded plans + snapshot audits (kept for design-rationale history)
