# Pillar 4 — Skills and MCP Ecosystem

> **ClickUp:** [v0.5.0 Wave 2D/2E shipped](https://app.clickup.com/t/86exgrp1n) · **Maturity:** Stable · **Modules:** `src/skills/`, `src/mcp/`, `src/skillforge/`

Composable, reusable agent capabilities. Skills are markdown bundles with tool wiring; MCP servers extend the tool surface to the broader Model Context Protocol ecosystem.

## What this pillar covers

- 5-skill bundled starter pack (web-search, scheduler-reminders, summarizer, research-assistant, meeting-notes)
- ClawHub multi-select skill picker (sorted by stars, SHA-256 verified install)
- 8 MCP servers curated picker — 2 zero-auth + 6 authenticated, with inline auth (filesystem dropped in v0.6.51 — see below)
- Spawn-and-validate at setup time (zero-auth servers)
- SkillForge — skill authoring helper

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Bundled skill starter pack | ✅ 5 skills | TBD | TBD |
| ClawHub remote install | ✅ SHA-256 verified | n/a | n/a |
| MCP curated picker | ✅ 9 servers | TBD | TBD |
| Setup-time MCP validation | ✅ spawn-and-wait | TBD | TBD |
| Skill authoring helper | ✅ `skillforge` | TBD | TBD |

## Current state by maturity

| Surface | Maturity |
|---|---|
| Skills runtime | Stable |
| Starter pack | Stable |
| ClawHub install + verify | Stable (fixed in v0.5.2) |
| MCP curated picker | Stable |
| MCP zero-auth setup-time validation | Stable (added v0.5.2) |
| Auth flow per MCP server | Stable |
| `skillforge` authoring helper | Implemented · needs UX polish |

## Architecture

```
~/.rantaiclaw/profiles/<name>/skills/
  ├── web-search/
  │   ├── SKILL.md
  │   └── tool definitions
  └── ...

~/.rantaiclaw/profiles/<name>/mcp/
  └── per-server config

src/skills/        ← Skill loader + runtime
src/mcp/           ← MCP client + auth flows
src/skillforge/    ← Authoring helper
```

## Trait extension point

Skills are data, not code — no trait. Adding a skill means writing `SKILL.md` + tool descriptors and dropping it in the skills dir, or installing from ClawHub.

For programmatic tool surfaces, see Pillar 3 — `Tool` trait.

## Why no `filesystem` MCP server in the curated list

Removed in v0.6.51. RantaiClaw ships built-in `shell`, `file_read`, and `file_write` tools gated by `SecurityPolicy.workspace_only` + `forbidden_paths`. The MCP `@modelcontextprotocol/server-filesystem` package duplicated those capabilities at the cost of:

- ~80MB of node + npx fetch on first agent boot
- 2 wasted tool iterations per filesystem op (the model probes both layers)
- A second allowed-dirs sandbox to keep in sync with the rantaiclaw workspace

MCP is reserved for integrations rantaiclaw can't natively implement — GitHub, Slack, Notion, Linear, etc. Users who still want the filesystem MCP can wire it manually with explicit allowed-dirs args:

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/workspace", "/path/to/skills"]
```

The TUI surfaces a one-shot deprecation toast at boot when it detects an existing `filesystem` MCP entry, pointing users at this section.

## CLI / config

```bash
rantaiclaw skills list
rantaiclaw skills install <source>      # ClawHub slug, git remote, or local path
rantaiclaw skills remove <name>
rantaiclaw setup skills                # multi-select picker
rantaiclaw setup mcp                   # 9-server curated picker
```

```toml
[skills.entries.web-search]
enabled = true

[skills.entries.summarizer]
enabled = false          # same as `rantaiclaw skills disable summarizer`

[mcp.<server-name>]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "..." }
```

## Roadmap

- [v0.6.0 — Product Completeness Beta](https://app.clickup.com/t/86exgu406) — Setup: Skills + Setup: MCP validate install/uninstall idempotence and the 9-server curated picker. Resilience: Skills + Resilience: MCP confirm state survives a server restart.
