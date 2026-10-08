# Privacy

This page says what RantaiClaw writes to disk about conversations and notes, who can read it back, how long it stays, how to delete it, and what leaves the machine. Where channels behave differently, the page names the difference.

## Who reads what

One RantaiClaw serves one person. Every named identity in `approval_owners` is treated as that person, and reads the whole memory only in a direct chat on a channel that marks direct chats. An owner through `"*"` never does.

Each channel turn reads memory through a memory view:

- A named owner in a direct chat reads all of memory.
- Everyone else reads only the notes of the conversation they are in. That covers a guest, a named owner in a group, a named owner in a chat the platform did not mark as direct, and an owner through `"*"`.
- A turn that no door gave a view reads nothing. Recall finds no note, the `[Memory context]` block is empty, and the `memory_store` and `memory_forget` tools refuse the call. The `POST /webhook` turn runs with no view.

A conversation is one chat, or one thread inside a chat. Everyone in a group chat shares its conversation, so a note stored in a group is readable by every sender whose turn reads that conversation.

Five channels mark direct chats: Telegram (private chats), WhatsApp Web, Discord (messages outside a guild), Lark (`p2p` chats) and Slack in Socket Mode (`im` conversations). Every other channel in the catalog marks none, so a named owner in a one-to-one chat there reads the conversation's notes only. The signals are listed in [DM detection](../reference/channels.md#0a-dm-detection). The channel catalog marks eleven channels "under development": Mattermost, Webhook, iMessage, Matrix, Signal, Linq, Nextcloud Talk, Email, IRC, DingTalk and QQ.

The operator's own surfaces run under all of memory: the `rantaiclaw memory` commands, the TUI chat and `/memory` commands, the console chat, `POST /api/v1/memory` and `DELETE /api/v1/memory/{key}`. `GET /api/v1/memory`, `GET /api/v1/memory/{key}` and `GET /api/v1/memory/stats` set no view and read the store directly. [Per-role channel permissions](per-role-permissions.md#memory-view-what-a-turn-may-read) lists every door.

## What is stored, and what is scrubbed

### Notes

Notes live in the `memories` table of `brain.db`. These doors write a note:

- the `memory_store` tool,
- `rantaiclaw memory add`,
- `POST /api/v1/memory` in the console,
- `/memory add` in the TUI,
- the one-time import of the old markdown memory files, which runs when the config loads and finds a markdown backup that is not yet imported, and only when the memory backend is `sqlite` or a `PENDING` marker exists (the config migration that retired the `markdown` backend makes that backup),
- hydration from `MEMORY_SNAPSHOT.md`, which runs when `auto_hydrate` is on, the backend is `sqlite`, `brain.db` is missing or smaller than 4,096 bytes, and the snapshot file exists.

No code saves a chat message as a note on its own. The tool, the CLI and the console screen note text before it is stored. The screen removes invisible characters, refuses text that contains the header of the memory block, and replaces a token that starts with one of the seven prefixes listed under [Channel Session Recording](../reference/channels.md#channel-session-recording) with `[REDACTED]`. `/memory add` in the TUI, the markdown import and the snapshot hydration do not screen the text.

The `MEMORY.md` block between the `rantaiclaw:memory` markers in `<workspace>/MEMORY.md` is a copy of the `core` notes that have no conversation, newest first. It holds whole entries until the next entry would pass 4,000 characters, then, when entries are left out, a footer such as `_… 3 more core memories not shown._` counts the rest. The runtime rewrites the block when memory opens on the `sqlite` backend and after the tool, the CLI, the console or the TUI stores or deletes a note. Each rewrite keeps the text outside the markers, except that the one-time markdown import can replace the whole file with the block alone. A turn that reads all of memory carries the file in its prompt.

When `snapshot_enabled` and `snapshot_on_hygiene` are both on (both default to off), the runtime also writes every `core` note that has no conversation to `<workspace>/MEMORY_SNAPSHOT.md`, with no size limit.

With `backend = "none"`, the memory backend stores nothing and reads nothing. `channel_history` and `sessions.db` below are separate stores.

### Channel recordings in `sessions.db`

After a channel turn ends with a reply, the runtime writes two rows to `<profile>/sessions/sessions.db`: the sender's text and the reply. The session row carries the conversation key (`<channel>:<chat id>`, plus `:<thread>` inside a thread), the model name and a title derived from the key. A message dropped before dispatch, such as an unaddressed group message, is not recorded. A turn that ends in an error, a cancellation or a timeout writes no recording.

Both texts pass `scrub_channel_message` first. The [Channel Session Recording](../reference/channels.md#channel-session-recording) section of the channels reference gives the rule and its limits. The recording has no limit on row size. If `sessions.db` cannot be opened, the runtime logs a warning and records no channel turn.

### Working history in `channel_history`

`channel_history` is a table in `brain.db` with one row per conversation, holding the key, the turns as JSON and the time of the last update. The runtime writes it only when `[memory] backend` is `sqlite`. With another backend the history stays in RAM until the process exits.

- The sender's text is stored as typed, before the model call, so an interrupted request keeps it. Neither `history.rs` nor `history_store.rs` calls a scrubber.
- A row has no flag for a shared or a direct chat. In a named owner's direct chat the row also holds the tool call and tool result rows of each turn, and the load at startup skips them. In any other chat the row holds the sender texts and the final replies only.
- A conversation keeps at most 50 rows. The oldest whole turns go first.

### Other sessions

The TUI (source `tui`), `rantaiclaw agent -m` turns (source `cli`) and the console chat (source `api`) store their sessions in the same `sessions.db`. No scrubber runs on them.

### Scheduled jobs

`<workspace>/cron/jobs.db` stores a job's name, prompt and command as given. Each run's output goes to `cron_runs` after `scrub_credentials` and a cut at 16 KiB. That scrubber is the key-and-value rule only, so it does not apply the token-prefix rule or the attachment redaction. `cron_jobs.last_output` holds the last run's output cut at 16 KiB, with no scrubber. A job that announces to a chat sends the same scrubbed and cut text as `cron_runs`.

## How long it stays

- **Notes.** A note stays until someone deletes it. The exception is a note in the `conversation` category: the hygiene pass deletes it when it has not been updated for `conversation_retention_days` (default 30, `0` turns this off). The pass runs when memory opens, at most once every 12 hours, and only when `hygiene_enabled` is on.
- **Channel recordings.** A message is deleted 30 days after it was written. A session is deleted when it has no message left. The sweep runs once at startup and every 24 hours while the channel runtime runs. The 30 days are fixed and have no config key.
- **`channel_history`.** The runtime prunes it at startup only. It deletes rows last updated more than 30 days ago, and rows whose key has no `:`. A daemon that runs for months does not prune it until the next start.
- **TUI, `agent -m` and console sessions.** They are never pruned.
- **`archive_after_days` and `purge_after_days`.** They act on files in `<workspace>/sessions/` and its `archive/` folder. The hygiene pass and onboarding are the only code that writes there, and onboarding only creates the empty folder. The `session_search` tool reads `<workspace>/sessions/sessions.db` when no profile is active. `sessions.db` lives in `<profile>/sessions/`, and the default workspace is `<profile>/workspace`, so under the default layout these two settings reach no recording.
- **Scheduled jobs.** `cron_runs` keeps the newest `[cron] max_run_history` runs per job (default 50).

## Where it is stored, and file modes

By default the runtime keeps its data under the profile directory, `~/.rantaiclaw/profiles/<name>/`. `sessions.db` is in `<profile>/sessions/`. The workspace directory defaults to `<profile>/workspace` and holds `memory/brain.db`, `cron/jobs.db`, `MEMORY.md` and `MEMORY_SNAPSHOT.md`. `RANTAICLAW_WORKSPACE` can point the workspace somewhere else.

The runtime sets a mode on several files and directories. These include:

- `config.toml` is written with mode 0600.
- `auth-profiles.json`, `pairing_codes.json` and `~/.rantaiclaw/.secret_key` are written with mode 0600.
- `cron/jobs.db` and its WAL and SHM files get mode 0600, on a best-effort basis.
- The profile's `secrets/` directory gets mode 0700, on a best-effort basis.
- The WhatsApp Web session database, which holds the account's keys, gets mode 0600, as does each snapshot file and `.extra` file written beside it. Its parent directory gets mode 0700. All of these are best effort.
- The Copilot token cache keeps `access-token` and `api-key.json` at mode 0600, and its directory gets mode 0700. Both are best effort. The directory is `copilot` inside the platform config directory, or `rantaiclaw-copilot-<user>` in the temp directory when no config directory is found.
- The pending OpenAI login file `auth-openai-pending.json` is written with mode 0600.

The runtime sets no mode on the profile directories, `brain.db` or `sessions.db`, and the list above is not exhaustive for other files. For every path without a mode, the operator's umask decides. The `systemd --user` installer writes the unit file with no mode. The OpenRC installer, which runs as root and uses `/etc/rantaiclaw`, sets 0750 on the workspace and the log directory, and 0600 on `config.toml`, `.secret_key` and `auth-profiles.json`.

## Groups

Channels differ in which group messages reach the agent. The table lists the gate each channel applies before dispatch.

| Channel | Marks direct chats | Gate on group messages | Setting |
|---|---|---|---|
| Telegram | yes | On: a group message reaches the agent only when it mentions the bot or replies to a bot message. Off: no gate. A direct message is never gated. | `mention_only`, default on |
| Discord | yes | On: a guild message reaches the agent only when it mentions the bot or replies to a bot message. Off: no gate. A direct message is never gated. | `mention_only`, default on |
| Slack | yes, in Socket Mode | A message in a channel, private channel or group DM reaches the agent only when it mentions the bot or replies in a thread the bot posted in. A direct message is not gated. | none, always on |
| Lark | yes | A group message reaches the agent only when it mentions the bot or replies to a bot message. | none, always on |
| WhatsApp Web | yes | A group message reaches the agent only when it mentions the bot's number or LID, or quotes a message from the bot. A direct message is not gated. | none, always on |
| Mattermost, under development | no | On: only a post that mentions the bot reaches the agent. Off: no gate. | `mention_only`, default off |
| Signal, under development | no | `group_id` unset: every direct and group message is accepted. `group_id = "dm"`: direct messages only. A group id: only that group. | `group_id` |
| Matrix, under development | no | Every message in the one room named by `room_id` is accepted. | none |

The other channels (WhatsApp Cloud API, Email, IRC, DingTalk, QQ, Nextcloud Talk, Linq and iMessage) mark no direct chats. Their group behaviour is not described here.

## How to delete

- **A note, from the console.** `DELETE /api/v1/memory/{key}` removes the note and answers `removed: true`.
- **A note, from the CLI.** `rantaiclaw memory clear --key <key>` deletes one note by key or by a unique prefix. `rantaiclaw memory clear [--category <name>]` lists up to 1,000 notes, asks for confirmation unless `--yes` is given, and deletes them, so a store with more notes needs a second run. The CLI has no `delete` or `forget` subcommand.
- **A note, from the TUI.** `/memory remove <key>` deletes the note.
- **A note, from a chat.** The `memory_forget` tool deletes by key or by a phrase the note contains, only among the notes the turn's view can read. A named owner in a direct chat can delete any note. Everyone else can delete only the notes of the conversation they are in.
- **What a delete leaves.** Each of these paths rewrites the `MEMORY.md` block and reports the sentence "A conversation that mentioned the note still holds it until /new in that chat." (the console puts it in the `note` field). A delete reaches the stored note, not a chat history that already contains its text.
- **A recorded conversation.** `DELETE /api/v1/sessions/{id}` on a channel session deletes every recording of that conversation from `sessions.db`, its `channel_history` row and the copy held by the running channel runtime. Notes and scheduled jobs made in the chat stay. The request fails with `409` while the agent is answering in that conversation. On a TUI, CLI or console session the request deletes that one session and has no such check.
- **`/new` in a chat.** `/new` and `/clear` delete the conversation's `channel_history` row and the in-memory copy, and end the open recording. The ended recording stays until someone deletes it or the 30-day sweep removes it. In a chat the platform marks as direct, any sender can run them. Elsewhere only a sender who can approve tool calls can, and an owner through `"*"` counts. The reply names `rantaiclaw memory clear --key <key>` as the way to remove notes. The commands exist on Telegram, Discord, Lark, WhatsApp Cloud API and WhatsApp Web. Slack has no reset command.
- **A whole profile.** `rantaiclaw profile delete <name>` removes the profile directory, and refuses the active profile unless `--force` is given. `rantaiclaw uninstall` removes the active profile's directory. `rantaiclaw uninstall --all` removes the whole `~/.rantaiclaw` root, and with `--keep-secrets` it keeps `~/.rantaiclaw/.secret_key`. A workspace directory outside the removed directory, for example one that `RANTAICLAW_WORKSPACE` points to, stays.

## What leaves the machine

- **The model provider** receives each request's prompt. That includes the conversation's stored history, the `[Memory context]` block of recalled notes (it is placed in front of the last user message), and, for a turn that reads all of memory, the content of `USER.md`, `MEMORY.md`, `BOOTSTRAP.md` and `TOOLS.md` in the system prompt, where those files exist and cut at a length limit.
- **An embedding provider** receives text when `[memory] embedding_provider` is not `none` (the default is `none`). The runtime sends it the text of a note when the note is stored or re-embedded, and the question text on each recall, unless the vector is already in the local cache. The providers are `openai`, `openrouter`, `minimax` and `custom:<base-url>`.
- **An OpenTelemetry collector** receives an error span that carries the error message text, when `[observability] backend` is `otel` (or `opentelemetry` or `otlp`) and the binary was built with the `observability-otel` feature. Two code paths emit that event. The heartbeat sends its error text as it is. The `/webhook` handler sends text that passed `sanitize_api_error`, which replaces the seven token prefixes, and, when the text is longer than 200 characters, cuts it at byte 200 (moved back to a character boundary) and appends `...`.
