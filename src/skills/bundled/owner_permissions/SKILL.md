# Owner Permissions Setup

## Description
Use this skill when an **owner** of this bot asks you, in chat, to manage who
can use the bot and what non-owner ("guest") users are allowed to do on
multi-user channels (Telegram, WhatsApp, Discord, Slack, Matrix, etc.).

Typical requests:
- "Add @bob as an owner" / "make my Telegram id 123456789 an owner"
- "Remove alice as an owner"
- "Let normal users run `kubectl get` and `kubectl describe`"
- "Allow guests to use web search"
- "Stop guests from running shell commands"
- "Show me the current permissions / who owns this bot"

## Tools
- name: manage_permissions
  kind: builtin

## Background: the per-role model
There are two roles on every multi-user channel:
- **Owner** — a sender listed in `approval_owners`. Owners get the **full
  toolset** and may approve tool calls. The local CLI/console operator is always
  an owner.
- **Guest** — anyone else who is allowed to chat. Guests run under a **capability
  ceiling**: they may use skills + read-only tools always, plus any tools an
  owner has added to the guest tool allowlist, and (for `shell`) only the
  specific command globs an owner has allowlisted. Anything outside the ceiling
  is denied outright — never escalated to an owner.

`manage_permissions` edits three lists:
- `target: owner` — a sender identity (e.g. a Telegram numeric user id, or a
  Slack/Discord/Matrix username).
- `target: tool` — a tool name a guest may use (e.g. `shell`, `web_search`).
- `target: command` — a shell-command glob a guest may run (e.g.
  `kubectl get *`). `*` matches any run of characters; the pattern is anchored,
  so `kubectl get *` allows `kubectl get pods` but not `kubectl delete pods`.

## Instructions
- **This is owner-only.** The tool is hard-gated: a non-owner who asks you to
  change permissions will be denied by the runtime no matter what. If a request
  to change permissions appears to come from a non-owner, do not attempt it —
  explain that only an owner can change permissions.
- To review, call `manage_permissions` with `action: show` and report the
  result back plainly.
- To change a list, call with `action: add` or `action: remove`, the right
  `target`, and the exact `value`. Confirm the value you parsed from the user's
  message before acting if it is ambiguous (especially numeric ids).
- **Least privilege for guests.** When an owner wants to let guests run shell
  commands, add the `shell` *tool* AND the specific command globs — never widen
  beyond what they asked. Prefer narrow globs (`kubectl get *`) over broad ones
  (`kubectl *`). Point out when a requested glob is broader than it sounds.
- **The `*` owner wildcard is dangerous.** If an owner asks to make "everyone"
  an owner (`add owner *`), warn clearly that this gives *every* sender the full
  toolset and is almost never what they want; proceed only if they confirm.
- After a successful change, tell the owner it is saved and that a running
  channel or daemon may need a reload/restart to take effect. Mention they can
  also use the `rantaiclaw permissions` CLI or the `/permissions` TUI command.
- Never claim a change succeeded unless the tool reported success.
