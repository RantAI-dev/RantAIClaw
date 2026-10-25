# RantaiClaw Commands Reference

This reference is derived from the current CLI surface (`rantaiclaw --help`).

**What "verified" means here**, because a date on a document is a claim:

- **Command coverage is checked continuously and blocks CI.**
  `scripts/ci/docs_command_coverage.sh` compares every variant of `enum Commands` in
  `src/main.rs` against this file on every Rust change. It has no exemption list. A command
  that exists and is not here fails the build.
- **Flags, subcommands and behaviour are checked by hand.** Nothing enforces those, so they
  carry a date:
  - **September 7, 2026** — `autonomy`, `auth`, `chat`, `insights`, `permissions`,
    `personality`, `profile`, `rollback`, `session`, `uninstall`, `update`, and the
    `doctor` / `doctor models` split. Written from the `--help` output of a binary built at
    that commit.
  - **July 12, 2026** — every other section.

## Top-Level Commands

| Command | Purpose |
|---|---|
| `setup` | Canonical setup wizard (replaces `onboard`) |
| `onboard` | Legacy alias for `setup` |
| `autonomy` | Show or switch the approval-policy preset for the active profile |
| `agent` | Run interactive chat or single-message mode |
| `chat` | Start the interactive TUI — **the default when no subcommand is given** |
| `gateway` | Start webhook and WhatsApp HTTP gateway |
| `daemon` | Start supervised runtime (gateway + channels + optional heartbeat/scheduler) |
| `service` | Manage user-level OS service lifecycle |
| `doctor` | Run diagnostics for configuration, daemon, scheduler and channel freshness |
| `status` | Print current configuration and system summary |
| `usage` | Print today's token usage against the daily ceiling |
| `cron` | Manage scheduled tasks |
| `models` | List and refresh provider model catalogs |
| `providers` | List provider IDs, aliases, and active provider |
| `channel` | Manage channels and channel health checks |
| `integrations` | Browse and inspect integrations |
| `skills` | List/install/remove skills |
| `config` | Show the active config (redacted) or export its JSON schema |
| `completions` | Generate shell completion scripts to stdout |
| `kb` | Knowledge Base CRUD + maintenance (in the default build) |
| `ui` | Install/run/stop the optional web console (claw-ui) |
| `memory` | Agent memory: list, get, add, recall, reindex, stats, clear |
| `update` | Self-replace the binary against a published GitHub release |
| `auth` | Manage provider subscription auth profiles (OAuth / setup tokens) |
| `permissions` | Owners and the non-owner (guest) capability ceiling, shared by all multi-user channels |
| `personality` | Show, list, or set the active persona preset |
| `profile` | Create, clone, switch, and delete profiles |
| `session` | Browse, search, and rename past sessions |
| `insights` | Cumulative session/message statistics (CLI parity for the TUI `/insights`) |
| `rollback` | Restore config and binary from a pre-update snapshot |
| `uninstall` | Remove profile data, optionally the binary |

## Command Groups

### `autonomy`

- `rantaiclaw autonomy` — print the active preset, the enforced autonomy level from `config.toml`, and the four options (warns when the preset marker has drifted from the enforced level)
- `rantaiclaw autonomy <preset>` — switch to `manual`, `smart`, `strict`, `off`, or `full` (alias for `off`)

Profile-level operation. Writes `<profile>/policy/{autonomy,command_allowlist}.toml` from the bundled preset AND mirrors `[autonomy].level`, `[autonomy].always_ask`, and `[autonomy].allowed_commands` into `config.toml` so the runtime gate actually consumes the change. `runtime_allowlist.toml` (from `/allow X --persist`) is preserved across preset switches.

The preset is read from the profile's marker (`<profile>/policy/autonomy.toml`), which is the one source of truth every surface reads — `rantaiclaw status`, this command, the channel prompt, and `GET /api/v1/status`. A profile with no marker has no preset: the command says `not provisioned — run \`rantaiclaw setup approvals\`` rather than naming a preset derived from `config.toml`, because the config cannot tell "no preset chosen" from "the default".

The preset's *effect* is encoded in `config.toml` as a `level` + `always_ask` pair — the same encoding the web console reads and writes — so a preset switched here is visible in the console (and vice versa) without a restart. The preset's *name* comes from the marker, not from that pair.

| Preset | `[autonomy].level` | `[autonomy].always_ask` | `[autonomy].auto_approve` | Behaviour |
|---|---|---|---|---|
| `manual` | `supervised` | `["*"]` (every tool) | empty | Every shell call prompts. Read-only file/memory tools not gated. |
| `smart` | `supervised` | empty | the default list (`file_read`, `memory_recall`) when empty, otherwise unchanged | Read-only and safe shell builtins pre-allowed (`ls`, `cd`, `echo`, `git status`, `which`, etc.). Writes prompt with an inline `[Y/A/N/Esc]` widget. |
| `strict` | `readonly` | unchanged, minus `"*"` | unchanged | `shell` tool removed from the model's registry entirely, and every acting tool is refused by the autonomy gate. Agent describes commands the user can run, doesn't execute. CC plan-mode analog. |
| `off` / `full` | `full` | unchanged, minus `"*"` | unchanged | No gating. Forbidden paths still enforced. CI / trusted-env only. |

Each preset sets the fields in its row to that preset's values, whatever the previous preset left behind. `manual` then `smart` gives the same config as `smart` on a fresh config. `"*"` is Manual's mark: only `manual` keeps it, and `strict` and `off` drop it. Named entries an operator wrote (for example `ssh`, `pty`) survive `strict` and `off`. An install already left in the old `full` plus `"*"` state is repaired by running `rantaiclaw autonomy off` again.

Inside the TUI: `Shift+Tab` cycles · `/autonomy` opens an interactive picker · `/autonomy <preset>` skips the picker.

### `status`

- `rantaiclaw status` — print the active configuration and a system summary

The `Autonomy` row names the approval preset the active profile has, read from the profile's marker (`<profile>/policy/autonomy.toml`) — the same source `rantaiclaw autonomy` reads, so the two cannot disagree. A profile with no marker has no preset, and the row reads `not provisioned — run \`rantaiclaw setup approvals\`` rather than a preset name derived from `config.toml`.

The `Enforced` row right after it names the autonomy level the runtime gate actually enforces, read from `config.toml`. A profile whose marker has drifted from its config therefore shows both: the preset the profile chose and the level in force — plus a warning naming both levels and the `rantaiclaw autonomy <preset>` command that resyncs them, the same warning `rantaiclaw autonomy` prints. The model is never told a preset the gate does not enforce: a drifted preset is dropped from the system prompt, which falls back to its generic safety floor.

### `setup`

- `rantaiclaw setup`
- `rantaiclaw setup --force`
- `rantaiclaw setup <topic>`
- `rantaiclaw setup <topic> --force`
- `rantaiclaw setup --non-interactive`
- `rantaiclaw setup whatsapp-web --non-interactive`

`setup` walks every wired section (provider, approvals, channels, persona, skills, mcp, login, and — when built with the default `kb` feature — knowledge) skipping any that are already configured. Pass `--force` to re-run already-configured sections.

Approval presets are kept rather than re-offered: `rantaiclaw setup approvals` on a profile that already has a preset on disk names the preset that is active and points at `rantaiclaw setup approvals --force`, because without `--force` the write would leave the preset in place anyway. With `--force` the picker appears, the preset you pick is written, and the preset's policy files — `<profile>/policy/autonomy.toml` and `<profile>/policy/command_allowlist.toml` — are regenerated from the preset bundle, so hand edits to them are lost.

`--non-interactive` (or a non-TTY context) behaves differently depending on whether a topic is given:

- **`rantaiclaw setup --non-interactive`** — walks the sections and emits each one's headless hint instead of prompting. A section that needs a prompt cannot run, so it is named in a closing summary as **not configured**, together with the command that configures it instead. The exit code follows the **provider** — the one required section: a sweep that could not configure the provider exits **non-zero**, because the agent cannot run without a provider credential. The other prompt-needing sections (`channels`, `mcp`, `login`, `knowledge`) are optional: they do not fail the run, and the summary names them as optional. A profile where the provider is configured — whatever the optional sections did — exits **0**. A section that errored aborts the run with a non-zero exit as before. Scripts should check the exit code.
- **`rantaiclaw setup <topic> --non-interactive`** — runs that topic's provisioner unattended. Text prompts take the default the provisioner declared, secret prompts get an empty value, a single-choice question takes its first option, and a multi-select selects **nothing**. The config is saved only when the provisioner reports that it configured something; a provisioner that aborted, errored or timed out exits **non-zero** and writes nothing. A topic with no provisioner (`channels`) runs its section instead: because the caller asked for that one section by name, a run in which it could not run exits **non-zero** — even though the same section is optional in a full sweep. Scripts should check the exit code.

Single-topic examples:
- `rantaiclaw setup provider` — re-run provider section only
- `rantaiclaw setup channels` — re-run channels section only
- `rantaiclaw setup approvals` — pick the Manual / Smart / Strict / Off preset; a preset already on disk is kept unless `--force` is given
- `rantaiclaw setup knowledge` — set the Knowledge Base API keys (`[knowledge].embedding_api_key` / `vision_api_key`, encrypted at rest; vision falls back to the embedding key). Env `KB_EMBEDDING_API_KEY` / `KB_EXTRACT_VISION_API_KEY` override config at load, with `OPENROUTER_API_KEY` as the final fallback. See [config-reference.md](config.md) for the gateway `GET`/`PUT /api/v1/config/knowledge` endpoints.
- `rantaiclaw setup whatsapp-web --non-interactive` — headless WhatsApp Web QR pairing (a three-minute window, the same as the console's; a timeout exits non-zero)

`setup` writes the config it loaded, to the same file the agent reads: `RANTAICLAW_CONFIG_DIR` if set, otherwise the active profile's `config.toml`.

`onboard` is the legacy entry point, kept for existing scripts. It is **not** an alias for `setup` — it is a separate command with its own flags (`--interactive`, `--channels-only`, `--api-key`, `--provider`, `--model`, `--memory`), documented below. New work should use `setup`.

### `onboard`

- `rantaiclaw onboard`
- `rantaiclaw onboard --interactive`
- `rantaiclaw onboard --channels-only`
- `rantaiclaw onboard --force`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --memory <sqlite|none>`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --model <MODEL_ID> --memory <sqlite|none>`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --model <MODEL_ID> --memory <sqlite|none> --force`

`onboard` safety behavior:

- If `config.toml` already exists, `onboard` asks for explicit confirmation before overwrite.
- In non-interactive environments, existing `config.toml` causes a safe refusal unless `--force` is passed.
- Use `rantaiclaw onboard --channels-only` when you only need to rotate channel tokens/allowlists.
- `onboard` writes `config.toml` to the same file the agent reads: `RANTAICLAW_CONFIG_DIR` if set, otherwise the active profile's `config.toml` (`~/.rantaiclaw/profiles/<name>/config.toml`). A v0.4 flat `~/.rantaiclaw/config.toml` is moved into the profile first and counts as the existing config.

### `agent`

- `rantaiclaw agent`
- `rantaiclaw agent -m "Hello"`
- `rantaiclaw agent --provider <ID> --model <MODEL> --temperature <0.0-2.0>`

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

### `ui`

- `rantaiclaw ui install [--dir <path>] [--ref <tag>] [--force] [--allow-unverified]`
- `rantaiclaw ui update [--check] [--force] [--dir <path>] [--allow-unverified]`
- `rantaiclaw ui start [--dir <path>] [--port <n>] [--host <addr>] [--gateway <url>] [--token <tok>]`
- `rantaiclaw ui stop [--dir <path>]`
- `rantaiclaw ui path [--dir <path>]`

Install and run the optional web console ([claw-ui](https://github.com/RantAI-dev/claw-ui)) into `~/.rantaiclaw/ui` (shared across profiles).

`ui install` downloads a **signed prebuilt release artifact** — no `git`, `npm`/`bun`, or on-machine build. `--ref` is now a claw-ui **release tag** (default: the pinned release), validated against `^[A-Za-z0-9._-]+$` before it's interpolated into the download URL. The archive is verified SHA256-then-cosign, in that order, before anything is extracted:

- A SHA256 mismatch always fails.
- A **missing cosign bundle fails closed.** claw-ui v0.3.0 onward is signed; v0.1.0 and v0.2.0 predate signing and carry no bundle, so installing one of those by `--ref` needs `--allow-unverified`.
- **`cosign` not installed on this host also fails closed.** It used to degrade to a warning with SHA-only verification continuing — but `SHA256SUMS` is served from the same origin as the archive, so it says nothing about where the archive came from. Pass `--allow-unverified` to proceed anyway; the command then says plainly that the artifact was not verified.

The same `--allow-unverified` flag exists on `rantaiclaw update` and `rantaiclaw ui update`. `ui start`'s interactive "update now?" prompt never proceeds unverified — an operator answering a version prompt has not been asked about signatures.

`ui start` requires **Node.js ≥ 18.18** on `PATH` and serves the installed production build directly with `node server.js`, bound to the `[ui] host` address (default `127.0.0.1`, loopback-only — the standalone server binds `0.0.0.0` by default, so loopback is a deliberate exposure-boundary default). Set `--host` (per-run) or `[ui] host` (persistent) to `0.0.0.0`/a specific IP to reach it from your LAN — the console is a full agent-control surface, so enable a login first (`rantaiclaw setup login`); when bound to loopback on a remote host, `ui start` prints a ready-to-copy `ssh -L` port-forward command. It auto-starts/reuses/restarts the gateway as needed and auto-pairs a token when the gateway requires pairing. `--gateway`/`--token` override the resolved values, which otherwise fall back to `$RANTAICLAW_GATEWAY_URL`/`$RANTAICLAW_TOKEN`, then a pre-existing `.env.local` (from an older install), then `[gateway]` config. The gateway URL, bearer token, and cookie-signing secret are passed to the `node` process as environment variables — `ui start` no longer writes them to `.env.local`.

`ui update` refreshes the console to this binary's pinned claw-ui release — a no-op ("already up to date") when the installed version already matches the pin, unless `--force`; `--check` reports availability without downloading. The installed tag is recorded in a `.version` marker so drift is detectable: `ui start` notices a lagging console and offers to update it (an interactive `[y/N]` prompt on a TTY, a one-line notice otherwise), and a successful `rantaiclaw update` reports when a newer console is available. Neither path auto-installs — updating is always an explicit `ui update`.

`ui stop` terminates the background gateway/console processes recorded by `ui start`. `ui path` prints the install directory.

### `cron`

- `rantaiclaw cron list`
- `rantaiclaw cron add <expr> [--tz <IANA_TZ>] [--agent] [--model <name>] <command|prompt>`
- `rantaiclaw cron add-at <rfc3339_timestamp> [--agent] [--model <name>] <command|prompt>`
- `rantaiclaw cron add-every <every_ms> [--agent] [--model <name>] <command|prompt>`
- `rantaiclaw cron once <delay> [--agent] [--model <name>] <command|prompt>`
- `rantaiclaw cron update <id> [--expression <expr>] [--tz <IANA_TZ>] [--command <command>] [--name <name>]`
- `rantaiclaw cron run <id>` — force-run now and record the run (does not reschedule)
- `rantaiclaw cron runs <id> [--limit <n>]` — print recent run history
- `rantaiclaw cron remove <id>`
- `rantaiclaw cron pause <id>`
- `rantaiclaw cron resume <id>`

Notes:

- `--agent` creates an AGENT job — the positional is the prompt, not a shell command. `--model` overrides the model for an agent job (ignored for shell jobs). Example: `rantaiclaw cron add --agent '0 9 * * *' 'Summarize overnight emails' --model claude-opus-4-8`.
- `cron run` reuses the same manual-run path as the `cron_run` tool and the web `POST /api/v1/cron/{id}/run`: it records to run history but does not shift the schedule or consume a one-shot.
- **The id `cron list` prints is the id every command takes.** `cron list` prints the first 8 characters of a job id, and that text can be handed back to every command that takes a job id: `cron run`, `cron pause`, `cron resume`, `cron update`, `cron remove`, `cron runs`, the `/cron` TUI commands, the `/api/v1/cron/{id}` HTTP endpoints and the `cron_*` agent tools. A full id always wins, even when it is also the prefix of another job's id. Otherwise the text is a prefix, and it must be at least 8 characters — a shorter one is refused with `Cron job id '<id>' is too short: a prefix must be at least 8 characters`, because a mistyped delete is worse than a refusal. A prefix that names exactly one job resolves to it; a prefix that names several is refused with `Ambiguous cron job id '<id>': matches N jobs — '<full id>' (<name>), '<full id>' (<name>)`, so the candidates can be picked from the message. A prefix that names nothing is refused with `Cron job '<id>' not found` (`cron runs` and `GET /api/v1/cron/{id}/runs` report an empty history instead, as they always did). The comparison is literal, not a `LIKE` pattern, so `%` and `_` in a typed id match nothing rather than everything.
- The TUI shows the id in every listing: `/cron list` rows and the cron picker rows carry the 8-character id next to the job name. Before, a named job was listed without any id.
- **Every door that writes a cron job honours `cron.enabled = true`.** With the switch off, the CLI (`cron add/add-at/add-every/once/update/remove/pause/resume/run`), the `cron_*` agent tools, the mutating HTTP endpoints (`POST /api/v1/cron`, `PUT /api/v1/cron/{id}`, `DELETE /api/v1/cron/{id}`, `POST /api/v1/cron/{id}/run`), the TUI `/cron` commands (`/cron add`, `/cron edit`, `/cron remove`, `/cron pause`, `/cron resume`) and the cron detail panel's action keys (`r` run, `p` pause/resume, `d` delete) all refuse with the same message: `cron is disabled by config (cron.enabled=false)`. The store refuses the write as well, so a door that forgets to check cannot reach it. Reads stay open: `cron list`, `cron runs`, `GET /api/v1/cron` and `GET /api/v1/cron/{id}/runs` still work — the two read-only agent tools (`cron_list`, `cron_runs`) are the exception and refuse reads too, as they always did.
- The scheduler's own bookkeeping is exempt from the gate: recording a run, re-anchoring a stale schedule, and deleting or disabling a one-shot that already fired still land, so a runtime flip of `cron.enabled` cannot make a fired one-shot fire again.
- If `scheduler.enabled = false`, jobs persist but never fire — `cron list` shows a "scheduler disabled" banner.
- Shell command payloads for schedule creation (`create` / `add` / `once`) are validated by security command policy before job persistence.
- **A shell job runs with the agent shell's environment, not the daemon's.** The job's child
  process sees an empty environment plus the allowlisted functional variables (`PATH`, `HOME`,
  `TERM`, `LANG`, the proxy variables and the cloud-tool pointers), the `[skills.entries.<n>]`
  env overlay, and the loaded skills' own tools directories appended to its `PATH`. It never
  inherits the daemon's own environment, so a provider API key or a channel token the daemon
  holds cannot be read back through a job's output. This holds for every shell job, whoever
  created it, and for every way one is run (the scheduler, `cron run`, the `cron_run` tool,
  the HTTP route, the TUI). An operator job that needs a secret reads it itself, for example
  `sh -c '. ~/.config/myjob.env && mycmd'`.
- **A job created from a chat reads only that chat.** A job that `cron_add` created
  from a chat carries that chat's channel and reply target. Its run reads the
  conversation of that chat, built the way channel dispatch builds it, and
  nothing else: not the shared tier, not another chat, not `USER.md` or
  `MEMORY.md` in the prompt. This holds for `main` and `isolated` alike. The
  origin has no thread, so a job created in a thread reads the conversation of the
  chat that holds the thread.
- **A job with no chat reads all of memory, whatever its `session_target`.** A
  job an operator creates from the CLI, the TUI or the console carries no chat.
  Its run reads all of memory, as the CLI and the daemon heartbeat do: every
  note, including notes scoped to other conversations, and `USER.md` and
  `MEMORY.md` in its prompt. This holds for `main` and for `isolated`, the
  default. `session_target` does not change the memory view of a job. Set it
  through the `cron_add`/`cron_update` tools or the HTTP API.

#### HTTP control (`/api/v1/cron*`)

The same schedules are controllable from the web console over the gateway API
(this is what powers the console's Schedules panel):

- `GET /api/v1/cron` — list jobs; `POST /api/v1/cron` — create (agent job via
  `prompt`, shell job via `command`)
- `PUT /api/v1/cron/{id}` — edit; `DELETE /api/v1/cron/{id}` — remove
- `POST /api/v1/cron/{id}/run?approved=<bool>` — force-run now (records run
  history; does not reschedule) — same manual-run semantics as the `cron_run` tool
- `GET /api/v1/cron/{id}/runs?limit=<n>` — run history

Exposure: every route is auth-gated exactly like the rest of `/api/v1` (bearer
token when pairing is required). Shell commands supplied over HTTP are
security-checked (`is_command_allowed`) before persistence — the same policy the
CLI/tool enforce, applied up-front for the HTTP surface's blast radius.

Job ids: every `{id}` takes the full job id or a prefix of it (at least 8
characters — the length `cron list` prints). On the mutating routes (`PUT`,
`DELETE`, `POST /api/v1/cron/{id}/run`) a too-short id or a prefix that names
several jobs is a `400 Bad Request`, and a missing job is a `404`.
`GET /api/v1/cron/{id}/runs` answers `200` with an empty run list for an id that
names no job — as it always did — and a `400` for a prefix that names several
jobs.

### `models`

- `rantaiclaw models refresh`
- `rantaiclaw models refresh --provider <ID>`
- `rantaiclaw models refresh --force`
- `rantaiclaw models list [--provider <ID>]`

`models list` prints the cached model catalog for a provider (falling back to a curated list when uncached), marking the default model — no network call. Defaults to the configured default provider.

`models refresh` currently supports live catalog refresh for provider IDs: `openrouter`, `openai`, `anthropic`, `groq`, `mistral`, `deepseek`, `xai`, `together-ai`, `gemini`, `ollama`, `llamacpp`, `astrai`, `venice`, `fireworks`, `cohere`, `moonshot`, `glm`, `zai`, `qwen`, and `nvidia`.

### `channel`

- `rantaiclaw channel list`
- `rantaiclaw channel start`
- `rantaiclaw channel doctor`
- `rantaiclaw channel bind-telegram <IDENTITY>`
- `rantaiclaw channel add <type> <json>`
- `rantaiclaw channel remove <name>`
- `rantaiclaw channel pair [--channel <surface>]`

`pair` mints a one-time pairing code that a user claims in chat with `/claim <code>`. It refuses with a non-zero exit when the surface is one no channel or gateway listener consumes (the message lists the valid surfaces), or when the surface is a catalog channel this config does not configure (the message points at `rantaiclaw onboard --channels-only`).

Runtime in-chat commands (Telegram/Discord while channel server is running):

- `/models`
- `/models <provider>`
- `/model`
- `/model <model-id>`

Those four are the channel server's own commands. The interactive chat's `/model` — the picker,
the saved config and the status-bar label — is a different command, documented under
[`chat`](#the-model-the-chat-runs-model).

Channel runtime also watches `config.toml` and hot-applies updates to:
- `default_provider`
- `default_model`
- `default_temperature`
- `api_key` / `api_url` (for the default provider)
- `reliability.*` provider retry settings

`add/remove` currently route you back to managed setup/manual config paths (not full declarative mutators yet).

### `integrations`

- `rantaiclaw integrations` — browse every integration grouped by category (default)
- `rantaiclaw integrations list` — same as the bare command
- `rantaiclaw integrations info <name>`

### `skills`

- `rantaiclaw skills list`
- `rantaiclaw skills show <name>`
- `rantaiclaw skills enable <name>`
- `rantaiclaw skills disable <name>`
- `rantaiclaw skills install <source>`
- `rantaiclaw skills install-deps [<slug> | --all]`
- `rantaiclaw skills inspect <slug>`
- `rantaiclaw skills update [<slug> | --all]`
- `rantaiclaw skills remove <name>`

`skills enable <name>` / `skills disable <name>` write `[skills.entries.<name>] enabled = true` / `enabled = false` in `config.toml` — the same key documented in `docs/reference/config.md`, which is also toggleable at runtime via the gateway's `PUT /api/v1/skills/{slug}/enabled`. Note the CLI takes the manifest **name** while the gateway route takes the directory **slug**; the config key is name-based either way.

`<source>` accepts a ClawHub reference, git remotes (`https://...`, `http://...`, `ssh://...`, and `git@host:owner/repo.git`), or a local filesystem path.

A ClawHub reference is either a bare slug (`weather`) or a publisher-qualified one (`@steipete/weather`). ClawHub namespaces skills per publisher, so a slug is not unique: several people can publish `weather`, and a bare slug that more than one of them uses is rejected rather than guessed at:

```
$ rantaiclaw skills install weather
`weather` is published by 4 owners on ClawHub — say which one you mean:
    @steipete/weather  https://clawhub.ai/steipete/skills/weather
    @lfengwa2/weather  https://clawhub.ai/lfengwa2/skills/weather
    ...
  For example: `rantaiclaw skills install @steipete/weather`
```

The publisher is never chosen automatically. Installing stages remote code that the agent will later read and act on, so picking one on your behalf — by popularity, by "official" badge, or by list order — would hand anyone who squats a popular slug a path onto the machine.

`skills inspect <slug>` takes the same two forms. A skill installed from a qualified reference records its publisher, and `skills update` re-fetches from that same publisher rather than resolving the slug again.

ClawHub serves a skill one file at a time, each with its own `sha256` in the version manifest. A version that lists any file without one — the key missing, an empty string, or a value that is not 64 hex characters — is refused rather than installed, and the error names the file (`clawhub: demo@1.0.0 lists run.sh without a sha256 — refusing to install unverified files`). Nothing is fetched or written before that check passes. The manifest and the bytes come from the same server, so the hash is what proves a file is the one the manifest describes; it does not vouch for the publisher.

This applies to `skills install` and `skills update` only. A skill already on disk is never re-verified, changed or removed because of it, and `skills update` of an installed skill to such a version fails while keeping the installed version.

Skill manifests (`SKILL.md`) support YAML frontmatter, `requires` gating, environment injection, and `metadata.clawdbot.install[]` recipes. `install-deps` runs the preferred host recipe (`brew`, `uv`, Node package managers, `go`, or `download`) and validates declared binaries afterward. It exits 0 only when every targeted skill has every required binary reachable afterwards — in that skill's own tools directory or on `$PATH` — and non-zero otherwise, including when a skill declares no recipe for a missing binary. A failing recipe reports the tool's exit status and the last lines of its error output.

In the TUI, `/skills` opens the local skills picker. Press `Ctrl+I` (or Tab in terminals that send Ctrl+I as Tab) on a gated skill row to run its install-deps recipe without leaving the picker.

#### Writing a skill by hand

```
/skill new "Kopi Pagi"     write a new skill
/skill edit kopi-pagi      edit one you wrote
```

Both hand the file to your `$EDITOR` (falling back to `$VISUAL`, then
`nano`/`vi`/`notepad`) and take it back when you save. The TUI is fully
suspended while the editor runs, which is why pasting a prepared `SKILL.md`
works here even though pasting into the composer historically did not — the
TUI never sees the pasted bytes.

`/skill edit` accepts either the display name or the directory name, and
**only opens skills you wrote yourself**. A ClawHub, bundled, git, or
local-path skill is refused, naming what manages it: editing one loses the
work silently, since a bundled skill is re-seeded by the next `setup` run and
a vendor-managed one by its installer. Which skills those are is recorded in
a `.origin.json` marker written at install time; skills that predate the
marker are classified from their directory's shape.

Whatever you save is validated before it replaces anything: frontmatter must
parse and carry a non-empty `name:`, and for an edit that name must be
unchanged (renaming is not supported — the name is the config key that tracks
whether a skill is enabled). If validation fails the editor **reopens** with
your text intact and the reason at the top, up to three times; after that the
staged file's path is reported so nothing is lost.

`/skill new` refuses before opening an editor if the name is unusable or
collides with an existing skill's name or directory. Editing is refused while
a response is streaming — retry once it finishes.

The web console offers the same thing as a form (`Write` button, and a pencil
on skills you wrote), backed by the routes in `docs/reference/api-v1.md`.

### API sessions

`POST /api/v1/agent/chat` records completed turns in the same `sessions.db` used by `agent -m`, the TUI, and the `/api/v1/sessions*` endpoints — the active profile's `~/.rantaiclaw/profiles/<name>/sessions/sessions.db`, so history is scoped per profile. API-created sessions use `source = "api"` and include the user message, assistant response, derived title, and end timestamp.

The same endpoint can stream partial output as Server-Sent Events when called
with `Accept: text/event-stream` or `?stream=1`; see
`docs/reference/api-v1-streaming.md` for the event schema.

### `memory`

- `rantaiclaw memory list [--category <name>] [--session <id>] [--limit <n>] [--offset <n>]`
- `rantaiclaw memory get <key>`
- `rantaiclaw memory add <key> <content> [--category <name>]`
- `rantaiclaw memory recall <query> [--limit <n>]`
- `rantaiclaw memory reindex`
- `rantaiclaw memory stats`
- `rantaiclaw memory clear [--key <key>] [--category <name>] [--yes]`

`list` shows a page: the newest entries, with the total in its header. `clear`
removes every matching entry, with or without `--category`; it is not limited to
the page `list` shows. `get` and `clear --key` match a key prefix against the whole
store, so an older note is found by its prefix too.

`add` screens content the same way the agent's own writes are screened: invisible
characters are stripped, credential-shaped tokens are redacted, and content
carrying the `[Memory context]` header is refused.

`recall` shows relevance as an absolute quality score, not as a percentage of
the best hit in that result set. Notes you saved rank before the raw
conversation rows. The header line names the search mode in use
(`keyword` or `keyword + semantic (<provider>)`) so the operator can see
whether vector scoring contributed to the result.

`stats` reports the same search mode as `recall` on the `Mode:` line.

`reindex` re-embeds memories the current embedding model cannot use — rows stored
while the embedding provider was unavailable, and rows embedded by a previous
model. Run it after changing `embedding_model` or `embedding_dimensions`; nothing
re-embeds on its own. It refuses backends that store no embeddings.

`list` reports the true entry count. The backend caps a single listing at 1,000
rows, and the output says when it is showing a subset.

### `config`

- `rantaiclaw config schema`
- `rantaiclaw config show`

`config schema` prints a JSON Schema (draft 2020-12) for the full `config.toml` contract to stdout.

`config show` prints the **active** configuration as JSON with all secrets redacted (provider/API keys, channel tokens, tunnel tokens, the gateway login hash, paired tokens, etc.). Every value in an MCP server `env` map, in a skill `env` map (`[skills.entries.<name>.env]`) and in a skill `config` map (`[skills.entries.<name>.config]`) is shown as an empty string (`null` for a value that is not a string), because the variable names are the operator's own (`DATABASE_URL`, `PGPASSWORD`) and skill `config` values reach the skill as environment variables; the names stay visible. `api_url` is shown without its `user:pass@` part and without `key`, `api_key` and `access_token` query parameters, or as `null` when it looks like a provider key. The proxy URLs lose their `user:pass@` part. It runs the same two redaction passes as `GET /api/v1/config`, which [`docs/reference/api-v1.md`](api-v1.md#get-apiv1config) lists in full.

### `completions`

- `rantaiclaw completions bash`
- `rantaiclaw completions fish`
- `rantaiclaw completions zsh`
- `rantaiclaw completions powershell`
- `rantaiclaw completions elvish`

`completions` is stdout-only by design so scripts can be sourced directly without log/warning contamination.

### `kb` (Knowledge Base)

Built by the `kb` feature, which is part of the **default** feature set — available in a normal `cargo build` / released binary. Drop it only with `--no-default-features`. See [kb.md](kb.md) for the full KB chapter (architecture, sidecars, HTTP API).

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
| `--group <name>` | Add to a knowledge-base group by name (repeatable). The group must exist; the document is attached to it once stored | _none_ |
| `--json` | Emit JSON instead of TOON | `false` |

An unknown `--group` name exits `1` with a TOON `error[not_found]` block naming the group, before the file is read or embedded, and nothing is stored. `--group` takes names here; the HTTP ingest takes group ids.

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

An unknown ID exits `1` with a TOON `error[not_found]` block naming the ID, in either mode. A soft delete of a document that is already soft-deleted is also not found and deletes nothing. `--hard` removes the document whether or not it was soft-deleted first.

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

#### `kb status`

Show whether the Knowledge Base is active, whether a key resolves (and from where), the database path, and the live document count. Never opens the store for writing.

| Flag | Description | Default |
|---|---|---|
| `--json` | Emit JSON instead of TOON | `false` |

#### `kb enable` / `kb disable`

Toggle `[knowledge].enabled` and persist it. `enable` refuses when no embedding key resolves anywhere (config or env) — it will not persist a config the gateway then rejects. `disable` keeps the credentials, so re-activation is one command. While disabled, the data subcommands (`search`, `ingest`, `list`, …) answer a `kb_disabled` TOON error with exit 1.

In the TUI, `/kb` shows the same status and `/kb enable` / `/kb disable` toggle it.

### `doctor` and `doctor models`

**These answer different questions, and the difference is the point.**

| | `rantaiclaw doctor` | `rantaiclaw doctor models` |
|---|---|---|
| Question | Is this install configured and healthy? | Which models does each provider actually serve? |
| Network | Only the `live` checks (provider ping, channel auth, MCP startup), which `--brief` and `--offline` skip | Always. It fetches each provider's catalog |
| Credentials | Checks a key **resolves**, and from where | Checks a provider **accepts** it |
| Exit code | 1 if any check fails | 0 unless probing itself errors |

A green `doctor` does **not** mean your model works — it means a credential was found. Both
commands now say which one they are in their own output.

`doctor` options:

| Flag | Description | Default |
|---|---|---|
| `--format <FORMAT>` | `text`, `json` (CI), or `brief` (one-liner, e.g. `doctor: 6/8 ok, 1 warn, 1 fail`) | `text` |
| `--brief` | Skip the slow `live` checks | `false` |
| `--offline` | Report every live check as `Info: skipped (offline)` | `false` |

Checks that never ran are listed under `[skipped]` rather than counted as passes — a check
that did not run is not a pass.

#### The checks `doctor` runs

| Check | Category | What it answers |
|---|---|---|
| `config.schema` | `config` | Does the active config parse and validate against the schema? |
| `config.provider_key` | `config` | Does a provider credential resolve, and from where? |
| `config.api_url` | `config` | Is the provider `api_url` override a usable URL? |
| `config.paths` | `config` | Do the configured workspace and data directories exist and are they writable? |
| `policy.allowlist` | `config` | Is the shell command allowlist coherent (missing, empty, or healthy)? |
| `policy.approval_owners` | `config` | Are approval owners configured? |
| `channels.dm_detection` | `config` | Can the DM-detection policy be read for the configured channels? |
| `memory.embedding` | `config` | Is the memory search mode usable (embedding provider present, index populated)? |
| `provider.ping` | `live` | Does the provider actually accept the credential? |
| `channels.auth` | `live` | Do the configured channels authenticate? |
| `mcp.startup` | `live` | Do the configured MCP servers start? |
| `daemon.registration` | `system` | Is **this** installation's daemon registered with the host service manager, and does the unit run this binary? |
| `daemon.scheduler` | `system` | Is the running daemon's scheduler component healthy? |
| `system.deps` | `system` | Are the required system dependencies present on the host? |

`--brief` skips the three `live` checks (`provider.ping`, `channels.auth`,
`mcp.startup`); `--offline` reports them as skipped without probing. Every other check
reads local state — the daemon's state file, the config, and the host service manager —
so the console's brief mode still reports on the daemon and its scheduler.

#### The daemon checks

`daemon.registration` names what it looked at: the service manager it asked, the unit
it asked for, and the binary that unit runs. A unit that runs a different binary than
the `rantaiclaw` you just invoked belongs to another installation, and the check says so
instead of reporting it as this one's service.

`daemon.registration` reports one of these registration verdicts:

| Verdict | Meaning | Severity |
|---|---|---|
| `daemon registered via <manager> as unit <unit> (runs <path>)` | This installation's unit is installed and runs this binary | ok |
| `daemon registered with <manager> as unit <unit> but not running` | The unit is installed but stopped | info |
| `… but runs <path> — not this binary (<path>)` | The unit that answered belongs to a different installation | warn |
| `unit <unit> is registered with <manager> … could not be verified — not confirmed as this installation` | The unit is installed, but the manager named no executable the check could compare — the unit may belong to another installation | warn |
| `could not query <manager> for unit <unit> — the user service manager did not answer; registration status unknown` | The service manager could not be reached, so the unit's presence is unknown — neither registered nor absent | warn |
| `daemon not registered with <manager> (optional) — looked for <units>` | No unit of that name is installed | info |

Two more lines are informational rather than registration verdicts. The first is
`init system not detected (skipped)`, printed when the host has no init system the check
can ask. The second is `development build does not talk to the service manager`, printed
when the binary is a debug or test build — the guard that keeps a developer's build away
from the operator's service manager. Both are `info`.

`daemon.scheduler` reads the daemon's own state file and reports one of:

| Verdict | Meaning | Severity |
|---|---|---|
| `healthy` | The daemon is running and its scheduler component reports `ok` | ok |
| `unhealthy — see daemon logs` | The daemon is running and its scheduler component reports `error` | fail |
| `daemon not running` | No state file, or one the daemon has stopped refreshing — start `rantaiclaw daemon` for scheduled jobs | warn |
| `not tracked yet` | The daemon is running but does not publish a scheduler component | warn |
| the raw status string | The daemon reported a status this build does not know (`starting`) | warn |

The TUI's `/doctor` panel reports the same scheduler verdict in its own row, so the two
surfaces cannot disagree.

`doctor models` options:

| Flag | Description | Default |
|---|---|---|
| `--provider <PROVIDER>` | Probe one provider instead of all known ones | all |
| `--use-cache` | Prefer cached catalogs; skip the forced live refresh | `false` |

### `update`

Self-replaces the binary from a published GitHub release. Verifies SHA-256 against
`SHA256SUMS`, swaps atomically on Unix, and on Windows stages the new binary for a self-swap
on next launch. Every run writes a snapshot first — see [`rollback`](#rollback).

| Flag | Description | Default |
|---|---|---|
| `--check` | Print the version delta only, download nothing. **Exit 1 if a newer version exists**, 0 if current | |
| `--channel <CHANNEL>` | `stable`, or `prerelease` to include alpha/beta/rc | `stable` |
| `--to <TAG>` | Pin to a specific release tag, e.g. `v0.6.2-alpha` | |
| `--allow-downgrade` | Permit `--to` an older version | `false` |

`RANTAICLAW_RELEASE_BASE_URL` redirects the download at an internal mirror.

**`stable` does not mean what it sounds like.** The channel filters on GitHub's
_prerelease_ flag (`src/lifecycle/update.rs:546`, `Channel::Stable => !r.prerelease`), not on
the tag. Every release so far is tagged `-alpha` and published with that flag **off**, so
`--channel stable` — the default — installs alpha builds. `prerelease` is a superset, not a
different set: today it resolves to the same release.

### `chat`

Starts the interactive TUI. **This is the default — `rantaiclaw` with no subcommand runs
`chat`.**

| Flag | Description |
|---|---|
| `--resume <ID>` | Resume a specific session by ID. A unique prefix of that ID is accepted and the full ID is what gets resumed; an unknown or ambiguous ID is refused with an error before the chat opens |
| `-m`, `--message <TEXT>` | Single-message mode; prints the reply and exits, no TUI |
| `--model <MODEL>` | Override the model for this run, e.g. `anthropic:claude-sonnet-4-20250514` |
| `--profile <NAME>` | Run against a different profile without changing the active marker |

`agent run -m` and `chat -m` both answer one message. `agent` is the headless entry point;
`chat` is the TUI's, and its `-m` exists so a script can use the same command a human types.

#### The model the chat runs (`/model`)

- `/model` — open the model picker
- `/model <provider:model>` — switch to a named target
- `/model <model>` — switch within the provider already in use

The picker offers what `rantaiclaw models list` prints — each enabled provider's whole
catalog, the cached one when `models refresh` or `doctor models` has written it, the curated
list otherwise — plus the model in use when its own provider's catalog does not already hold
it. That is the one list the setup model step reads too, and a `custom:<base-url>` provider
lists the models cached for that endpoint. `Up`/`Down` navigate, typing narrows the rows,
`Enter` selects, `Esc` cancels.

**`config.toml` is written only once an agent has actually been rebuilt with the new model.**
The TUI builds the provider through the same factory the agent uses, and a target that cannot
run is refused there, before anything is written: `Model not changed: <provider> cannot run
this model: <reason>`. A target that survives the check is put to the agent as a reload, and
the status bar, the running config and `default_provider`/`default_model` move together, and
only when that reload reports the new agent in place. The status bar reads `provider:model`,
the label the chat boots with. A rebuild that fails leaves all three exactly as they were and
prints the reason once: `Model not changed: the agent could not be rebuilt: <reason>`. The
model in `config.toml` is therefore always one an agent was built with, never one merely asked
for. The write the TUI makes itself is recognised by the config watcher, so a switch does not
rebuild the same config a second time. If that write then fails, the switch still stands for the
session — an agent really is running the new model — and the chat says so once, naming the error:
`⚠ Model set to <provider:model> for this session, but saving config.toml failed: <reason>`.

Picking a session (`/sessions`, `/resume`) changes neither the model in use nor the status
bar: the agent is not rebuilt, so what answers the next message is still the model the label
names. A session recorded under another model says so once, after its history is replayed,
naming that model and pointing at `/model`.

**One limit, and it is the typed form.** A row picked from the list carries its provider and
its model as two values and is never split back apart, so `Qwen/Qwen2.5-7B` and a base URL
that is only a host and a port (`custom:http://localhost:11434`) are both read correctly when
they are picked. A target typed as `/model custom:<url>:<model>` is split as text: the base
URL has to end in a path (`http://localhost:11434/v1`), and the model id has to hold no `/`.
Without a path the split lands inside the URL, and a `/` in the model id cuts it. Pick those
from the list.

### `auth`

Provider **subscription** authentication — OAuth logins and setup tokens — as opposed to the
API keys `setup provider` writes into config. Both can be present; credential precedence is
documented in [`providers.md`](providers.md).

| Subcommand | Description |
|---|---|
| `login` | Log in with OpenAI Codex OAuth |
| `paste-redirect` | Finish OAuth by pasting the redirect URL or auth code |
| `paste-token` | Paste a setup/auth token (Anthropic subscription auth) |
| `setup-token` | Alias for `paste-token`, interactive by default |
| `refresh` | Refresh an OpenAI Codex access token from its refresh token |
| `logout` | Remove an auth profile |
| `use` | Set the active auth profile for a provider |
| `list` | List auth profiles |
| `status` | Active profile and token expiry |

A stored **anthropic** profile is used for inference: `agent -p anthropic` and an
`anthropic-custom:<url>` provider send it when no config key is set and before any
`ANTHROPIC_*` env var. Profiles stored for other providers are not used for inference.
Order and header rules are in [`providers.md`](providers.md#anthropic-family-credential-order).

### `permissions`

The per-role permission model shared by **all** multi-user channels. Owners get the full
toolset; everyone else is a guest, held to a capability ceiling that is never escalated.

| Subcommand | Description |
|---|---|
| `show` | Current owners and the guest capability ceiling. An owner-only tool (for example `delegate`) listed for guests is shown as refused |
| `add <target> <value>` | Add an entry. Targets: `owner`, `tool`, `command` (aliases: `owners`, `tools`, `commands`, `cmd`) |
| `remove <target> <value>` | Remove an entry |

```bash
rantaiclaw permissions add owner 123456789          # Telegram numeric user ID
rantaiclaw permissions add owner alice              # Slack/Discord/etc username
rantaiclaw permissions add tool shell               # let guests use the shell tool
rantaiclaw permissions add command 'kubectl get *'  # ...but only these commands
```

Adding `tool shell` without a `command` entry gives guests a shell with no allowed
commands, which denies everything — the two are meant to be set together.

Two rules govern what `add` accepts:

- **Unknown guest tool names are refused.** `add tool <name>` accepts a built-in tool name
  (the list is `BUILTIN_TOOL_NAMES` in `src/tools/mod.rs`), an `mcp__*` name (accepted with a
  note, since it cannot be checked until that server is loaded), or a `skill_*` name (accepted
  with a note, since it cannot be checked until that skill is loaded). An owner-only tool such
  as `delegate` is refused, because guests can never use it. Any other name is refused and the
  config is left unchanged. `remove` is not checked, so a bad entry already on disk can be removed.
- **Wide or high-risk guest command globs are stored with a warning.** A glob whose command name
  holds `*` or `?` (for example `rm*`, which matches every command whose text starts with `rm`),
  or one the command classifier rates high risk (for example `rm -rf *`), is stored, and the
  output says what it grants. Nothing is refused for breadth alone.

### `personality`

| Subcommand | Description |
|---|---|
| `show` | Active persona preset for the current profile |
| `list` | Available presets |
| `set <preset>` | Switch preset, e.g. `rantaiclaw personality set concise_pro` |

### `profile`

Each profile is a self-contained `~/.rantaiclaw/profiles/<name>/` directory with its own
config, persona, workspace, memory, skills, sessions, policy, secrets and runtime state. The
`default` profile is created on first run.

| Subcommand | Description |
|---|---|
| `list` | All profiles |
| `create <name> [--clone <src>]` | Create, optionally cloning an existing profile |
| `use <name>` | Switch the active profile |
| `clone <src> <dst>` | Clone under a new name |
| `delete <name>` | Delete; refuses the active profile unless `--force` |
| `current` | Print the active profile name |

To switch for a single invocation without moving the active marker, use the global
`-p <name>` flag or the `RANTAICLAW_PROFILE` environment variable. Precedence is
**CLI flag > env var > active-profile file > `default`**.

### `session`

Browse the active profile's
`~/.rantaiclaw/profiles/<name>/sessions/sessions.db`. Session IDs accept a unique prefix.

| Subcommand | Description |
|---|---|
| `list [--limit N]` | Recent sessions; 50 by default |
| `get <id-prefix>` | Show a session's messages |
| `search <query>` | Full-text search across all session messages |
| `title <id-prefix> <text>` | Rename a session |

### `insights`

Cumulative session and message statistics for the active profile — the CLI parity command for
the TUI's `/insights`. No subcommands.

### `usage`

Today's token usage for the active profile's workspace, read from the same record the daily
ceiling is enforced against (`[cost] max_tokens_per_day`). No subcommands. Output shape:

```text
Token usage for 2026-10-11 (UTC)
Tokens today: 2200
Ceiling: 4000 (55% used)
By model:
  priced/model: 1800 tokens, 2 requests, cost $0.0126
  unpriced/model: 400 tokens, 1 requests, cost not reported
```

- The day is the UTC day. Records from other surfaces sharing the workspace count.
- A model with no `[cost.prices]` entry shows `cost not reported`, never `$0.0000`.
- `[cost] max_tokens_per_day = 0` prints `Ceiling: none` and still counts the tokens.
- `[cost] enabled = false` prints the date header and then `accounting off` as the only
  substantive line: nothing is counted or enforced.

### `rollback`

Undo a `rantaiclaw update`. Every update writes a snapshot of config plus active-profile state
to `~/.rantaiclaw/.update-snapshots/<timestamp>/` **and** keeps the previous binary as
`rantaiclaw.old` beside the live one. `rollback` restores both, newest snapshot by default.

| Flag | Description |
|---|---|
| `--list` | Show available snapshots and exit without restoring |
| `--snapshot <PATH>` | Restore a specific snapshot rather than the newest |
| `-y`, `--yes` | Skip the confirmation prompt |

### `uninstall`

Removes profile data, and optionally the binary. By default it removes **only the active
profile**. It calls `service uninstall` first when a daemon unit is installed, and makes a
best-effort attempt to comment out `PATH` amendments the installer added to shell rc files.

| Flag | Description |
|---|---|
| `--all` | Remove every profile and the whole `~/.rantaiclaw/` root |
| `--purge` | Full wipe **and** self-delete the binary |
| `--keep-secrets` | Wipe profiles but keep `.secret_key` |
| `--dry-run` | Print the plan, change nothing |
| `-y`, `--yes` | Skip the confirmation prompt |

`--dry-run` first is the cheap habit here: `--all` and `--purge` are not recoverable, and
`rollback` does not undo them.

## Validation Tip

To verify docs against your current binary quickly:

```bash
rantaiclaw --help
rantaiclaw <command> --help
```
