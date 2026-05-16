# Documentation Cleanup Plan — 2026-05-06

> **Status:** proposal, awaiting approval before any file moves or deletes.
>
> **Goal:** make RantaiClaw's docs **state-of-the-art** for a *lightweight* AI agent runtime competing directly with OpenClaw and Hermes-agent. Reduce noise, single-source-of-truth per product pillar, mirror the ClickUp release ladder.

## What's wrong with the docs today

| Problem | Evidence |
|---|---|
| **Sprawling root** | 34 markdown files at `docs/` root — flat dump of guides, references, proposals, niche setups |
| **Superseded planning artifacts in main IA** | `docs/superpowers/` holds 9 dated plan/spec docs totaling **7,618 lines** — superseded by shipped releases v0.5.0–v0.5.3 |
| **Empty scaffold subdirs** | `docs/getting-started/`, `docs/reference/`, `docs/operations/`, `docs/contributing/`, `docs/hardware/`, `docs/tui/` each have only a placeholder `README.md` (`<32 lines`) and no real content |
| **Niche guides bloat root** | `mattermost-setup.md`, `nextcloud-talk-setup.md`, `matrix-e2ee-guide.md`, `zai-glm-setup.md`, `arduino-uno-q-setup.md`, `nucleo-setup.md` all sit at `docs/` root rather than in their domain |
| **Multilingual parity claim is false** | CLAUDE.md §4.1 references `README.zh-CN.md` / `.ja.md` / `.ru.md` — none exist. Doc governance contract is misaligned with reality |
| **Roadmap docs duplicated by ClickUp** | `docs/security-roadmap.md` Phase 1/2/3 are now ClickUp v0.6 / v0.7 / v1.0 release tasks. `docs/tui/feature-inventory.md` similar |
| **No pillar docs** | The product positioning task (`86exe9tdq`) defines 9 pillars but no doc anchors a pillar-level "what does this product do here" view |
| **No competitor framing** | Product thesis is "lightest of three vs OpenClaw + Hermes-agent." Zero docs articulate the parity matrix |

## Headline numbers

| Today | After cleanup |
|---|---|
| 57 markdown files | ~30 files |
| 15,697 total lines | ~7,500 lines (52% reduction) |
| 34 files at `docs/` root | 2 files (`README.md`, `SUMMARY.md`) |
| `docs/superpowers/` 9 files / 7,618 LoC | **Deleted** — content lives in git history + ClickUp |
| Sprawl across topics | One doc per **pillar**, one doc per **runtime contract**, one doc per **operations concern** |

## Proposed structure

```
docs/
├── README.md                (entry hub: pillars + contracts + how-to)
├── SUMMARY.md               (TOC; auto-aligned with structure below)
│
├── start/                   (Fastest path to a working agent)
│   ├── install.md           (merge of install.md + one-click-bootstrap.md)
│   ├── first-run.md         (wizard walkthrough — replaces scattered TUI docs)
│   └── troubleshooting.md
│
├── reference/               (Runtime contracts — what users build against)
│   ├── commands.md          (slimmer; tables + flag reference)
│   ├── config.md            (config keys + defaults; links to schema for full)
│   ├── providers.md         (organized by tier: core / extended / experimental)
│   ├── channels.md          (organized: core / niche / experimental, with feature-flag column)
│   ├── tools.md             (NEW: catalog with approval-flow per tool)
│   └── extending.md         (custom Provider/Channel/Tool/Memory; replaces custom-providers.md)
│
├── pillars/                 (One doc per product pillar — mirrors the [Product] task in ClickUp)
│   ├── 1-setup.md           (Setup + first-run UX; competitor parity vs OpenClaw/Hermes)
│   ├── 2-providers.md       (Provider/model runtime)
│   ├── 3-tools-approvals.md (Tools + approvals + security)
│   ├── 4-skills-mcp.md      (Skills + MCP ecosystem)
│   ├── 5-channels.md        (Multi-channel runtime)
│   ├── 6-memory-profiles.md (Memory + profiles + persistence)
│   ├── 7-gateway-daemon.md  (Gateway + daemon + ops)
│   ├── 8-install-release.md (Install + packaging + release)
│   └── 9-docs-adoption.md   (This system + adoption funnel)
│
├── operations/              (Production / ops)
│   ├── runbook.md           (operations-runbook.md)
│   ├── deployment.md        (network-deployment.md slimmed)
│   ├── proxy.md             (proxy-agent-playbook.md slimmed)
│   └── resource-limits.md
│
├── security/                (Security model + threats)
│   ├── model.md             (merge of agnostic-security.md + frictionless-security.md)
│   ├── audit.md             (audit-logging.md)
│   ├── sandboxing.md
│   └── threats/
│       ├── ssrf.md          (http-request-ssrf-threat-model.md)
│       └── shell.md         (shell-execution-security-note.md)
│
├── hardware/                (Niche but supported — clearly scoped)
│   ├── README.md            (hardware-peripherals-design.md slimmed)
│   ├── boards/
│   │   ├── arduino-uno-q.md
│   │   ├── nucleo.md        (nucleo-setup.md)
│   │   └── esp32.md         (NEW from datasheets)
│   ├── adding-boards.md     (adding-boards-and-tools.md)
│   └── datasheets/          (kept; reference only)
│
├── contributing/
│   ├── pr-workflow.md
│   ├── reviewer-playbook.md
│   ├── release-process.md
│   ├── ci-map.md
│   └── actions-policy.md    (actions-source-policy.md)
│
└── project/                 (Time-bound snapshots, immutable per CLAUDE.md §4.1)
    ├── README.md
    ├── codebase-bloat-audit-2026-05-06.md
    └── docs-cleanup-plan-2026-05-06.md
```

## Action list

### Delete (12 files, 7,820 lines)

| File | Lines | Reason |
|---|---|---|
| `docs/superpowers/plans/2026-04-21-tui-agent-async-bridge.md` | 2,020 | Superseded by v0.2.0 (#29) — content in git + ClickUp |
| `docs/superpowers/plans/2026-05-02-tui-unified-setup.md` | 1,293 | Superseded by v0.5.0 (#37) |
| `docs/superpowers/plans/2026-05-04-all-setup-provisioners-roadmap.md` | 1,120 | Superseded by v0.5.0–v0.5.2 |
| `docs/superpowers/plans/2026-05-03-tui-setup-interactive-picker.md` | 1,028 | Superseded by v0.5.2 (#44) |
| `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md` | 820 | Superseded by v0.5.0 release notes |
| `docs/superpowers/specs/2026-04-16-tui-design.md` | 385 | Shipped in v0.2–v0.3 |
| `docs/superpowers/specs/2026-04-21-tui-agent-async-bridge-design.md` | 368 | Shipped in v0.2.0 |
| `docs/superpowers/specs/2026-04-25-installer-ux-upgrade-design.md` | 343 | Shipped in v0.4.1–v0.4.2 |
| `docs/superpowers/specs/2026-04-29-setup-audit.md` | 241 | Shipped in v0.5.2 (#44) |
| `docs/docs-inventory.md` | 103 | Replaced by `SUMMARY.md` |
| `docs/security-roadmap.md` | 185 | Roadmap now lives in ClickUp v0.6 / v0.7 / v1.0 |
| `docs/tui/feature-inventory.md` | 183 | Feature tracking now in ClickUp |

### Merge (10 files → 4 files, ~700 LoC saved through dedup)

| Source files | Target | Why |
|---|---|---|
| `install.md` (400) + `one-click-bootstrap.md` (159) | `start/install.md` | Single install story; one-liner + full options |
| `agnostic-security.md` (353) + `frictionless-security.md` (317) | `security/model.md` | Two halves of the same security narrative |
| `mattermost-setup.md` (63) + `nextcloud-talk-setup.md` (78) + `matrix-e2ee-guide.md` (141) | absorbed into `reference/channels.md` | Per-channel sections; no need for separate files |
| `zai-glm-setup.md` (142) + (future per-provider snippets) | absorbed into `reference/providers.md` | Per-provider sections |

### Move (most files keep content; just relocate)

| Current | New location | Notes |
|---|---|---|
| `commands-reference.md` | `reference/commands.md` | + slim by 30%: tables instead of prose |
| `config-reference.md` | `reference/config.md` | + slim by 40%: link to schema for exhaustive |
| `providers-reference.md` | `reference/providers.md` | + reorganize by tier |
| `channels-reference.md` | `reference/channels.md` | + reorganize core/niche/experimental |
| `custom-providers.md` | `reference/extending.md` | + cover all 4 trait extension points |
| `troubleshooting.md` | `start/troubleshooting.md` | |
| `operations-runbook.md` | `operations/runbook.md` | |
| `network-deployment.md` | `operations/deployment.md` | |
| `proxy-agent-playbook.md` | `operations/proxy.md` | |
| `resource-limits.md` | `operations/resource-limits.md` | |
| `audit-logging.md` | `security/audit.md` | |
| `sandboxing.md` | `security/sandboxing.md` | |
| `security/http-request-ssrf-threat-model.md` | `security/threats/ssrf.md` | |
| `security/shell-execution-security-note.md` | `security/threats/shell.md` | |
| `hardware-peripherals-design.md` | `hardware/README.md` | + slim |
| `arduino-uno-q-setup.md` | `hardware/boards/arduino-uno-q.md` | |
| `nucleo-setup.md` | `hardware/boards/nucleo.md` | |
| `adding-boards-and-tools.md` | `hardware/adding-boards.md` | |
| `datasheets/*` | `hardware/datasheets/*` | |
| `pr-workflow.md` | `contributing/pr-workflow.md` | |
| `reviewer-playbook.md` | `contributing/reviewer-playbook.md` | |
| `release-process.md` | `contributing/release-process.md` | |
| `ci-map.md` | `contributing/ci-map.md` | |
| `actions-source-policy.md` | `contributing/actions-policy.md` | |

### Create (10 new files, mostly small)

| File | Size estimate | Source |
|---|---|---|
| `docs/README.md` (rewrite) | 80 lines | Entry hub linking pillars + contracts + how-to |
| `pillars/1-setup.md` | 150 lines | Distill from existing TUI/setup docs + ClickUp v0.5/v0.6 scope |
| `pillars/2-providers.md` | 150 lines | From CHANGELOG + providers-reference + ClickUp |
| `pillars/3-tools-approvals.md` | 150 lines | From security/audit/policy docs |
| `pillars/4-skills-mcp.md` | 120 lines | From v0.5.0 release notes |
| `pillars/5-channels.md` | 150 lines | From channels-reference + per-channel guides |
| `pillars/6-memory-profiles.md` | 100 lines | From v0.5.0 (Wave 1) |
| `pillars/7-gateway-daemon.md` | 100 lines | From operations + v0.5.0 (Wave 4B) |
| `pillars/8-install-release.md` | 100 lines | From release-process + install |
| `pillars/9-docs-adoption.md` | 80 lines | Meta-doc; how docs map to ClickUp |
| `reference/tools.md` (new) | 200 lines | Tool catalog with approval flow per tool |

### Each pillar doc has the same shape (state-of-the-art template)

```markdown
# Pillar N: <Name>

> **ClickUp:** [<release task>](url) · **Maturity:** <bucket> · **Surface area:** <files/modules>

## What this pillar covers
<2 sentences>

## Vs OpenClaw / Hermes-agent
| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Surface X | ✅ | ✅ | ✅ |
| Surface Y | ✅ Lighter | ✅ | ❌ |

## Current state (per maturity rule)
- **Implemented:** ...
- **Needs validation:** ...
- **Needs UX polish:** ...
- **Needs docs:** ...
- **Stable:** ...

## Architecture (1 mermaid diagram)
<diagram>

## Trait extension point
<link to src/.../traits.rs>

## Relevant CLI / config
<minimal table>

## Roadmap
<links to ClickUp release tasks under this pillar>
```

This shape gives readers — including AI agents reviewing the codebase — a deterministic answer to: "where does X live? what's its maturity? how does it compare to competitors? where's the trait?"

## Net effect

| Metric | Before | After | Change |
|---|---|---|---|
| Total `.md` files | 57 | ~30 | -47% |
| Total LoC in docs | 15,697 | ~7,500 | -52% |
| Files at `docs/` root | 34 | 2 | -94% |
| `docs/superpowers/` | 9 files, 7,618 LoC | **gone** | -100% |
| Single-source-of-truth per pillar | 0 | 9 | new |
| Competitor-parity framing | 0 docs | every pillar doc | new |
| ClickUp linkage | minimal | every pillar + every release note | new |

## Risks

1. **Loss of design history** — the `docs/superpowers/` plans are detailed. Mitigation: they remain in git history (`git log -- docs/superpowers/`) and the *outcome* of each plan is in the corresponding ClickUp release task description + CHANGELOG.md.
2. **Broken inbound links** — anyone who linked to a `docs/foo.md` URL externally will hit a 404 after moves. Mitigation: keep redirect stubs at the old paths for one release cycle (`docs/install.md` → 1-line "moved to start/install.md").
3. **CLAUDE.md §4.1 doc governance contract** — references multilingual parity that doesn't exist today. Either implement (heavy) or amend the contract to match reality. Recommend: amend.

## Execution order

1. **Phase A (this PR)** — create new structure scaffolds, write new pillar docs, write new merged docs.
2. **Phase B (next PR)** — move existing files into new locations with redirect stubs.
3. **Phase C (one release later)** — delete `docs/superpowers/`, `docs/security-roadmap.md`, `docs/docs-inventory.md`, `docs/tui/feature-inventory.md`. Remove redirect stubs. Update CLAUDE.md §4.1 to match new IA.

## Decisions needed before execution

1. ✅/❌ Delete `docs/superpowers/` outright (vs archiving in `docs/project/archive/superpowers/`)?
2. ✅/❌ Delete `security-roadmap.md` (vs keeping a slim "see ClickUp" pointer)?
3. ✅/❌ Drop multilingual parity claim from CLAUDE.md §4.1 (vs committing to translate the lean doc set)?
4. ✅/❌ Single-PR merge of Phase A + B (faster but bigger blast) vs split (safer but two churn cycles)?
5. ✅/❌ Restructure now (before v0.6.0 cut) or after v0.6.0 ships (less in-flight churn)?
