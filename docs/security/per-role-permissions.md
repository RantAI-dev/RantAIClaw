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
  - tools filtered to `guest_allowed_tools`,
  - if `shell` is permitted, commands must match `guest_allowed_commands`
    (globs, same matcher as the existing command allowlist) — **out-of-list =
    hard deny**, never escalated to the owner.
  - Guests never see the owner's profile (`USER.md`) or notes (`MEMORY.md`) in
    the system prompt, and a guest's persona carries neither the owner's name
    nor the owner's timezone. A guest who is allowed `file_read`, `file_write`,
    `pdf_read`, or `image_info` is still denied access to `MEMORY.md`,
    `USER.md`, `BOOTSTRAP.md`, `MEMORY_SNAPSHOT.md`, and anything under
    `memory/`; the check runs again after the path is resolved, so a symlink
    or an editor copy of a private file is denied too. A guest's
    `memory_store` `replaces` and `memory_forget` stay inside that guest's own
    conversation.
  - A guest's `memory_store` stores the note in that guest's own conversation,
    so it cannot overwrite or move the owner's note or another conversation's
    note. A guest's own core note still appears in the owner's `MEMORY.md`
    until the projection filters by place. A key keeps the place it was first
    stored in: storing an existing key from another place fails and changes
    nothing, and the guest sees only "This key is already in use". That leaves
    a guest able to tell that a key exists.
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
  file under `memory/`, `USER.md`, `MEMORY.md`, `BOOTSTRAP.md` and
  `MEMORY_SNAPSHOT.md` (also through a symlink), and any SQLite database
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

## Config (`[channels_config]`)

```toml
approval_owners        = ["alice", "+1555..."]      # owners (existing field)
guest_allowed_tools    = ["file_read", "web_search", "shell"]  # tools a guest may use
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
3. Guest → full registry still runs, but a `GuestGate` checks each call before
   execution and denies outright (`GuestGate::deny_reason`) any tool outside
   `guest_allowed_tools` or any shell command outside `guest_allowed_commands`
   (no union with `auto_approve`), plus a guest-scoped `SecurityPolicy`
   (`allowed_commands = guest_allowed_commands`, out-of-list denied,
   forbidden-paths still apply).

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
