# RantaiClaw Commands Reference

This reference is derived from the current CLI surface (`rantaiclaw --help`).

Last verified: **February 20, 2026**.

## Top-Level Commands

| Command | Purpose |
|---|---|
| `setup` | Canonical setup wizard (replaces `onboard`) |
| `onboard` | Legacy alias for `setup` |
| `agent` | Run interactive chat or single-message mode |
| `gateway` | Start webhook and WhatsApp HTTP gateway |
| `daemon` | Start supervised runtime (gateway + channels + optional heartbeat/scheduler) |
| `service` | Manage user-level OS service lifecycle |
| `doctor` | Run diagnostics and freshness checks |
| `status` | Print current configuration and system summary |
| `cron` | Manage scheduled tasks |
| `models` | Refresh provider model catalogs |
| `providers` | List provider IDs, aliases, and active provider |
| `channel` | Manage channels and channel health checks |
| `integrations` | Inspect integration details |
| `skills` | List/install/remove skills |
| `migrate` | Import from external runtimes (currently OpenClaw) |
| `config` | Export machine-readable config schema |
| `completions` | Generate shell completion scripts to stdout |
| `hardware` | Discover and introspect USB hardware |
| `peripheral` | Configure and flash peripherals |

## Command Groups

### `setup`

- `rantaiclaw setup`
- `rantaiclaw setup --force`
- `rantaiclaw setup <topic>`
- `rantaiclaw setup <topic> --force`
- `rantaiclaw setup --non-interactive`
- `rantaiclaw setup whatsapp-web --non-interactive`

`setup` walks every wired section (provider, channels, persona, skills, mcp) skipping any that are already configured. Pass `--force` to re-run already-configured sections. Pass `--non-interactive` (or run in non-TTY context) to emit each section's headless hint and exit rather than prompting.

Single-topic examples:
- `rantaiclaw setup provider` — re-run provider section only
- `rantaiclaw setup channels` — re-run channels section only
- `rantaiclaw setup whatsapp-web --non-interactive` — headless WhatsApp Web QR pairing (120s timeout)

`onboard` is a legacy alias for `setup`; its behaviour is unchanged.

### `onboard`

- `rantaiclaw onboard`
- `rantaiclaw onboard --interactive`
- `rantaiclaw onboard --channels-only`
- `rantaiclaw onboard --force`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --memory <sqlite|lucid|markdown|none>`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --model <MODEL_ID> --memory <sqlite|lucid|markdown|none>`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --model <MODEL_ID> --memory <sqlite|lucid|markdown|none> --force`

`onboard` safety behavior:

- If `config.toml` already exists, `onboard` asks for explicit confirmation before overwrite.
- In non-interactive environments, existing `config.toml` causes a safe refusal unless `--force` is passed.
- Use `rantaiclaw onboard --channels-only` when you only need to rotate channel tokens/allowlists.

### `agent`

- `rantaiclaw agent`
- `rantaiclaw agent -m "Hello"`
- `rantaiclaw agent --provider <ID> --model <MODEL> --temperature <0.0-2.0>`
- `rantaiclaw agent --peripheral <board:path>`

### `gateway` / `daemon`

- `rantaiclaw gateway [--host <HOST>] [--port <PORT>]`
- `rantaiclaw daemon [--host <HOST>] [--port <PORT>]`

### `service`

- `rantaiclaw service install`
- `rantaiclaw service start`
- `rantaiclaw service stop`
- `rantaiclaw service restart`
- `rantaiclaw service status`
- `rantaiclaw service uninstall`

### `cron`

- `rantaiclaw cron list`
- `rantaiclaw cron add <expr> [--tz <IANA_TZ>] <command>`
- `rantaiclaw cron add-at <rfc3339_timestamp> <command>`
- `rantaiclaw cron add-every <every_ms> <command>`
- `rantaiclaw cron once <delay> <command>`
- `rantaiclaw cron remove <id>`
- `rantaiclaw cron pause <id>`
- `rantaiclaw cron resume <id>`

Notes:

- Mutating schedule/cron actions require `cron.enabled = true`.
- Shell command payloads for schedule creation (`create` / `add` / `once`) are validated by security command policy before job persistence.

### `models`

- `rantaiclaw models refresh`
- `rantaiclaw models refresh --provider <ID>`
- `rantaiclaw models refresh --force`

`models refresh` currently supports live catalog refresh for provider IDs: `openrouter`, `openai`, `anthropic`, `groq`, `mistral`, `deepseek`, `xai`, `together-ai`, `gemini`, `ollama`, `llamacpp`, `astrai`, `venice`, `fireworks`, `cohere`, `moonshot`, `glm`, `zai`, `qwen`, and `nvidia`.

### `channel`

- `rantaiclaw channel list`
- `rantaiclaw channel start`
- `rantaiclaw channel doctor`
- `rantaiclaw channel bind-telegram <IDENTITY>`
- `rantaiclaw channel add <type> <json>`
- `rantaiclaw channel remove <name>`

Runtime in-chat commands (Telegram/Discord while channel server is running):

- `/models`
- `/models <provider>`
- `/model`
- `/model <model-id>`

Channel runtime also watches `config.toml` and hot-applies updates to:
- `default_provider`
- `default_model`
- `default_temperature`
- `api_key` / `api_url` (for the default provider)
- `reliability.*` provider retry settings

`add/remove` currently route you back to managed setup/manual config paths (not full declarative mutators yet).

### `integrations`

- `rantaiclaw integrations info <name>`

### `skills`

- `rantaiclaw skills list`
- `rantaiclaw skills install <source>`
- `rantaiclaw skills remove <name>`

`<source>` accepts git remotes (`https://...`, `http://...`, `ssh://...`, and `git@host:owner/repo.git`) or a local filesystem path.

Skill manifests (`SKILL.toml`) support `prompts` and `[[tools]]`; both are injected into the agent system prompt at runtime, so the model can follow skill instructions without manually reading skill files.

### `migrate`

- `rantaiclaw migrate openclaw [--source <path>] [--dry-run]`

### `config`

- `rantaiclaw config schema`

`config schema` prints a JSON Schema (draft 2020-12) for the full `config.toml` contract to stdout.

### `completions`

- `rantaiclaw completions bash`
- `rantaiclaw completions fish`
- `rantaiclaw completions zsh`
- `rantaiclaw completions powershell`
- `rantaiclaw completions elvish`

`completions` is stdout-only by design so scripts can be sourced directly without log/warning contamination.

### `hardware`

- `rantaiclaw hardware discover`
- `rantaiclaw hardware introspect <path>`
- `rantaiclaw hardware info [--chip <chip_name>]`

### `peripheral`

- `rantaiclaw peripheral list`
- `rantaiclaw peripheral add <board> <path>`
- `rantaiclaw peripheral flash [--port <serial_port>]`
- `rantaiclaw peripheral setup-uno-q [--host <ip_or_host>]`
- `rantaiclaw peripheral flash-nucleo`

## Validation Tip

To verify docs against your current binary quickly:

```bash
rantaiclaw --help
rantaiclaw <command> --help
```
