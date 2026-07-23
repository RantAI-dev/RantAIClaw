<p align="center">
  <img src="assets/rantaiclaw-banner.png" alt="RantaiClaw" width="600" />
</p>

<h3 align="center">Multi-Agent Runtime for Production AI Employees</h3>

<p align="center">
  <strong>100% Rust</strong> · Single binary · 17 channels · Live config API · ClawHub compatible
</p>

<p align="center">
  <a href="https://github.com/RantAI-dev/RantAIClaw/releases/latest"><img src="https://img.shields.io/github/v/release/RantAI-dev/RantAIClaw?label=release&color=blue" alt="latest release" /></a>
  <a href="https://github.com/RantAI-dev/RantAIClaw/blob/main/LICENSE"><img src="https://img.shields.io/github/license/RantAI-dev/RantAIClaw" alt="license" /></a>
  <a href="https://github.com/RantAI-dev/RantAIClaw/actions/workflows/ci-run.yml"><img src="https://img.shields.io/github/actions/workflow/status/RantAI-dev/RantAIClaw/ci-run.yml?branch=main&label=CI" alt="CI status" /></a>
  <a href="https://github.com/RantAI-dev/RantAIClaw/stargazers"><img src="https://img.shields.io/github/stars/RantAI-dev/RantAIClaw?style=social" alt="stars" /></a>
</p>

<p align="center">
  <a href="#install"><strong>Install</strong></a> ·
  <a href="docs/README.md">Docs</a> ·
  <a href="docs/reference/commands.md">Commands</a> ·
  <a href="docs/reference/config.md">Config</a> ·
  <a href="docs/reference/channels.md">Channels</a> ·
  <a href="docs/reference/providers.md">Providers</a> ·
  <a href="docs/reference/api-v1.md">HTTP API</a> ·
  <a href="docs/start/troubleshooting.md">Troubleshooting</a> ·
  <a href="docs/contributing/pr-workflow.md">Contributing</a>
</p>

<p align="center">
  <video src="https://github.com/RantAI-dev/RantAIClaw/raw/main/assets/tui.mp4" autoplay loop muted playsinline width="800">
    Your browser does not support inline video.
    <a href="https://github.com/RantAI-dev/RantAIClaw/raw/main/assets/tui.mp4">Download the TUI demo (MP4, ~770 KB)</a>.
  </video>
</p>

---

## Install

```bash
# Linux + macOS — auto-detects platform, downloads, verifies SHA256,
# installs, and runs `rantaiclaw setup --force` (full guided wizard).
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

**Windows (native, recommended)** — run in PowerShell:

```powershell
iwr https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.ps1 -UseBasicParsing | iex
```

Both installers detect your arch, download the matching prebuilt binary, verify SHA-256, amend `PATH`, and end by launching the **full guided setup wizard** (`rantaiclaw setup --force` — provider, approvals, channels, persona, skills, MCP, login, knowledge). Pass `--skip-setup` / `-SkipSetup` (or set `RANTAICLAW_SKIP_SETUP=1`) to install only. **Windows alternative (WSL2):** install [WSL2](https://learn.microsoft.com/en-us/windows/wsl/install) and run the Linux one-liner above from inside the Ubuntu shell.

| Method | Command |
|---|---|
| Docker | `docker pull ghcr.io/rantai-dev/rantaiclaw:latest` |
| Cargo | `cargo install --git https://github.com/RantAI-dev/RantAIClaw --locked` |
| From source | `git clone https://github.com/RantAI-dev/RantAIClaw.git && cd RantAIClaw && ./bootstrap.sh --from-source` |
| Manual | [Pick a release archive](https://github.com/RantAI-dev/RantAIClaw/releases/latest), verify against `SHA256SUMS`, extract, move into `PATH` |
| Homebrew *(planned)* | `brew install rantaiclaw` |

Every release ships cosign-signed archives plus SBOMs (`rantaiclaw.cdx.json`, `rantaiclaw.spdx.json`).

> **Step-by-step per-platform tutorial** (macOS Gatekeeper, Linux distro notes, Windows PowerShell, Raspberry Pi, Docker compose, cosign verify) is published with every release — see the latest [release notes](https://github.com/RantAI-dev/RantAIClaw/releases/latest). Long-form reference: [`docs/start/install.md`](docs/start/install.md) · [Troubleshooting](docs/start/troubleshooting.md).

### First run

The installer already ran `rantaiclaw setup --force` for you. To re-run, validate, or jump into the TUI:

```bash
rantaiclaw --version
rantaiclaw setup         # re-walk any unconfigured sections
rantaiclaw setup --force # re-walk every section from scratch
rantaiclaw doctor        # validate the install
rantaiclaw chat          # launch the TUI (also the default with no subcommand)
```

### Update / Rollback / Uninstall

```bash
rantaiclaw update             # self-replace from the latest release
rantaiclaw rollback           # restore the pre-update binary snapshot
rantaiclaw uninstall          # remove profile data, optionally the binary

# Manual removal — the installer picks the first writable dir on PATH, so check all of them
rm -f ~/.cargo/bin/rantaiclaw ~/.local/bin/rantaiclaw /usr/local/bin/rantaiclaw
rm -rf ~/.rantaiclaw          # config + workspace (back up first if needed)
```

On Windows the PowerShell installer writes to `%LOCALAPPDATA%\Programs\rantaiclaw` — remove that directory and its `PATH` entry. If you set `RANTAICLAW_INSTALL_DIR` at install time, remove the binary from there instead.

---

## What is RantaiClaw?

RantaiClaw is a **production-grade multi-agent runtime** written in Rust. It powers autonomous AI employees that talk across chat channels, execute tools, manage memories, query a local knowledge base, and run skills — all from a single binary.

Built for **RantAI's digital employee platform**, RantaiClaw runs inside Docker containers as the execution engine for AI agents that operate 24/7 with real-world integrations.

### Measured footprint

Measured against the published `v0.8.3-alpha` `x86_64-unknown-linux-gnu` release artifact:

| Metric | Value |
|--------|-------|
| Binary size (uncompressed) | ~32.5 MB |
| Release archive (download) | ~13.1 MB |
| `rantaiclaw --version` cold start | < 10 ms |
| Resident memory, trivial invocation | ~14 MB |

Steady-state daemon memory depends on which channels, providers, and MCP servers you enable — measure it for your own configuration rather than trusting a headline number. No garbage collector, no interpreter startup: async Rust on `tokio`.

---

## Key Features

### Interactive TUI

`rantaiclaw chat` (or bare `rantaiclaw`) opens a fullscreen terminal chat with a bottom-pinned composer:

- **Readline chords** — `Ctrl+A` / `Ctrl+E` / `Ctrl+U` / `Ctrl+K` / `Ctrl+W` in the composer.
- **Mouse and keyboard scroll** — wheel, `PgUp` / `PgDn`; the view sticks to the bottom while streaming.
- **Soft-wrap aware caret** — `Up` / `Down` move by visual row, not logical line.
- **`Shift+Tab`** cycles the approval-policy preset in place; `/autonomy` opens the picker.
- **Slash commands** — `/skill`, `/cron`, `/setup`, `/autonomy` and more; `/help` lists them.

### Multi-Channel Communication

Connect your agent to any combination of channels simultaneously. Each channel renders the model's Markdown into whatever the target platform actually understands, so replies do not leak raw CommonMark:

| Channel | Build gate | Reply rendering |
|---------|-----------|-----------------|
| Telegram | built in | HTML |
| Discord | built in | Markdown (fenced-code aware splitting) |
| Slack | built in | mrkdwn (single-char markup + Slack links) |
| Mattermost | built in | Markdown (native tables) |
| DingTalk | built in | Markdown |
| WhatsApp Cloud | built in | single-char markup |
| WhatsApp Web | `whatsapp-web` *(default on)* | single-char markup |
| Signal | built in | plain text |
| Email (IMAP/SMTP) | built in | plain text |
| IRC | built in | plain text |
| QQ | built in | plain text |
| Linq | built in | plain text |
| Nextcloud Talk | built in | plain text |
| iMessage | built in | plain text |
| Lark/Feishu | `channel-lark` | plain text |
| Matrix (E2EE) | `channel-matrix` | Markdown via matrix-sdk *(not yet on the shared renderer)* |
| CLI | built in | plain text |

Each channel runs independently with its own lifecycle — add, remove, or update channels at runtime without restarting. Per-platform setup: [`docs/reference/channels.md`](docs/reference/channels.md).

### Web Console

The console is a **separate Next.js app** ([claw-ui](https://github.com/RantAI-dev/claw-ui)), deliberately not bundled into the binary. The CLI fetches a pinned prebuilt release, checks SHA-256, and verifies the cosign signature when `cosign` is on `PATH` — it refuses outright if the signature bundle is missing, and warns and continues on SHA-256 alone if `cosign` is not installed. Install `cosign` first if you want signature verification enforced.

```bash
rantaiclaw ui install     # download + verify the pinned claw-ui release
rantaiclaw ui start       # serve the console on http://127.0.0.1:3939
rantaiclaw ui stop
rantaiclaw ui path        # where it was installed
```

Two processes, two ports: the Rust binary serves the API on the gateway port, Node serves the console on `3939`. The console authenticates at its own edge and holds the gateway bearer token server-side — the browser never sees it. Requires Node.js ≥ 18.18.

Protect it with a username/password gate (Argon2id, verified at `POST /login`), and optionally auto-lock an unattended session:

```toml
[gateway.login]
username = "operator"
password_hash = "$argon2id$..."   # written by `rantaiclaw setup login`
idle_timeout_secs = 1800          # 0 (default) = never auto-lock
```

`rantaiclaw setup login` offers 15m / 30m / 1h / 4h. Idleness is measured from operator input, so a long agent turn does not by itself keep the session alive — the TUI re-arms its login gate once the window lapses, and the console expires its session cookie. The gate masks the UI only: a turn in flight keeps streaming behind it. With no `password_hash` set there is nothing to unlock with, so the timeout is ignored.

### Multi-Provider Intelligence

Route to any LLM provider with automatic fallback. 33 providers ship built in (many with several alias keys), including:

- **Aggregators** — OpenRouter, Vercel AI Gateway, Cloudflare AI Gateway, OpenCode Zen
- **Frontier** — Anthropic, OpenAI (plus Codex subscription auth), Google Gemini, xAI, Mistral, Cohere
- **Fast inference** — Groq, Together AI, Fireworks AI, Perplexity, NVIDIA NIM
- **Open / regional** — DeepSeek, Qwen/DashScope, Z.AI, GLM/Zhipu, MiniMax, Moonshot/Kimi, Doubao, Qianfan
- **Local** — Ollama, LM Studio, llama.cpp
- **Cloud** — AWS Bedrock, GitHub Copilot
- **Custom** — any OpenAI-compatible endpoint via `custom:<url>`, or `anthropic-custom:<url>`

```bash
rantaiclaw providers          # list every supported key
rantaiclaw models refresh     # refresh model catalogs
rantaiclaw auth login --provider openai-codex   # OAuth; Codex is the only login provider
rantaiclaw auth setup-token   # Anthropic subscription tokens (paste-token / paste-redirect too)
```

Full matrix with base URLs and auth notes: [`docs/reference/providers.md`](docs/reference/providers.md).

### Autonomy and Approvals

Two layers — the **runtime enum** the approval gate branches on, and the **four named presets** the setup wizard writes to disk.

**Runtime enum** (`AutonomyLevel` in `src/security/policy.rs`):

| Value | Behavior |
|-------|----------|
| `readonly` | Observe only; no shell, no writes |
| `supervised` (default) | Boot allowlist + runtime allowlist; unknown shell commands trigger an interactive approval prompt instead of hard-failing |
| `full` | Bypass the shell allowlist entirely (forbidden paths and `block_high_risk_commands` still apply) |

**Setup wizard presets** (each writes a runtime enum + `allowed_commands` + `forbidden_paths` bundle):

| Preset | Wizard label | Maps to |
|---|---|---|
| **Manual** | prompt for every tool call (safest) | `supervised` + empty allowlist |
| **Smart** | safe read-only commands pre-allowed (recommended) | `supervised` + curated read-only allowlist |
| **Strict** | deny-by-default, no prompts (unattended agents) | `supervised` + strict mode + reads plus safe-write bookkeeping (`memory_write`, `skill_install`, `cron_*`, `session_*`) |
| **Off** | no gating at all (CI / fully-trusted only) | `full` autonomy |

Approval UX **in the TUI**:

- **Inline single-key prompt.** When the agent attempts a command not on the allowlist, a boxed widget replaces the input row: `[Y] yes (session)` · `[A] always (persist)` · `[N] no` · `[Esc] deny`.
- **Indefinite wait.** The prompt sits until you act — no auto-deny clock, so the model does not time out and try alternatives behind your back.
- **Deny cancels the whole turn.** Saying no rejects the call *and* cancels the LLM turn. One decision, one outcome.
- **Cascading approvals.** Commands like `cd … && python3 …` prompt for each blocking basename in the chain, capped at 6 per call.

On **chat channels and the gateway** the same gate applies with different ergonomics: approvals auto-deny after 300 s, and a denial fails that single tool call rather than cancelling the turn — the model may try something else. Use the `/allow X` slash command to persist an allowlist entry from those surfaces.
- **Strict preset = plan mode.** Under Strict the `shell` tool is **unregistered** from the model's tool list — the agent describes what you could run instead of trying to run it.
- **Switch fast.** `Shift+Tab` cycles presets in the TUI; `/autonomy` opens the picker; `rantaiclaw autonomy <preset>` flips it from the shell.

### Agentic Tool System

Roughly 45 tools are registered, gated by config and by the active preset:

| Group | Tools |
|-------|-------|
| Shell and files | `shell`, `file_read`, `file_write`, `glob_search` |
| Memory | `memory_store`, `memory_recall`, `memory_forget` |
| Web | `web_search_tool`, `http_request`, `browser`, `browser_open` |
| Scheduling | `cron_add`, `cron_list`, `cron_remove`, `cron_update`, `cron_run`, `cron_runs`, `schedule` |
| Tasks | `create_task`, `list_tasks`, `get_task`, `update_task_status`, `create_subtask`, `complete_subtask`, `review_task`, `add_comment`, `read_comments` |
| Skills | `skills_list`, `skills_search`, `skill_view`, `skills_install`, `skills_install_deps`, `author_skill` |
| Ops | `git_operations`, `proxy_config`, `ssh`, `pty`, `screenshot`, `image_info`, `pdf_read`, `pushover` |
| Multi-agent | `delegate` |
| Owner-only | `manage_permissions`, `issue_pairing_code` |
| Integrations | `composio` (1000+ apps) |

`ssh` and `pty` require the `remote-install` feature (on by default) and sit on the `always_ask` list. `browser`, `http_request`, and `web_search_tool` follow their config sections. Skills contribute their own `skill_<name>_<tool>` adapters at load time.

### Knowledge Base

A local, embedded RAG store (`kb` feature, **on by default**) with hybrid search, drift detection, and an entity/relation graph:

```bash
rantaiclaw kb ingest ./handbook.pdf     # PDF; Office documents with `kb-office`
rantaiclaw kb search "refund policy"
rantaiclaw kb list
rantaiclaw kb drift                     # find stale embeddings
rantaiclaw kb re-embed
rantaiclaw kb graph                     # entity/relation view
```

Also exposed over HTTP under `/api/v1/kb/*`. See [`docs/reference/kb.md`](docs/reference/kb.md) and [`docs/reference/kb-tuning.md`](docs/reference/kb-tuning.md).

### Cron and Scheduling

Jobs added from the CLI run a **shell command** on a schedule (not an agent prompt):

```bash
rantaiclaw cron add "0 9 * * 1-5" "./scripts/daily-report.sh"   # cron expression
rantaiclaw cron add-at "2026-08-01T09:00:00Z" "./scripts/launch.sh"  # RFC3339, one-shot
rantaiclaw cron add-every 1800000 "./scripts/check-queue.sh"    # interval in MILLISECONDS
rantaiclaw cron once 30m "./scripts/backup.sh"                  # relative delay: s/m/h/d
rantaiclaw cron list
rantaiclaw cron pause <id>    # also: resume, update, remove
```

The daemon runs the scheduler. Agent-prompt jobs are created by the agent itself through the `cron_*` and `schedule` tools. Cron is CLI/TUI/tool-driven today — there is no cron HTTP endpoint yet.

### MCP Server Management

Run Model Context Protocol servers as stdio subprocesses and expose their tools to the agent — GitHub, Slack, Notion, Linear, or any MCP-compatible server. Configure them in `config.toml` under `[mcp_servers.*]`, through `rantaiclaw setup mcp`, or live over the API.

Servers are spawned once when the agent is constructed; a server that dies stays down until the agent is rebuilt (the gateway builds a fresh agent per chat request, so API-added servers take effect on the next call). Automatic respawn with backoff exists in the codebase but is not wired into the live path yet.

**Known limitation:** MCP tools currently reach the TUI and the gateway (web console, `/api/v1/agent/chat`) only. Chat channels assemble their own tool list and do not include MCP tools, so an agent reached over Telegram, Discord, Slack, and the rest cannot call them.

### ClawHub Skills Ecosystem

Install community skills from [ClawHub](https://clawhub.ai):

```bash
rantaiclaw skills install deploy-checker
rantaiclaw skills list
rantaiclaw skills inspect deploy-checker
```

Skills are workspace-scoped. A `SKILL.md` carries instructions (metadata in YAML frontmatter); a `SKILL.toml` manifest is what registers executable tools. Create your own:

```toml
# SKILL.toml — deploy-checker
prompts = ["Always run pre-deploy checks before approving a release."]

[skill]
name = "deploy-checker"
description = "Validates deployment readiness before release."
version = "0.1.0"

[[tools]]
name = "run_checks"
description = "Run the pre-deploy validation script."
kind = "shell"                     # shell | http | script
command = "./scripts/pre-deploy.sh"
```

### Memory System

Multiple backends for persistent agent memory:

- **SQLite** (default) — zero-config, file-based, isolated per profile
- **Markdown** — human-readable memory files
- **PostgreSQL** — shared memory across agents (`memory-postgres` feature; use the exact key `postgres`)

Recall is keyword-based (FTS5/BM25) out of the box: `embedding_provider` defaults to `"none"`. Set it to a real embedding provider to enable semantic vector recall on the SQLite backend. Past conversations are browsable with `rantaiclaw session list|search|get`.

### Live Config API

The gateway serves a versioned control plane on `127.0.0.1:9393` — localhost-only and pairing-gated by default. Pair once to get a bearer token, then:

```bash
TOKEN=...   # issued by `rantaiclaw channel pair` / POST /pair

# Read the running config (secrets redacted)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:9393/api/v1/config

# Hot-swap the model without restarting
curl -X PUT http://127.0.0.1:9393/api/v1/config/model \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"provider":"anthropic","model":"claude-sonnet-4.6","temperature":0.7}'

# Tighten autonomy on the fly
curl -X PUT http://127.0.0.1:9393/api/v1/config/autonomy \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"level":"readonly"}'

# Add an MCP server while running
curl -X POST http://127.0.0.1:9393/api/v1/config/mcp_servers/github \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"command":"npx","args":["-y","@modelcontextprotocol/server-github"]}'
```

Other surfaces: `/api/v1/status`, `/api/v1/doctor`, `/api/v1/agent/chat` (SSE or JSON), `/api/v1/sessions*`, `/api/v1/skills*`, `/api/v1/memory*`, `/api/v1/channels`, `/api/v1/providers*`, `/api/v1/secrets`, `/api/v1/kb/*`. Unauthenticated operational roots: `/health`, `/readyz`, `/metrics`. Inbound webhooks: `/webhook`, `/whatsapp`, `/linq`, `/nextcloud-talk`, `/triggers/*`.

Changes persist to `config.runtime.toml` and survive restarts. Full endpoint reference: [`docs/reference/api-v1.md`](docs/reference/api-v1.md) · streaming details: [`docs/reference/api-v1-streaming.md`](docs/reference/api-v1-streaming.md).

---

## Commands

```bash
rantaiclaw chat                # Interactive TUI chat (default with no subcommand)
rantaiclaw setup [topic]       # Guided wizard; topics: provider approvals channels
                               #   persona skills mcp login knowledge
rantaiclaw doctor              # Diagnostics: config, policy, daemon, system deps
rantaiclaw daemon              # Gateway + channel listeners + scheduler + heartbeat
rantaiclaw gateway             # Gateway server only (webhooks, HTTP API)
rantaiclaw service install     # Run as an OS service (systemd/launchd)
rantaiclaw autonomy <preset>   # Switch approval policy
rantaiclaw channel list        # also: add, remove, pair, doctor, start
rantaiclaw skills install <id> # Install a community skill from ClawHub
rantaiclaw kb search "<query>" # Query the knowledge base
rantaiclaw cron list           # Scheduled tasks
rantaiclaw ui start            # Launch the web console
rantaiclaw auth login --provider openai-codex
                               # OAuth login (Codex only); see `auth --help` for token modes
rantaiclaw session list        # Browse past sessions
rantaiclaw memory list         # Inspect agent memory (also: get, stats, clear)
rantaiclaw profile list        # Multi-profile configs
rantaiclaw permissions show    # Per-role channel permissions
rantaiclaw migrate --from auto # Import config from a legacy OpenClaw / ZeroClaw install
rantaiclaw status              # Verify install and show config health
rantaiclaw config schema       # Dump the config JSON schema
rantaiclaw completions <shell> # Shell completion script
rantaiclaw --help              # All commands
```

📖 **[Full command reference →](docs/reference/commands.md)** · **[Install guide →](docs/start/install.md)** · **[Troubleshooting →](docs/start/troubleshooting.md)** · **[Releases →](https://github.com/RantAI-dev/RantAIClaw/releases)**

---

## Configuration

RantaiClaw uses TOML configuration at `~/.rantaiclaw/config.toml`. The values below are the real shipped defaults:

```toml
schema_version = 14

# Model
default_provider = "openrouter"
default_model = "anthropic/claude-sonnet-4.6"
default_temperature = 0.7

# Autonomy
[autonomy]
level = "supervised"
auto_approve = ["file_read", "memory_recall"]
always_ask = ["ssh", "pty"]
workspace_only = true
max_actions_per_hour = 200
block_high_risk_commands = false

# Channels
[channels_config]
cli = true

[channels_config.discord]
bot_token = "..."
guild_id = "..."
mention_only = false

[channels_config.telegram]
bot_token = "..."
allowed_users = ["*"]

# MCP servers
[mcp_servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp_servers.github.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_..."

# Gateway — localhost-only and pairing-gated by default
[gateway]
host = "127.0.0.1"
port = 9393
require_pairing = true
allow_public_bind = false   # read docs/operations/network-deployment.md before exposing on a LAN

# Web console auth (optional)
[gateway.login]
username = "operator"
idle_timeout_secs = 0       # 0 = never auto-lock; presets 900 / 1800 / 3600 / 14400

# Web console host
[ui]
host = "127.0.0.1"
```

Local capability tools (`web_search`, `http_request`, `browser`) ship **enabled** so a fresh install is useful immediately; network *exposure* stays deny-by-default. See the [Config Reference](docs/reference/config.md) for every option and [`docs/security/README.md`](docs/security/README.md) for the threat model.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                      RantaiClaw Binary                        │
├──────────┬──────────┬───────────┬────────┬───────────────────┤
│ Channels │  Tools   │   MCP     │   KB   │     Gateway       │
│ Registry │ Registry │ Registry  │ Store  │   (HTTP API)      │
├──────────┼──────────┼───────────┼────────┼───────────────────┤
│Telegram  │ shell    │ github    │ search │ GET  /health      │
│Discord   │ file_*   │ notion    │ ingest │ POST /pair        │
│Slack     │ memory_* │ linear    │ drift  │ GET  /api/v1/*    │
│WhatsApp  │ cron_*   │ slack     │ graph  │ PUT  /api/v1/     │
│Matrix    │ browser  │ custom    │        │        config/*   │
│QQ, IRC…  │ delegate │           │        │ POST /webhook     │
├──────────┴──────────┴───────────┴────────┴───────────────────┤
│                  Agent Loop (src/agent/)                      │
│        System Prompt → LLM → Tool Calls → Response            │
├──────────────────────────────────────────────────────────────┤
│      Provider Layer (OpenRouter / Anthropic / local / …)      │
├──────────────────────────────────────────────────────────────┤
│         Memory (SQLite / Markdown / PostgreSQL)               │
└──────────────────────────────────────────────────────────────┘
                              ▲
                              │ bearer token, held server-side
                    ┌─────────┴──────────┐
                    │  claw-ui console   │ separate Next.js process, :3939
                    └────────────────────┘
```

### Key Modules

| Module | Path | Responsibility |
|--------|------|----------------|
| Agent | `src/agent/` | Orchestration loop, prompt construction |
| Channels | `src/channels/` | Multi-channel transport + reply rendering |
| Tools | `src/tools/` | Tool execution with security boundaries |
| MCP | `src/mcp/` | MCP server process management |
| Gateway | `src/gateway/` | HTTP server, config API, webhooks |
| Config | `src/config/` | Schema, migrations, runtime persistence |
| Memory | `src/memory/` | Multi-backend memory system |
| KB | `src/kb/` | Knowledge base, embeddings, entity graph |
| Security | `src/security/` | Policy engine, pairing, secrets, console login |
| Providers | `src/providers/` | LLM provider adapters |
| Skills | `src/skills/` | Skill loading and execution |
| TUI | `src/tui/` | Fullscreen terminal chat |
| Peripherals | `src/peripherals/` | Hardware boards (STM32, RPi GPIO) |

---

## Feature Flags

Default features: `tui`, `whatsapp-web`, `remote-install`, `kb`.

```bash
# Default build
cargo build --release

# Matrix E2EE / Lark
cargo build --release --features "channel-matrix,channel-lark"

# Hardware peripherals (RPi GPIO, Arduino, STM32 probe)
cargo build --release --features "hardware,peripheral-rpi,probe"

# Browser automation, PostgreSQL memory, OpenTelemetry
cargo build --release --features "browser-native,memory-postgres,observability-otel"

# Office / OCR document ingestion for the KB
cargo build --release --features "kb-office,kb-ocr"

# Minimal: drops TUI, WhatsApp Web, KB — and remote-install, so no `ssh`/`pty` tools
cargo build --release --no-default-features
```

| Feature | Default | Enables |
|---|---|---|
| `tui` | ✅ | Fullscreen terminal chat |
| `whatsapp-web` | ✅ | WhatsApp multi-device backend |
| `remote-install` | ✅ | `ssh` / `pty` tools, remote provisioning |
| `kb` | ✅ | Knowledge base, vector search, PDF ingest |
| `channel-matrix` | — | Matrix (E2EE) channel |
| `channel-lark` | — | Lark/Feishu channel |
| `hardware`, `peripheral-rpi`, `probe` | — | USB/serial boards, RPi GPIO, STM32 flashing |
| `browser-native` | — | Fantoccini-backed browser automation |
| `memory-postgres` | — | PostgreSQL memory backend |
| `observability-otel` | — | OpenTelemetry export |
| `kb-office`, `kb-ocr` | — | Office-document / OCR ingestion |
| `legacy-providers` | — | Hand-rolled OpenAI provider path |

---

## Development

```bash
# Format
cargo fmt --all

# Lint
cargo clippy --all-targets -- -D warnings

# Test
cargo test

# Full CI check (Docker-based, recommended before opening a PR)
./dev/ci.sh all
```

Read [`CLAUDE.md`](CLAUDE.md) for the engineering protocol, [`docs/contributing/pr-workflow.md`](docs/contributing/pr-workflow.md) for the PR flow, and [`docs/contributing/reviewer-playbook.md`](docs/contributing/reviewer-playbook.md) if you are reviewing.

---

## Credits

RantaiClaw is built on the foundation of [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw), an open-source AI agent runtime. We extend our gratitude to the ZeroClaw community for their pioneering work in Rust-native agent systems.

**RantaiClaw adds on top of ZeroClaw:**
- **Live Config API** — versioned `/api/v1` control plane for runtime configuration
- **Channel Registry** — per-channel lifecycle with graceful shutdown via CancellationToken
- **Per-platform reply rendering** — Markdown translated into each channel's native markup
- **MCP Server Management** — stdio-based process supervision with exponential backoff
- **Knowledge Base** — embedded hybrid-search RAG store with drift detection and an entity graph
- **Multi-agent orchestration** — cross-employee task delegation and review
- **ClawHub integration** — skill marketplace discovery and installation
- **Web console** — cosign-verified prebuilt claw-ui behind an Argon2id password gate
- **Autonomy presets (Manual / Smart / Strict / Off)** — configurable agent independence with tool-level permissions
- **Profile isolation** — per-profile config, sessions, and knowledge base
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
