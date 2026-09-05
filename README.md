<p align="center">
  <img src="assets/rantaiclaw-banner.png" alt="RantaiClaw" width="600" />
</p>

<h3 align="center">Multi-Agent Runtime for Production AI Agents</h3>

<p align="center">
  <strong>100% Rust</strong> · Single binary · 17 channels · Scheduled runs · Approval gate · MCP + Skills
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

---

## What is RantaiClaw?

A production multi-agent runtime in Rust. One binary runs agents that talk across chat
channels, execute tools behind an approval gate, remember things, query a local
knowledge base, run skills, and act on a schedule without anyone watching.

Written for RantAI's agent platform, where it runs inside containers as the execution
engine for agents operating 24/7 against real integrations.

## Install

```bash
# Linux + macOS — detects platform, downloads, verifies SHA-256,
# installs, and runs the guided setup wizard.
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

**Windows (native)** — in PowerShell:

```powershell
iwr https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.ps1 -UseBasicParsing | iex
```

Both installers detect your arch, verify SHA-256, amend `PATH`, and finish by launching
the full setup wizard (`rantaiclaw setup --force` — provider, approvals, channels,
persona, skills, MCP, login, knowledge). Pass `--skip-setup` / `-SkipSetup` (or set
`RANTAICLAW_SKIP_SETUP=1`) to install only.

| Method | Command |
|---|---|
| Docker | `docker pull ghcr.io/rantai-dev/rantaiclaw:latest` |
| Cargo | `cargo install --git https://github.com/RantAI-dev/RantAIClaw --locked` |
| From source | `git clone … && cd RantAIClaw && ./bootstrap.sh --from-source` |
| Manual | [Pick a release archive](https://github.com/RantAI-dev/RantAIClaw/releases/latest), verify against `SHA256SUMS`, extract, move into `PATH` |

Every release ships cosign-signed archives plus SBOMs (`rantaiclaw.cdx.json`,
`rantaiclaw.spdx.json`). `rantaiclaw update` and `rantaiclaw ui install` **refuse**
an archive whose signature does not verify — including when `cosign` is missing
locally, since the `SHA256SUMS` file comes from the same server as the archive.
`--allow-unverified` is the explicit way past, and it says the artifact was not
verified.

### First run

```bash
rantaiclaw --version
rantaiclaw setup         # re-walk any unconfigured sections
rantaiclaw setup --force # re-walk every section
rantaiclaw doctor        # validate the install
rantaiclaw chat          # launch the TUI (also the default with no subcommand)
```

### Update / rollback / uninstall

```bash
rantaiclaw update    # self-replace from the latest release
rantaiclaw rollback  # restore the pre-update binary snapshot
rantaiclaw uninstall # remove profile data, optionally the binary
```

The installer picks the first writable dir on `PATH`, so check all of them when
removing manually; config and workspace live in `~/.rantaiclaw`. On Windows the
PowerShell installer writes to `%LOCALAPPDATA%\Programs\rantaiclaw`.

---

## The agent loop

An agent is a loop, not a model:

```
while iterations < max_tool_iterations:        # default 10
    response = provider(messages, tools)
    if response has tool_calls:
        approval gate                          # the single chokepoint
        result = execute(tool)
        messages += (tool_call, result)
    else:
        return response
```

The model never executes anything — it emits a request, and the runtime holds all the
authority. That is what makes one gate sufficient to bound the blast radius.

## Security

The most active area of the codebase, and the part to read first if you are evaluating
this for production. Authorisation is **three independent axes**, not one setting — and the
limits of each are stated below rather than implied away.

### 1. Autonomy level — how free the agent is

`ReadOnly` · `Supervised` · `Full`, reached through the presets you actually see:
Manual and Smart map to `Supervised`, **Strict maps to `ReadOnly`**, Off maps to `Full`.

Strict is plan mode: the `shell` tool is **dropped from the tool list the model sees**.
Not refused after being requested — never offered. A model cannot negotiate for
something it cannot see.

`Shift+Tab` cycles the preset live in the TUI; the runtime rebuilds the `SecurityPolicy`
on each switch and the TUI re-subscribes to the fresh approval broadcast.

### 2. Caller identity — owner vs guest

**Agents live on multi-user chat channels.** In a group chat anyone can type, so an
approval prompt aimed at "the user" is not enough. Senders listed in
`channels_config.approval_owners` get the full toolset. Everyone else is a **guest**,
and their turn runs under a `GuestGate` built per-turn: the tool must be permitted, and
if it is `shell` the command must match an allowed glob. **Anything else is denied
outright — a hard ceiling, never escalated to an owner.**

### 3. Risk classification — per subcommand, not per binary

Tools carry Low / Medium / High risk. `git`, `cargo` and `npm` look benign until you
remember that `git checkout` can fire hooks and `npm install` runs arbitrary build
scripts, so their code-executing subcommands are classified Medium.

### Hygiene at the gate

- **Arguments are de-quoted before the allowlist check** — otherwise `"rm"` passes where
  `rm` is blocked.
- **Option injection is blocked**: a leading-dash git branch name is refused.
- **Injection blocking** for `$()`, backticks, `&&`, `>`.
- **Credential scrubbing where text leaves the process.** `token`/`api_key`/`password`/
  `secret`/`bearer`/`credential` patterns are redacted (keeping a 4-char prefix for
  context) before cron run history is stored and before a cron result is announced to a
  chat; auto-saved conversation turns are screened the same way before they reach memory,
  which is the one store re-injected into later prompts. Provider and bot tokens are
  scrubbed from error logs too. **Tool output is not scrubbed on its way to the model** —
  an agent asked to read a credentials file is expected to be able to read it.
- **Rate limiting** (default 20 actions/hour) that stays correct on hosts with under an
  hour of uptime.
- **Deny cancels the whole turn**, not just the call — otherwise the model quietly tries
  a different command. There is no auto-deny timeout: the prompt waits indefinitely and
  the model is genuinely frozen. Cascading approvals walk `&&` chains, capped at 6
  prompts per call.

### What these controls do *not* cover

A stale document asserting an active security control is worse than silence, so:

- **`[security.sandbox]` has no effect today.** The `Sandbox` trait and its Landlock,
  bubblewrap, firejail and Docker backends exist, but `create_sandbox` has **no
  production caller** and the shell tool spawns commands unwrapped. Wiring it is a
  tracked follow-up (`plans/215`). For real in-process confinement today, use
  **`[runtime].kind`** — `native` or `docker`.
- **The tool-call audit trail is on, but `[security.audit]` still configures
  nothing.** Every tool call — executed and refused — now writes one JSON record to
  `<profile>/audit.log` (channel, tool name, approved, allowed, succeeded,
  duration; never the arguments). What is *not* wired is the operator's
  `[security.audit]` block: `SecurityConfig` is not a field of `Config`, so that
  section is still an unknown top-level key and the trail runs on defaults
  (enabled, 100 MB rotation). Writing it produces an `unknown config key
  \`security\`` warning at load.
- **`forbidden_paths` covers the file tools only** (`file_read`, `file_write`,
  `pdf_read`, `image_info`). It does **not** confine the shell: an allowlisted `cat` or
  `grep` can still read any path. Its always-denied floor cannot be removed and matching
  is case-folded, but that is the scope. For shell confinement, lower the autonomy level
  or set `[runtime].kind`.
- **`command_allowlist.toml` globs are advisory.** They are shown to the model, not
  enforced. The runtime gate matches `autonomy.allowed_commands` by **basename**, so
  `git status` there enforces as "any `git`". Editing the file changes what the model is
  told, not what the gate allows. Mutating subcommands are gated by `command_risk_level`
  instead.

## Scheduled runs

`rantaiclaw cron` (and `/cron` in the TUI) runs agents and shell jobs on a schedule.
Scheduling an agent is not "adding cron" — it moves the agent into an environment with
no human in it, so every assumption that depended on someone watching had to be replaced
explicitly:

| Problem | Resolution |
|---|---|
| A scheduled run needs approval, but nobody is there | Scheduled runs route to a non-interactive approval backend |
| Manual runs could double-execute | Poll loop decoupled; manual runs guarded |
| Read-modify-write races on a job | `update_job` reads and writes in one `IMMEDIATE` transaction |
| Daemon down for a week, then a flood of catch-ups | Stale catch-up runs gated behind `max_catchup_age_secs` |
| DST spring-forward deletes an hour | The schedule warns when it skips one |
| crontab weekday ordinals ≠ Quartz numbering | Remapped |
| `memory_recall` bleeding across conversations | Runs scoped to `cron:<job_id>` |
| A policy refusal looking like a crash | Recorded as status `refused`, and never announced to a channel |

Run history is redacted before storage and `jobs.db` is locked to `0600`.

## Channels

Connect an agent to any combination simultaneously. Each channel renders the model's
Markdown into what the target platform actually understands, so replies never leak raw
CommonMark.

| Channel | Build gate | In a release binary? | Reply rendering |
|---|---|---|---|
| Telegram | built in | yes | HTML |
| Discord | built in | yes | Markdown (fenced-code aware splitting) |
| Slack | built in | yes | mrkdwn |
| Mattermost | built in | yes | Markdown (native tables) |
| DingTalk | built in | yes | Markdown |
| WhatsApp Cloud | built in | yes | single-char markup |
| WhatsApp Web | `whatsapp-web` *(default on)* | yes | single-char markup |
| Signal · Email (IMAP/SMTP) · IRC · QQ · Linq · Nextcloud Talk · iMessage · CLI | built in | yes | plain text |
| Lark/Feishu | `channel-lark` | no | plain text |
| Matrix (E2EE) | `channel-matrix` | no | Markdown via matrix-sdk |

## Providers

Around 16 adapters — OpenAI, Anthropic, Gemini, Bedrock, GLM/Z.AI, Ollama, Copilot,
OpenAI Codex, Gemini CLI, OpenRouter and more — behind one `Provider` trait, with a
resilient wrapper (`providers/reliable.rs`) handling retry, fallback and timeouts
uniformly, and a router for `provider:model` resolution.

Anthropic, OpenAI and Gemini route through [`rig-core`](https://github.com/0xPlaygrounds/rig)
for streaming and tool calling. The hand-rolled implementations remain available behind
`--features legacy-providers` if rig misbehaves.

## Memory, knowledge, and context

**Three retrieval systems, deliberately separate:**

- `src/kb/` — the Knowledge Base subsystem for org-level documents: extraction,
  chunking, embedding, reranking, retrieval and store, backed by `sqlite-vec`
  (feature `kb`, on by default; `kb-office` adds docx/xlsx/pptx).
- `src/memory/` — agent memory with pluggable backends (markdown · sqlite · postgres),
  embeddings and vector merge.
- `src/rag/` — hardware datasheet retrieval: keyword by default, semantic optional, plus
  explicit pin-alias tables (`red_led: 13`). For pin lookup an alias table beats any
  embedding; not every retrieval problem is a vector problem.

**KB and memory may not import each other.** Different lifecycles, different ownership,
different retention — mixing them is the most common way an agent platform leaks one
user's data to another.

**Compaction.** When history outgrows the context budget, older turns are folded into a
five-section markdown summary — *Summary · Key facts established · State touched · Open
questions/TODOs · Most recent thread* — with `_None._` required for empty sections so
the format stays machine-parseable and the agent can rehydrate selectively. Historical
messages are replayed as real chat turns rather than stringified text, which produces
noticeably better summaries.

## Skills, MCP, and identity

**Skills are data, not code** — a `SKILL.md` plus tool descriptors dropped into the
profile's skills directory, or installed from ClawHub with SHA-256 verification.
The `author_skill` tool writes one from chat.

**MCP** extends the tool surface to the wider Model Context Protocol ecosystem, with a
curated picker and spawn-and-validate at setup time. The `filesystem` server was
deliberately **removed** from the curated list: it duplicated built-in `shell`,
`file_read` and `file_write` at the cost of ~80 MB of node fetched on first boot, two
wasted tool iterations per operation (the model probes both layers), and a second
allowed-dirs sandbox to keep in sync. **MCP is reserved for integrations RantaiClaw
cannot natively implement** — GitHub, Slack, Notion, Linear. You can still wire the
filesystem server manually with explicit allowed-dirs.

**Identity** supports OpenClaw markdown and **AIEOS v1.1** JSON — a portable spec
covering identity, psychology, linguistics, motivations, capabilities and history,
converted into the system prompt. Persona as structured, versionable, auditable data
rather than a hardcoded prompt string.

## Operations

- **Gateway** — HTTP `/api/v1` with SSE streaming, plus config, cron, task and approval
  endpoints (web and channel approval relays).
- **Observability** — `Observer` trait with OpenTelemetry (`observability-otel`),
  Prometheus, log and noop backends.
- **Tunnel** — Cloudflare, ngrok, Tailscale or custom, to expose an agent without
  opening a port.
- **Remote** — keys, registry and sessions for driving an agent from elsewhere.
- **Profiles** — `~/.rantaiclaw/profiles/<name>/` with its own config, workspace,
  memory, audit log, persona and skills; daemon handoff on switch; import from
  OpenClaw / ZeroClaw via `rantaiclaw migrate`.

## Interactive TUI

`rantaiclaw chat` opens a fullscreen terminal chat with a bottom-pinned composer:
readline chords (`Ctrl+A`/`E`/`U`/`K`/`W`), mouse and keyboard scroll that sticks to the
bottom while streaming, soft-wrap-aware caret movement, `Shift+Tab` to cycle the
approval preset in place, and slash commands (`/skill`, `/cron`, `/setup`, `/autonomy`,
`/help`).

## Footprint

No garbage collector and no interpreter startup: async Rust on `tokio`, shipped as a
single binary with no runtime dependencies. That is what makes it viable on client
hardware you do not control, including aarch64 on-prem boxes.

> Published footprint figures were measured against an older release artifact and have
> not been re-measured for the current version. Steady-state memory depends on which
> channels, providers and MCP servers you enable — **measure your own configuration**
> rather than trusting a headline number.

## Build features

`default = ["tui", "whatsapp-web", "remote-install", "kb"]`

Notable optional features: `hardware` (USB/serial boards), `peripheral-rpi`,
`channel-matrix`, `channel-lark`, `memory-postgres`, `observability-otel`,
`browser-native`, `probe` (probe-rs for Nucleo memory read), `rag-pdf`, `kb-office`,
`legacy-providers`.

The Landlock crate compiles in automatically on Linux with no feature flag, but see
**What these controls do not cover** — the sandbox layer is not wired to command
execution yet.

## Supply chain

`arrayref` is pinned to exactly `0.3.9`. In the 2026-08-20 incident every legitimate
version was yanked the same day and a trojaned `0.3.10` was published declaring a
dependency on `proc-macro1` — a typosquat of `proc-macro2` that pulls an HTTP client to
exfiltrate **at compile time**. crates.io versions are immutable, so a yank cannot alter
`0.3.9`'s bytes; the exact pin stops `cargo update` resolving forward. See `deny.toml`.
Do not bump it without independently verifying the crate is clean.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) and
[`docs/contributing/pr-workflow.md`](docs/contributing/pr-workflow.md).

## License

AGPL-3.0-only. See [`LICENSE`](LICENSE).
