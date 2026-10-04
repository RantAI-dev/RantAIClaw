# Per-role channel permissions (owner / normal user)

Status: implementing. Builds on the unified-agent-runtime approval model
(`approval_owners` / `can_approve` / shared `run_structured_loop`).

## Problem

Today a channel has two gates: a **chat allowlist** (who may talk) and
`approval_owners` (who may *approve* a gated tool). A non-owner who triggers a
privileged tool makes the bot ask an owner to approve — an online round-trip,
and it's all-or-nothing. There's no way to say "share my bot: my friend can use
it for safe things (and a few specific commands like `kubectl get`), but can't
run arbitrary privileged tools." This is the feature.

## Model: two roles, per turn

- **Owner** = sender in `channels_config.approval_owners`. Full toolset; turns run
  under the normal autonomy policy (their `shell` etc. still subject to the
  existing approval/allowlist).
- **Normal user (guest)** = allowed to chat (channel allowlist) but NOT an owner.
  Their turns run under a **capability ceiling**:
  - tools filtered to `guest_allowed_tools`. A tool the gate treats as
    owner-only (for example `delegate`) is refused even when it is listed, and
    the permissions summary shows it as refused,
  - if `shell` is permitted, commands must match `guest_allowed_commands`
    (globs, same matcher as the existing command allowlist) — **out-of-list =
    hard deny**, never escalated to the owner.
  - A guest's system prompt describes the guest's turn, not the host. It leaves
    out:
    - the owner's profile (`USER.md`), notes (`MEMORY.md`), `BOOTSTRAP.md` and
      `TOOLS.md` (its scaffold asks the owner for SSH hosts and device
      nicknames);
    - the absolute workspace path, which contains the OS user name. The
      workspace section says only that file paths are relative to the bot's
      workspace;
    - the `Host:` line of the runtime section;
    - the host's timezone. It reads `Timezone: UTC`;
    - the `<location>` of each skill, which is a path on the host. In Full mode
      that path is absolute, and in Compact mode it is absolute for a skill
      outside the workspace. The skill list keeps each name and description,
      and in Full mode its instructions and tools;
    - the owner's name and timezone in the persona. The persona calls the
      person in the chat "the user";
    - every tool the guest may not call, and every instruction to use one. That
      includes the instruction to schedule a reminder with `cron_add`: a prompt
      carries it only when the caller's tools hold `cron_add`, and the gate
      treats it as owner-only, so a guest never gets it. The guest prompt built
      at start-up has no tool list, no task section and no tool-use protocol. Each message adds them from the ceiling as reloaded,
      so an edit to `guest_allowed_tools` applies to the next message. The
      tool list is the tools `guest_allowed_tools` permits, each with its
      description. The task section tells the guest to use them, or, when the
      guest has no allowed tool, to answer from the conversation, and says it
      has no tools. On a provider without native tool calling the tool-use
      protocol block lists those tools only and its example calls the first of
      them. A guest with no allowed tool gets no block and no instruction to
      emit `<tool_call>` tags. On a provider with native tool calling the
      tools are the specs sent with each request. Under the Strict preset the
      safety section promises a guest only the reads it has among `file_read`,
      `memory_recall` and `web_search_tool`. A guest with no tool is told none
      of its tools run, and a guest whose tools are none of those three reads
      is promised nothing. The `shell` line follows the guest's tools. Under
      the Smart and Manual presets on a channel, the safety section says that
      reading files and recalling memory run without a gate only for the
      tools among `file_read` and `memory_recall` that the guest has, and says
      nothing about reads for a guest that has neither.

    It keeps `SOUL.md` and `IDENTITY.md`, since they describe the bot, and the
    skill list without locations. It leaves out `AGENTS.md`: that scaffold
    names tools a guest may not have, and an operator's own copy can hold
    anything.
  - A guest who is allowed `file_read`, `file_write`, `pdf_read`, or
    `image_info` is still denied access to `MEMORY.md`, `USER.md`,
    `BOOTSTRAP.md`, `MEMORY_SNAPSHOT.md`, `TOOLS.md`, and anything under
    `memory/`; the check runs again after the path is resolved, so a symlink
    or an editor copy of a private file is denied too. A guest's
    `memory_store` `replaces` and `memory_forget` stay inside that guest's own
    conversation.
  - A guest's `memory_store` stores the note in that guest's own conversation,
    so it cannot overwrite or move the owner's note or another conversation's
    note. The owner's `MEMORY.md` and `MEMORY_SNAPSHOT.md` hold shared notes
    only, so a guest's core note stays out of `MEMORY.md` and out of the
    owner's system prompt. A key keeps the place it was first stored in:
    storing an existing key from another place fails and changes nothing, and
    the guest sees only "This key is already in use". That leaves a guest able
    to tell that a key exists.
  - A guest's `file_write` refuses the files that feed the owner's prompt:
    anything under `skills/`, and `AGENTS.md`, `SOUL.md`, `TOOLS.md`,
    `IDENTITY.md` and `HEARTBEAT.md` at the workspace root, and the AIEOS identity
    file when one is configured. The rule reads `AGENTS.md.`, `AGENTS.md:stream` and a
    name with trailing spaces as `AGENTS.md`. The `screenshot` tool is under the same
    two write rules, judged on where its file lands, so it cannot overwrite a
    prompt file or a private file with an image. `file_write` also checks the target before it creates any
    directory, so a refused write leaves nothing behind. `image_info` refuses a
    path that resolves outside the workspace, as `file_read` does.
    The prompt files stay readable with `file_read`, except `TOOLS.md`: the
    private-file rule above refuses it for a guest, whatever the guest prompt
    shows.
  - These guest rules cover only the four file tools (`file_read`,
    `file_write`, `pdf_read`, `image_info`) and the memory tools
    (`memory_store`, `memory_recall`, `memory_forget`). `glob_search` and
    `shell` are not subject to them, and an MCP filesystem tool reaches a guest
    only when an operator grants it.
- **Attachments follow `file_read`.** A guest's reply carries an attachment only
  when `guest_allowed_tools` includes `file_read`, and then only a local file a
  guest `file_read` could return. The runtime filters the reply before it is
  sent, since an attachment marker needs no tool call. The filter runs
  `file_read`'s checks in `file_read`'s order. It withholds a URL, a path the
  security policy refuses (so an absolute path under the default
  `workspace_only`), a file under `memory/`, `USER.md`, `MEMORY.md`,
  `BOOTSTRAP.md`, `MEMORY_SNAPSHOT.md` and `TOOLS.md` (also through a symlink),
  a path that resolves outside the workspace, anything that is not a regular
  file, a file larger than 10 MiB (`file_read`'s limit), and any SQLite
  database under any name or its journal files (`-wal`, `-shm`, `-journal`).
  The filter judges against the workspace the upload resolves, read for each
  reply, and refuses every attachment when it cannot resolve that workspace.
  It reads markers and path-only replies from the text alone and never asks the
  filesystem whether the file exists. Two guest-visible effects follow. An
  unclosed marker fragment is removed from the reply, and a reply that is only
  a file name with a known extension is withheld even when no such file exists.
  A reply that is only a file path, which Telegram uploads without a marker, is
  judged the same way, after the runtime's `Noted:` line is set aside. A
  refused attachment is replaced by one closing line in the reply. The four
  error texts that end a failed turn (context window, provider capability,
  provider error and timeout) pass the same filter, on the draft and on the
  plain send. A guest's turn does not stream into a Telegram draft: the guest
  sees the placeholder until the filtered reply replaces it. An owner's draft
  still streams. The guest's prompt offers attachments only under the same
  grant, without the absolute workspace path. Owner replies are not filtered.
  Approval prompts, command replies and the other runtime messages never
  upload a file, for any sender, because only the model's reply and a cron
  job's announced output may.
  An approval prompt shows square brackets in a tool's name or arguments as
  parentheses and backticks as apostrophes, so an argument holding
  `[DOCUMENT:x]` reads `(DOCUMENT:x)` to the owner.
- **Runtime commands follow who the command changes.** A command that spends
  the owner's keys or changes what another person sees runs only for an owner,
  and a command that changes only the sender's own conversation runs for the
  sender. The owner answer is the one the rest of the turn uses
  (`approval_owners`, so `["*"]` makes everyone an owner). A guest who sends an
  owner-only command gets one line saying it is for the owner. Nothing changes
  and the message does not reach the model.

  | Command | Changes | Guest | Owner |
  |---|---|---|---|
  | `/models`, `/model` with no argument, `/start`, `/help`, an unknown command | nothing | runs | runs |
  | `/models <provider>`, `/model <model-id>` | the conversation's provider or model, and its history | refused | runs |
  | `/new`, `/clear` in a direct chat | the sender's own history | runs | runs |
  | `/new`, `/clear` in a group, or in a chat the platform did not mark as direct | a history several people share | refused | runs |

  `/start`, `/help` and the unknown-command reply list only the commands the
  sender can run in that chat. Pairing and approval replies (`/bind`, `/claim`,
  `/approve`, `/deny`) are handled before these commands and keep their own
  checks.
- **Secure default:** empty `approval_owners` ⇒ everyone is a guest; empty
  `guest_allowed_*` ⇒ guests get only chat, the agent calls no tool on a
  guest's behalf. Nobody gets privileged capability until an owner opts them
  in.

This subsumes the sharing case, removes the approval ping-pong for guests, and
makes a `["*"]` chat allowlist safe (public for safe stuff, private for privileged).

## Channel session storage

Every addressed message on a connected channel is recorded as an open session
in `sessions.db`, alongside the TUI and API sessions. Channel sessions are
private to the daemon; the recording path has no ownership check, so a guest
turn produces the same row as an owner's, but a guest cannot read any
session. Guest turns run under a tool gate that does not include a session
reader, and no session id or URL-shaped value is handed back to the guest.
The operator's review surfaces (CLI `--source channel`, TUI
`/sessions channel`, `GET /api/v1/sessions?source=channel`) require the
operator path, which guests do not have. See
[Channel Session Recording](../reference/channels.md#channel-session-recording)
for storage location, retention, and the deletion paths.

## What stays open

These paths are known and not closed by the rules above.

- **Cron announcements.** A cron job's announced output uploads the files it
  names to the chat the owner chose, with no guest filter. If that chat holds
  guests, they receive the file. Only an owner can create the job, and the
  attachment must still resolve inside the workspace.
- **`shell`.** A guest granted `shell` can write any file an allowed command
  writes, create hard links and read private files. The operator names the
  commands in `guest_allowed_commands`, and chaining, redirects and `$` are
  refused. The grant is the capability.
- **`git_operations`.** A guest granted it can run `checkout` and `stash`, which
  rewrite tracked workspace files from content already committed, and `add`
  and `commit`, which change history. The guest cannot author a committed
  prompt file, because `file_write` and `screenshot` refuse it.
- **Hard links.** The write rules compare paths, not inodes, so a second hard
  link to a prompt file under another name is not recognised. Creating one
  needs `shell`. Refusing every file with more than one link would refuse
  ordinary files too.
- **Windows short names.** A short name such as `AGENTS~1.MD` for a prompt file
  that does not exist yet is not recognised. An existing file resolves to its
  real name.
- **Inbound image markers.** An `[IMAGE:path]` marker in a guest's message reads
  a workspace image into the provider request. It stays inside the workspace
  and the type comes from the extension, but the private-file rule does not
  apply. Pointing it at a private file needs a link named like an image, which
  needs `shell`. The data goes to the operator's provider, not to the chat.
- **The AIEOS identity file through `screenshot` or `shell`.** `file_write`
  refuses it. `screenshot` writes only an image, which breaks the load and
  falls back to the workspace files. `shell` is covered above.
- **A bare file name.** A guest's reply that is only a file name with a known
  extension is withheld even when no such file exists. This follows from not
  asking the filesystem. A reply that names a real ordinary workspace file
  still passes.
- **A link swapped after the reply filter.** The reply filter judges the
  canonical path of an attachment, and the channel resolves the path again when
  it uploads. A sender who can write in the workspace between the two steps can
  swap a link in between. A guest needs `shell` for that, a tool the operator
  granted, so the window is accepted.

Two gaps have no fix yet, so treat them as operator guidance.

- Do not grant `browser` to guests. Its `screenshot` action writes to the path
  the model names, and that path is not confined to the workspace. It reaches
  any file the process may write, the config file included, and it bypasses the
  guest write rules, so a guest with `browser` can overwrite a prompt file, a
  private file, the AIEOS file or the config file with an image.
- `glob_search` lists the names of every workspace file, including those under
  `memory/` and `USER.md`. It never returns content, but the names are visible
  to a guest granted it.

## Memory view: what a turn may read

Every turn runs under a memory view that the door which started it sets. The
view decides what `memory_recall`, the `[Memory context]` block in front of the
user's message, the `replaces` and `contains` lookups of `memory_store` and
`memory_forget`, and the capacity notice of `memory_store` may read. A turn that
no door gave a view reads nothing: the recall finds no note and the block is
empty. A door that forgets to set a view fails closed.

| Door | View |
|---|---|
| Channel turn, a named owner in a direct chat | all of memory |
| Channel turn, a named owner in a group, or in a chat the platform did not mark as a direct message | that conversation only |
| Channel turn, a sender who is an owner only through `approval_owners = ["*"]` | that conversation only, in a direct chat as well |
| Channel turn, a guest | that conversation only |
| TUI, `agent -m`, `chat -m`, the web console chat | all of memory |
| Daemon heartbeat | all of memory (its reply goes to the journal and the observer, never to a chat) |
| Cron job created from a chat | that chat's conversation, for `main` and `isolated` |
| Cron job with no chat, `main` or `isolated` | all of memory |
| Webhook (`POST /webhook`, `POST /triggers/{path}`) | none: reads nothing and writes nothing |
| A delegated sub-agent | the view of the turn that delegated |

- **A named owner** is an identity written in `approval_owners`. The `"*"` entry
  lets any allowed sender approve tool calls and names nobody, so a sender who is
  an owner only through it keeps approval rights and never gets the view of all
  of memory. This needs no new key.
- **`USER.md`, `MEMORY.md`, `BOOTSTRAP.md` and `TOOLS.md` in the prompt follow
  the view.** Only a turn that sees all of memory carries them. A turn under one
  conversation or with no view does not: an owner in a group, a wildcard owner, a
  cron job created from a chat and a webhook turn get a prompt without them. Such
  an owner keeps the owner persona, the workspace path and the host line, and the
  chat-kind line tells an owner in a group that the owner's private notes are
  available only in a direct chat with the bot. A channel builds the owner prompt
  on every message and reads those files then, so a note deleted since the last
  message is gone from the next one without a restart. The guest prompt carries
  none of them and is built once when the channel starts. The prompt of the
  interactive TUI or CLI session is built when the session starts.
- **A note keeps the place it was written in.** `memory_store` puts a note where
  the turn's view puts it. It refuses a key that already holds a different note
  unless `replaces` names it, and identical content is not an error.
  `memory_forget` deletes only what the turn's view can see, by key and by
  `contains`. A turn with no view, a webhook turn included, can neither store
  nor delete a note: both tools refuse it before any lookup with one answer
  that names no key. The console, the CLI
  `memory` commands and the TUI `/memory` commands are the operator's own
  surfaces: they run under the view of all of memory, may write to any place,
  and replace a note on purpose.
- **The file tools split their rules by what they do.** The read rule follows
  the view: `file_read`, `pdf_read` and `image_info` refuse the owner's private
  files (`USER.md`, `MEMORY.md`, `memory/` and the rest) in every turn that does
  not read all of memory. That covers a turn under a conversation view, an owner
  in a group included, and a turn with no view, a webhook included. The write
  rule follows guest status, not the view: `file_write` refuses the private
  files, `skills/` and the owner prompt files to a guest only. An owner in a
  group, an owner through `approval_owners = ["*"]` and a cron job created from a
  chat keep the write access an owner has in a direct chat.
- A turn under a conversation view also stores a `memory_store` note in that
  conversation, and `memory_forget` by key reaches that conversation's notes
  only, whoever asks.
- The webhook keeps `shell` and the file tools. Its file tools refuse the owner's
  private files, but `shell` and `glob_search` are not covered by the file rules,
  so a webhook turn can still read `MEMORY.md`, `MEMORY_SNAPSHOT.md` and
  `memory/brain.db` through them.

## Config (`[channels_config]`)

```toml
approval_owners        = ["alice", "+1555..."]      # owners (existing field)
guest_allowed_tools    = ["file_read", "web_search_tool", "shell"]  # tools a guest may use
guest_allowed_commands = ["kubectl get *", "kubectl describe *", "ls *"]  # shell globs for guests
```

- Defaults: `guest_allowed_tools = []` (⇒ the agent calls no tool on a guest's
  behalf), `guest_allowed_commands = []` (⇒ no shell for guests). The owner's
  `autonomy.auto_approve` list is **not** unioned in; operators who want a
  guest to be able to read files or recall memory list those tools here.

## Enforcement point

One place — the shared agent loop, per turn:
1. Resolve `is_owner = can_approve(approval_owners, sender)` (CLI/console ⇒ owner).
2. Owner → existing path (full registry + normal `SecurityPolicy`).
3. Guest → the loop runs on the registry entries `guest_allowed_tools` permits,
   and a `GuestGate` checks each call before execution. It denies outright
   (`GuestGate::deny_reason`) any tool outside `guest_allowed_tools`, including
   a call to a tool that was left out of the guest's list, and any shell command
   outside `guest_allowed_commands` (no union with `auto_approve`). Guests do
   not get a `SecurityPolicy` of their own: `guest_allowed_commands` reaches
   only the `GuestGate`, and the shell tool keeps the owner's policy.

Because the loop is unified, this lands on **every multi-user channel at once**:
Telegram, WhatsApp, Discord, Slack, Mattermost, Signal, Matrix, IRC, DingTalk,
Lark, QQ, Linq, Nextcloud Talk. (CLI = single local owner; console = authed owner.)

## Setup surfaces

- **CLI:** `rantaiclaw permissions ...` — owners add/remove/list, guest tools +
  commands add/remove/list. Headless-friendly.
- **TUI:** a slash command + an onboarding wizard step.
- **Chat (self-setup skill):** an **owner-only** tool the agent can call to mutate
  owners / guest allowlists, plus a `SKILL.md` so the owner can just say
  "add +1555 as a guest who can run `kubectl get`" and the agent does it.
  The tool MUST verify the requesting sender is an owner before applying
  (security-critical — chat-driven config mutation).

## Schema

Additive fields → bump `config::migrations::CURRENT_VERSION` 4→5 (no-op arm +
test), regenerate the `config_schema@v5` drift snapshot.

## Release

Version bump + CHANGELOG + PR + CI green + merge + tag (new alpha).
