# Pillar 4 — Skills and MCP Ecosystem

> **ClickUp:** [v0.5.0 Wave 2D/2E shipped](https://app.clickup.com/t/86exgrp1n) · **Maturity:** Stable · **Modules:** `src/skills/`, `src/mcp/`

Composable, reusable agent capabilities. Skills are markdown bundles with tool wiring; MCP servers extend the tool surface to the broader Model Context Protocol ecosystem.

## What this pillar covers

- 5-skill bundled starter pack (web-search, scheduler-reminders, summarizer, research-assistant, meeting-notes)
- ClawHub multi-select skill picker (sorted by stars, SHA-256 verified install)
- 8 MCP servers curated picker — 2 zero-auth + 6 authenticated, with inline auth (filesystem dropped in v0.6.51 — see below)
- Spawn-and-validate at setup time (zero-auth servers)
- `author_skill` — writes a skill bundle from chat
- Skill `[[tools]]` adapters offered to the model at every door (see below)

## Vs OpenClaw / Hermes-agent

| | RantaiClaw | OpenClaw | Hermes-agent |
|---|---|---|---|
| Bundled skill starter pack | ✅ 5 skills | TBD | TBD |
| ClawHub remote install | ✅ SHA-256 verified | n/a | n/a |
| MCP curated picker | ✅ 9 servers | TBD | TBD |
| Setup-time MCP validation | ✅ spawn-and-wait | TBD | TBD |
| Skill authoring helper | ✅ `author_skill` tool | TBD | TBD |

## Current state by maturity

_Rows below were read against the code on **2026-09-07**. A row with no evidence behind it is
a claim, not a status; where a row could not be settled by reading, it says so._

| Surface | Maturity |
|---|---|
| Skills runtime | Stable |
| Starter pack | Stable |
| ClawHub install + verify | Stable (fixed in v0.5.2) |
| MCP curated picker | Stable |
| MCP zero-auth setup-time validation | Stable (added v0.5.2) |
| Auth flow per MCP server | Stable |
| MCP tool **reach** | Partial — TUI/CLI agent and the gateway's `/api/v1` chat only. Chat channels, cron and gateway webhooks get no MCP tools. **No longer silent**: `rantaiclaw doctor` says so next to `mcp.startup` and `docs/reference/config.md` documents it (Wave 2). Issue #283 — making the reach universal — stays open |
| Skill tool **reach** | Stable — the `[[tools]]` adapters of every loaded, enabled skill are in the tool list of every turn: CLI, TUI, console chat, chat channels, cron, heartbeat and the gateway webhook. A disabled skill contributes nothing |
| MCP crash supervision | **None.** A crashed server is not respawned. The gateway keeps one connected pool and reconnects on config change; the respawn-with-backoff stack that used to be in `src/mcp` had no caller and was deleted (plan 312, issue #282) |
| `author_skill` authoring tool | Stable |

## Skill tools at every door

A skill's `[[tools]]` blocks become real callable tools, named `skill_<skill>_<tool>`. They sit in the shared tool
registry, so the model is offered them in every turn: CLI, TUI, console chat, chat channels, cron, heartbeat and the
gateway webhook alike. A skill that is disabled in the config contributes no tool. Two skills whose names sanitise to
the same adapter name yield one adapter; the later one is refused, and the collision is logged with both skill names.

Skill tools are gated like any other tool:

- `readonly` (the Strict preset) refuses them — a declared command is not known to be read-only.
- An owner under `supervised` gets an approval prompt naming the tool; skill tools are in no auto-approve list.
- A guest needs an explicit grant for the tool name (`rantaiclaw permissions add tool skill_<skill>_<tool>`).
- A delegate sub-agent receives a skill tool only when its `allowed_tools` names it.

### Which limits bind a skill tool

For the `shell` kind, the program a skill tool runs is the one the skill declares. Its `SKILL.toml` `[[tools]]` block
names the command: the stored command is split on whitespace, the first word is the program and the rest are its
arguments. The model may add one extra argument, appended as a single argument.

No shell sits in that path. The command never passes through a shell, so there is no expansion, no chaining, no pipe and
no redirect, and the model's extra argument is one argument rather than a command line. A skill tool cannot run a
pipeline.

The `shell` tool's command limits do not bind a skill tool. `allowed_commands` and the high- and medium-risk levels bind
the `shell` tool and a cron job's command, and bind no skill tool: a skill tool is not classified by the risk
classifier, so `block_high_risk_commands` and `require_approval_for_medium_risk` do not apply to it. The path limits
`forbidden_paths` and `workspace_only` are not the `shell` tool's limits either: they bind the file tools, the `ssh`
tool's local path and a path-shaped argument of a cron job's command, and bind no skill tool.

What does apply:

- the `readonly` refusal, since a skill tool never declares a read-only call;
- the approval prompt an owner gets under `supervised`;
- a 120 s timeout;
- for the `shell` kind, a refusal when the command string names a credential directory. The string is the declared
  command plus the model's extra argument, so a benign declared command still fails when the model's argument names a
  credential directory. The `http` kind has no such check.

The `http` kind runs `curl` directly, with explicit arguments and no shell, against the URL the skill declares.
`http_request.allowed_domains` does not apply to it, and neither do the built-in tool's private-host and DNS-rebinding
guards. The URL comes from the skill declaration, not from the model; the model picks the method and the body. The
method is one of `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD` or `OPTIONS`, and anything else falls back to `GET`.

Installing or enabling a skill is the trust decision. The `[[tools]]` block in its `SKILL.toml` says which program runs,
or which URL the `http` kind calls, so read it before you enable the skill.

### Where a recipe installs, and how the skill finds its own binary

A skill can declare the binaries it needs in `metadata.clawdbot.requires.bins`, and how to get them in
`metadata.clawdbot.install[]`. A `download` recipe installs into the skill's own tools directory,
`<XDG data>/rantaiclaw/tools/<skill name>/` — the skill's manifest name, not its directory slug. When the data
directory cannot be resolved the fallback is `.rantaiclaw/tools/<skill name>`, relative to the current directory. A
recipe MAY narrow that to a relative subdirectory; an absolute path or one with a `..` component is refused, so a
recipe cannot drop an executable into a directory the operator's own `PATH` reaches.

A skill whose manifest name is not a single path component — it carries a separator, a `..`, a `.`, a NUL byte
or a `:` anywhere (a drive prefix is the Windows case, but any colon is refused), or it is empty — has no tools
directory of its own, and its `download` recipe is refused rather than writing outside `<data>/tools/`. The rule is
the same on every platform, so a skill named the same way is treated the same way everywhere.

Three places look for that binary, and they agree:

- the `requires` gate, so `skills list` shows the skill as active rather than gated;
- `skills install-deps`, so the recipe's own post-check passes and the command exits 0;
- the process that runs the skill's tools: the child gets a `PATH` that starts with that skill's own tools
  directory, so the binary is found by name.

That directory is visible to that skill and to nothing else. Another skill's tools directory is not on the `PATH` of
a skill's tool, a plain `shell` call never sees it, and no MCP server, cron job or the process-wide `PATH` does
either. Nothing is symlinked into a `PATH` directory and no shell profile is edited, so a binary a recipe installed
cannot shadow a system binary for anything but that skill's own tool call.

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
```

## Trait extension point

Skills are data, not code — no trait. Adding a skill means writing `SKILL.md` + tool descriptors and dropping it in the skills dir, or installing from ClawHub.

For programmatic tool surfaces, see Pillar 3 — `Tool` trait.

## Why no `filesystem` MCP server in the curated list

Removed in v0.6.51. RantaiClaw ships built-in `file_read` and `file_write` tools gated by `SecurityPolicy.workspace_only` + `forbidden_paths`, and a `shell` tool, which those path limits do not bind. The MCP `@modelcontextprotocol/server-filesystem` package duplicated those capabilities at the cost of:

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

ClawHub installs require a `sha256` for every file in the version manifest. A version that lists a file without a valid 64-hex hash is refused before anything is fetched, and the refusal names the file; `skills update` keeps the installed version instead. Skills already on disk are not re-verified or changed by this check.

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
