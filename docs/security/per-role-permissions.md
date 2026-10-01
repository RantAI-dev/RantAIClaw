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
    - every tool the guest may not call, and every instruction to use one. The
      guest prompt built at start-up has no tool list, no task section and no
      tool-use protocol. Each message adds them from the ceiling as reloaded,
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
      is promised nothing. The `shell` line follows the guest's tools.

    It keeps `AGENTS.md`, `SOUL.md` and `IDENTITY.md`, since they describe the
    bot, and the skill list without locations.
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
    `IDENTITY.md` and `HEARTBEAT.md` at the workspace root. Reading them is
    unchanged. `file_write` also checks the target before it creates any
    directory, so a refused write leaves nothing behind. `image_info` refuses a
    path that resolves outside the workspace, as `file_read` does.
  - These guest rules cover only the four file tools (`file_read`,
    `file_write`, `pdf_read`, `image_info`) and the memory tools
    (`memory_store`, `memory_recall`, `memory_forget`). `glob_search` and
    `shell` are not subject to them, and an MCP filesystem tool reaches a guest
    only when an operator grants it.
- **Attachments follow `file_read`.** A guest's reply carries an attachment only
  when `guest_allowed_tools` includes `file_read`, and then only a local file a
  guest `file_read` could return. The runtime filters the reply before it is
  sent, since an attachment marker needs no tool call. It withholds a URL, a
  file under `memory/`, `USER.md`, `MEMORY.md`, `BOOTSTRAP.md`,
  `MEMORY_SNAPSHOT.md` and `TOOLS.md` (also through a symlink), and any SQLite database
  under any name or its journal files (`-wal`, `-shm`, `-journal`). A reply
  that is only a file path, which Telegram uploads without a marker, is judged
  the same way. A refused attachment is replaced by one closing line in the reply.
  The guest's prompt offers attachments only under the same grant, without the
  absolute workspace path. Owner replies are not filtered.
- **Secure default:** empty `approval_owners` ⇒ everyone is a guest; empty
  `guest_allowed_*` ⇒ guests get only chat, the agent calls no tool on a
  guest's behalf. Nobody gets privileged capability until an owner opts them
  in.

This subsumes the sharing case, removes the approval ping-pong for guests, and
makes a `["*"]` chat allowlist safe (public for safe stuff, private for privileged).

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
| The `/compress` memory flush of the TUI | all of memory |
| Daemon heartbeat | all of memory (its reply goes to the journal and the observer, never to a chat) |
| Cron job created from a chat | that chat's conversation, for `main` and `isolated` |
| Cron job with no chat, `main` or `isolated` | all of memory |
| Webhook (`POST /webhook`, `POST /triggers/{path}`) | none: reads nothing |
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
  `contains`, and deletes nothing in a turn with no view. The console, the CLI
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
