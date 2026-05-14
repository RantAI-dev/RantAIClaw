<p align="center">
  <img src="assets/rantaiclaw-banner.png" alt="RantaiClaw" width="600" />
</p>

<h3 align="center">Multi-Agent Runtime for Production AI Employees</h3>

<p align="center">
  <strong>100% Rust</strong> · Zero overhead · Multi-channel · Live config · ClawHub compatible
</p>

<p align="center">
  <a href="https://github.com/RantAI-dev/RantAIClaw/releases/latest"><img src="https://img.shields.io/github/v/release/RantAI-dev/RantAIClaw?label=release&color=blue" alt="latest release" /></a>
  <a href="https://github.com/RantAI-dev/RantAIClaw/blob/main/LICENSE"><img src="https://img.shields.io/github/license/RantAI-dev/RantAIClaw" alt="license" /></a>
  <a href="https://github.com/RantAI-dev/RantAIClaw/actions/workflows/ci-run.yml"><img src="https://img.shields.io/github/actions/workflow/status/RantAI-dev/RantAIClaw/ci-run.yml?branch=main&label=CI" alt="CI status" /></a>
  <a href="https://github.com/RantAI-dev/RantAIClaw/stargazers"><img src="https://img.shields.io/github/stars/RantAI-dev/RantAIClaw?style=social" alt="stars" /></a>
</p>

<p align="center">
  <a href="docs/install.md"><strong>Install</strong></a> ·
  <a href="https://clawhub.ai">ClawHub Skills</a> ·
  <a href="docs/config-reference.md">Config</a> ·
  <a href="docs/channels-reference.md">Channels</a> ·
  <a href="docs/providers-reference.md">Providers</a> ·
  <a href="docs/troubleshooting.md">Troubleshooting</a> ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

---

## What is RantaiClaw?

RantaiClaw is a **production-grade multi-agent runtime** written in Rust. It powers autonomous AI employees that communicate across channels (Discord, Slack, Telegram, WhatsApp), execute tools, manage memories, and run skills — all from a single binary.

Built for **RantAI's digital employee platform**, RantaiClaw runs inside Docker containers as the execution engine for AI agents that operate 24/7 with real-world integrations.

### Why Rust?

| Metric | RantaiClaw | Python alternatives |
|--------|-----------|-------------------|
| Cold start | **< 200ms** | 2-5s |
| Memory (idle) | **~15 MB** | 200-500 MB |
| Binary size | **~12 MB** | N/A (runtime + deps) |
| Concurrent channels | **Thousands** | Hundreds |

No garbage collector. No runtime overhead. Just async Rust with `tokio`.

---

## Key Features

### Multi-Channel Communication
Connect your agent to any combination of channels simultaneously:

| Channel | Status | Protocol |
|---------|--------|----------|
| Telegram | Stable | Long-poll API |
| Discord | Stable | WebSocket gateway |
| Slack | Stable | Socket Mode / Web API |
| WhatsApp Web | Stable | Multi-device protocol |
| WhatsApp Cloud | Stable | Cloud API |
| Matrix (E2EE) | Feature-gated | Matrix SDK |
| Mattermost | Stable | WebSocket |
| Signal | Stable | signal-cli REST |
| Email (IMAP/SMTP) | Stable | IMAP + SMTP |
| IRC | Stable | IRC protocol |
| DingTalk | Stable | WebSocket |
| Lark/Feishu | Feature-gated | WebSocket |
| CLI | Built-in | stdin/stdout |

Each channel runs independently with its own lifecycle — add, remove, or update channels at runtime without restarting.

### Live Config API
Update any configuration at runtime via HTTP:

```bash
# Hot-swap model without restart
curl -X PATCH http://localhost:8080/config/model \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"provider": "anthropic", "model": "claude-sonnet-4-20250514"}'

# Add a Discord channel while running
curl -X PATCH http://localhost:8080/config/channels \
  -d '{"discord": {"bot_token": "...", "guild_id": "..."}}'

# Remove a channel gracefully
curl -X PATCH http://localhost:8080/config/channels \
  -d '{"telegram": null}'

# Start an MCP server for GitHub tools
curl -X PATCH http://localhost:8080/config/mcp-servers \
  -d '{"github": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "..."}}}'
```

Changes persist to `config.runtime.toml` and survive restarts.

### Multi-Provider Intelligence
Route to any LLM provider with automatic fallback:

- **OpenRouter** — access 200+ models through one API
- **OpenAI** — GPT-4o, o1, o3
- **Anthropic** — Claude Sonnet, Opus, Haiku
- **Google Gemini** — Gemini 2.5 Pro/Flash
- **Copilot** — GitHub Copilot models
- **ZAI GLM** — Chinese language models
- Custom OpenAI-compatible endpoints

### ClawHub Skills Ecosystem
Install community skills from [ClawHub](https://clawhub.ai):

```bash
rantaiclaw skill install deploy-checker
rantaiclaw skill install code-reviewer
rantaiclaw skill install meeting-summarizer
```

Skills are workspace-scoped markdown files with embedded tools and instructions. Create your own:

```markdown
# SKILL.md — deploy-checker

## Description
Validates deployment readiness before release.

## Tools
- name: run_checks
  kind: shell
  command: ./scripts/pre-deploy.sh

## Instructions
- Always run pre-deploy checks before approving a release
- Report any failing checks with specific remediation steps
```

### MCP Server Management
Run Model Context Protocol servers inside the container for tool integrations:

- **GitHub** — repositories, issues, PRs, code search
- **Slack** — channels, messages, users
- **Notion** — pages, databases, blocks
- **Linear** — issues, projects, cycles
- **Custom** — any MCP-compatible server

MCP servers are supervised with automatic restart on crash (exponential backoff, max 5 retries).

### Agentic Tool System
Built-in tools with security boundaries:

| Tool | Description |
|------|-------------|
| `shell` | Execute commands (sandboxed, allowlist-controlled) |
| `file_read` | Read files from workspace |
| `file_write` | Write files to workspace |
| `web_search` | Search the web |
| `memory_store` | Persist facts to long-term memory |
| `memory_recall` | Query memory by semantic similarity |
| `cron_schedule` | Create/manage scheduled tasks |
| `send_message` | Message coworkers |
| `browser` | Web automation (optional, feature-gated) |
| `composio` | 150+ app integrations via Composio |

### Autonomy

Two layers — the **runtime enum** the approval gate branches on, and the **L1–L4 presets** the setup wizard writes to disk.

**Runtime enum** (`AutonomyLevel` in `src/security/policy.rs`):

| Value | Behavior |
|-------|----------|
| `read_only` | Observe only; no shell, no writes |
| `supervised` (default) | Boot allowlist + runtime allowlist; unknown shell commands trigger an interactive approval prompt instead of hard-failing |
| `full` | Bypass the shell allowlist entirely (forbidden paths and `block_high_risk_commands` still apply) |

**Setup wizard presets** (write the right runtime enum + command_allowlist + forbidden_paths bundle):

| Preset | Wizard label | Maps to |
|---|---|---|
| **L1** | Manual — prompt for every tool call (safest) | `supervised` + narrow auto_approve |
| **L2** | Smart — prompt only for writes and system changes (recommended) | `supervised` + curated auto_approve (recommended starting point) |
| **L3** | Strict — prompt for all exec, allow read-only | `supervised` + deny-by-default + read-only auto_approve |
| **L4** | Off — autonomous execution, no prompts | `full` autonomy |

In v0.6.5+, Supervised mode adds an **async approval flow**: unknown shell basenames trigger a `/allow` / `/deny` prompt instead of hard-failing. See `docs/security/` and the user-facing site for the full mechanism.

### Memory System
Multiple backends for persistent agent memory:

- **SQLite** (default) — zero-config, file-based
- **Markdown** — human-readable memory files
- **PostgreSQL** — shared memory across agents (optional)

Memory supports semantic search via embeddings for context-aware recall.

---

## Quick Install

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

Works on Linux (x86_64, aarch64, armv7) and macOS (Intel, Apple Silicon). The installer auto-detects your platform, downloads the latest pre-built binary, verifies its SHA256 checksum, and installs it — no Rust toolchain, no compiler, no git clone.

> **Windows:** native Windows is not yet supported by the installer. Run via [WSL2](https://learn.microsoft.com/en-us/windows/wsl/install), or download the `x86_64-pc-windows-msvc.zip` manually from the [latest release](https://github.com/RantAI-dev/RantAIClaw/releases/latest).
>
> **PATH:** binary lands in `~/.cargo/bin` (or `~/.local/bin`). If `rantaiclaw --version` says "command not found", the installer prints the exact `export PATH=...` line for your shell.

After installation:

```bash
rantaiclaw --version
rantaiclaw setup                   # guided wizard — provider, approvals, channels, persona, skills, MCP
rantaiclaw doctor                  # verify the install and surface any gaps
rantaiclaw chat                    # start chatting!
```

> The legacy `rantaiclaw onboard` command still works as an alias for
> `rantaiclaw setup` through v0.5.0; new recipes should prefer `setup`.

---

## Getting Started

```bash
rantaiclaw chat                # Interactive TUI chat session
rantaiclaw setup               # Guided wizard (or `rantaiclaw setup <topic>` for a single section)
rantaiclaw doctor              # Diagnostics: config, policy, daemon, system deps
rantaiclaw daemon              # Run gateway: HTTP API + multi-channel listeners
rantaiclaw skill install <id>  # Install a community skill from ClawHub
rantaiclaw profile list        # Manage multi-profile configs (v0.5.0+)
rantaiclaw migrate --from auto # Import config from a legacy OpenClaw / ZeroClaw install
rantaiclaw status              # Verify install and show config health
rantaiclaw config get|set      # Inspect/update runtime config
rantaiclaw --help              # All commands
```

📖 **[Full install reference →](docs/install.md)** · **[Troubleshooting →](docs/troubleshooting.md)** · **[Releases →](https://github.com/RantAI-dev/RantAIClaw/releases)**

---

## Other install options

| Method | Command |
|--------|---------|
| **One-liner** (recommended) | `curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh \| bash` |
| **Manual download** | [Pick a release archive](https://github.com/RantAI-dev/RantAIClaw/releases/latest), verify against `SHA256SUMS`, extract, move into `PATH`. |
| **Build from source** | `git clone https://github.com/RantAI-dev/RantAIClaw.git && cd RantAIClaw && ./bootstrap.sh --from-source` |
| **Cargo** | `cargo install --git https://github.com/RantAI-dev/RantAIClaw --locked` |
| **Docker** | `docker pull ghcr.io/rantai-dev/rantaiclaw:latest` |
| **Bootstrap-managed Docker** | `./bootstrap.sh --docker --interactive-onboard` |
| **Homebrew** *(when published)* | `brew install rantaiclaw` |

Step-by-step recipes for each (with checksum + cosign verification, feature flags, container persistence): see **[docs/install.md](docs/install.md)**.

### Updating

```bash
# Re-run the installer — always pulls the latest release
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

### Uninstalling

```bash
rm -f ~/.cargo/bin/rantaiclaw ~/.local/bin/rantaiclaw
rm -rf ~/.rantaiclaw            # config + workspace (back up first if needed)
```

---

## Configuration

RantaiClaw uses TOML configuration at `~/.rantaiclaw/config.toml`:

```toml
# Model configuration
default_provider = "openrouter"
default_model = "anthropic/claude-sonnet-4-20250514"
default_temperature = 0.7

# Autonomy
[autonomy]
level = "supervised"
auto_approve = ["file_read", "memory_recall", "web_search"]
workspace_only = true
max_actions_per_hour = 100

# Channels
[channels_config]
cli = true

[channels_config.discord]
bot_token = "..."
guild_id = "..."
mention_only = true

[channels_config.telegram]
bot_token = "..."
allowed_users = ["*"]

# MCP Servers
[mcp_servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.github.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_..."

# Gateway
[gateway]
enabled = true
port = 8080
allow_public_bind = true
```

See [Config Reference](docs/config-reference.md) for all options.

---

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    RantaiClaw Binary                 │
├──────────┬──────────┬───────────┬───────────────────┤
│ Channels │  Tools   │   MCP     │    Gateway        │
│ Registry │ Registry │ Registry  │  (Config API)     │
├──────────┼──────────┼───────────┼───────────────────┤
│Telegram  │ shell    │ github    │ GET  /config      │
│Discord   │ file_*   │ notion    │ PATCH /config/*   │
│Slack     │ memory_* │ linear    │ GET  /health      │
│WhatsApp  │ cron_*   │ slack     │ POST /webhook     │
│Matrix    │ browser  │ custom    │ GET  /config/     │
│...       │ composio │           │      channels     │
├──────────┴──────────┴───────────┴───────────────────┤
│              Agent Loop (src/agent/)                 │
│     System Prompt → LLM → Tool Calls → Response     │
├─────────────────────────────────────────────────────┤
│           Provider Layer (OpenRouter/Anthropic/...)   │
├─────────────────────────────────────────────────────┤
│           Memory (SQLite/Markdown/PostgreSQL)         │
└─────────────────────────────────────────────────────┘
```

### Key Modules

| Module | Path | Responsibility |
|--------|------|----------------|
| Agent | `src/agent/` | Orchestration loop, prompt construction |
| Channels | `src/channels/` | Multi-channel communication |
| Tools | `src/tools/` | Tool execution with security boundaries |
| MCP | `src/mcp/` | MCP server process management |
| Gateway | `src/gateway/` | HTTP server, Config API, webhooks |
| Config | `src/config/` | Schema, runtime persistence |
| Memory | `src/memory/` | Multi-backend memory system |
| Security | `src/security/` | Policy engine, pairing, secrets |
| Providers | `src/providers/` | LLM provider adapters |
| Skills | `src/skills/` | Skill loading and execution |

---

## Feature Flags

```bash
# Default build (all common channels + tools)
cargo build --release

# With WhatsApp Web support
cargo build --release --features whatsapp-web

# With Matrix E2EE support
cargo build --release --features channel-matrix

# With hardware peripherals (RPi GPIO, Arduino)
cargo build --release --features hardware

# With browser automation
cargo build --release --features browser-native

# With OpenTelemetry observability
cargo build --release --features observability-otel

# Kitchen sink
cargo build --release --features "whatsapp-web,channel-matrix,browser-native,observability-otel"
```

---

## Development

```bash
# Format
cargo fmt --all

# Lint
cargo clippy --all-targets -- -D warnings

# Test
cargo test

# Full CI check
./dev/ci.sh all
```

---

## Credits

RantaiClaw is built on the foundation of [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw), an open-source AI agent runtime. We extend our gratitude to the ZeroClaw community for their pioneering work in Rust-native agent systems.

**RantaiClaw adds on top of ZeroClaw:**
- **Live Config API** — runtime configuration changes via HTTP endpoints
- **Channel Registry** — per-channel lifecycle with graceful shutdown via CancellationToken
- **MCP Server Management** — stdio-based process supervision with exponential backoff
- **Multi-agent orchestration** — team communication, cross-employee task delegation and review
- **ClawHub integration** — skill marketplace discovery and installation
- **Digital employee platform** — dashboard UI, integration management, deployment automation
- **Autonomy levels (L1–L4)** — configurable agent independence with tool-level permissions
- **Runtime config persistence** — `config.runtime.toml` overlay preserving base config

---

## Community

- **GitHub Discussions** — [RantAI-dev/RantAIClaw/discussions](https://github.com/RantAI-dev/RantAIClaw/discussions)
- **Issues & Feature Requests** — [RantAI-dev/RantAIClaw/issues](https://github.com/RantAI-dev/RantAIClaw/issues)
- **ClawHub Skills** — [clawhub.ai](https://clawhub.ai)

---

## Sponsor This Project

RantaiClaw is built and maintained by the RantAI team. If this project is useful to you, consider sponsoring to support ongoing development:

<p align="center">
  <a href="https://github.com/sponsors/RantAI-dev">
    <img src="https://img.shields.io/badge/Sponsor-RantAI-ea4aaa?style=for-the-badge&logo=github-sponsors&logoColor=white" alt="Sponsor RantAI" />
  </a>
</p>

Your sponsorship helps fund:
- New channel integrations and MCP server support
- Performance optimization and security hardening
- ClawHub skills ecosystem development
- Documentation and community support

---

## License

Licensed under the [GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE).

Copyright 2025–2026 RantAI.

---

## Star History

<p align="center">
  <a href="https://www.star-history.com/#RantAI-dev/RantAIClaw&type=Date">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=RantAI-dev/RantAIClaw&type=Date&theme=dark" />
      <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=RantAI-dev/RantAIClaw&type=Date" />
      <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=RantAI-dev/RantAIClaw&type=Date" />
    </picture>
  </a>
</p>

---

<p align="center">
  Built with Rust by <a href="https://rantai.com">RantAI</a>
</p>
