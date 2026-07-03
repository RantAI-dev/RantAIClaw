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
| `kb` | Knowledge Base CRUD + maintenance (gated by `--features kb`) |

## Command Groups

### `autonomy`

- `rantaiclaw autonomy` — print the currently-active preset + the four options
- `rantaiclaw autonomy <preset>` — switch to `manual`, `smart`, `strict`, `off`, or `full` (alias for `off`)

Profile-level operation. Writes `<profile>/policy/{autonomy,command_allowlist,forbidden_paths}.toml` from the bundled preset AND mirrors `[autonomy].level` + `[autonomy].allowed_commands` into `config.toml` so the runtime gate actually consumes the change. `runtime_allowlist.toml` (from `/allow X --persist`) is preserved across preset switches.

| Preset | Behaviour |
|---|---|
| `manual` | Every shell call prompts. Read-only file/memory tools not gated. |
| `smart` | Read-only and safe shell builtins pre-allowed (`ls`, `cd`, `echo`, `git status`, `which`, etc.). Writes prompt with an inline `[Y/A/N/Esc]` widget. |
| `strict` | `shell` tool removed from the model's registry entirely. Agent describes commands the user can run, doesn't execute. CC plan-mode analog. |
| `off` / `full` | No gating. Forbidden paths still enforced. CI / trusted-env only. |

Inside the TUI: `Shift+Tab` cycles · `/autonomy` opens an interactive picker · `/autonomy <preset>` skips the picker.

### `setup`

- `rantaiclaw setup`
- `rantaiclaw setup --force`
- `rantaiclaw setup <topic>`
- `rantaiclaw setup <topic> --force`
- `rantaiclaw setup --non-interactive`
- `rantaiclaw setup whatsapp-web --non-interactive`

`setup` walks every wired section (provider, channels, persona, skills, mcp, and — when built with the default `kb` feature — knowledge) skipping any that are already configured. Pass `--force` to re-run already-configured sections. Pass `--non-interactive` (or run in non-TTY context) to emit each section's headless hint and exit rather than prompting.

Single-topic examples:
- `rantaiclaw setup provider` — re-run provider section only
- `rantaiclaw setup channels` — re-run channels section only
- `rantaiclaw setup knowledge` — set the Knowledge Base API keys (`[knowledge].embedding_api_key` / `vision_api_key`, encrypted at rest; vision falls back to the embedding key). Env `KB_EMBEDDING_API_KEY` / `KB_EXTRACT_VISION_API_KEY` override config at load, with `OPENROUTER_API_KEY` as the final fallback. See [config-reference.md](config-reference.md) for the gateway `GET`/`PUT /api/v1/config/knowledge` endpoints.
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
- `rantaiclaw skills install-deps [<slug> | --all]`
- `rantaiclaw skills inspect <slug>`
- `rantaiclaw skills update [<slug> | --all]`
- `rantaiclaw skills remove <name>`

`<source>` accepts a ClawHub slug, git remotes (`https://...`, `http://...`, `ssh://...`, and `git@host:owner/repo.git`), or a local filesystem path.

Skill manifests (`SKILL.md`) support YAML frontmatter, `requires` gating, environment injection, and `metadata.clawdbot.install[]` recipes. `install-deps` runs the preferred host recipe (`brew`, `uv`, Node package managers, `go`, or `download`) and validates declared binaries afterward.

In the TUI, `/skills` opens the local skills picker. Press `Ctrl+I` (or Tab in terminals that send Ctrl+I as Tab) on a gated skill row to run its install-deps recipe without leaving the picker.

### API sessions

`POST /api/v1/agent/chat` records completed turns in the same `sessions.db` used by `agent -m`, the TUI, and the `/api/v1/sessions*` endpoints. API-created sessions use `source = "api"` and include the user message, assistant response, derived title, and end timestamp.

The same endpoint can stream partial output as Server-Sent Events when called
with `Accept: text/event-stream` or `?stream=1`; see
`docs/api-v1-streaming.md` for the event schema.

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

### `kb` (Knowledge Base)

Gated by `--features kb`. Off in the default build. See [kb.md](kb.md) for the full KB chapter (architecture, sidecars, HTTP API).

The `kb` subcommands follow the axi-cli contract: idempotent, never interactive, TOON output by default, `--json` toggles JSON. Exit code `0` is success, `1` is an operational failure (the binary prints a TOON `error[1]{code,message}:` block to stdout).

#### `kb search <query>`

Hybrid retrieval over the knowledge base.

| Flag | Description | Default |
|---|---|---|
| `--top <n>` | Max chunks to return | `5` |
| `--group <id>` | Filter by knowledge-base group ID (repeatable) | _none_ |
| `--category <c>` | Filter by category | _none_ |
| `--json` | Emit JSON instead of TOON | `false` |

#### `kb ingest <path>`

Extract, chunk, embed, and store a file.

| Flag | Description | Default |
|---|---|---|
| `--title <t>` | Override document title | file stem |
| `--category <c>` | Add to categories (repeatable) | _none_ |
| `--group <id>` | Add to knowledge-base groups (repeatable) | _none_ |
| `--json` | Emit JSON instead of TOON | `false` |

Supported file types depend on feature flags — see [kb.md](kb.md#ingest-a-document).

#### `kb list`

List documents.

| Flag | Description | Default |
|---|---|---|
| `--organization <id>` | Filter by organization ID | _none_ |
| `--json` | Emit JSON instead of TOON | `false` |

#### `kb get <id>`

Show one document. Exits `1` with a TOON `error[not_found]` block when the ID is absent.

| Flag | Description | Default |
|---|---|---|
| `--json` | Emit JSON instead of TOON | `false` |

#### `kb delete <id>`

Soft-delete by default (sets `deleted_at`, hides from search). `--hard` permanently removes the document and its chunks.

| Flag | Description | Default |
|---|---|---|
| `--hard` | Permanently remove rows | `false` |

#### `kb drift`

Report which chunks were embedded with a non-current model.

| Flag | Description | Default |
|---|---|---|
| `--json` | Emit JSON instead of TOON | `false` |

#### `kb re-embed`

Re-embed chunks using the currently-configured embedding model.

| Flag | Description | Default |
|---|---|---|
| `--include-current` | Re-embed even chunks already on the current model | `false` |
| `--dry-run` | Report what would happen without writing | `false` |
| `--batch-size <n>` | Batch size for re-embed work | `100` |
| `--json` | Emit JSON instead of TOON | `false` |

## Validation Tip

To verify docs against your current binary quickly:

```bash
rantaiclaw --help
rantaiclaw <command> --help
```
