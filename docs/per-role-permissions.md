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
- **Secure default:** empty `approval_owners` ⇒ everyone is a guest; empty
  `guest_allowed_*` ⇒ guests get only chat + read-only tools + skills. Nobody
  gets privileged capability until an owner opts them in.

This subsumes the sharing case, removes the approval ping-pong for guests, and
makes a `["*"]` chat allowlist safe (public for safe stuff, private for privileged).

## Config (`[channels_config]`)

```toml
approval_owners        = ["alice", "+1555..."]      # owners (existing field)
guest_allowed_tools    = ["file_read", "web_search", "shell"]  # tools a guest may use
guest_allowed_commands = ["kubectl get *", "kubectl describe *", "ls *"]  # shell globs for guests
```

- Defaults: `guest_allowed_tools = []` (⇒ a safe built-in read-only set + skills),
  `guest_allowed_commands = []` (⇒ no shell for guests).
- Read-only tools (`file_read`, `memory_*`) and skills are always guest-available.

## Enforcement point

One place — the shared agent loop, per turn:
1. Resolve `is_owner = can_approve(approval_owners, sender)` (CLI/console ⇒ owner).
2. Owner → existing path (full registry + normal `SecurityPolicy`).
3. Guest → filtered tool registry (`guest_allowed_tools` ∪ safe set ∪ skills) +
   a guest-scoped `SecurityPolicy` (`allowed_commands = guest_allowed_commands`,
   out-of-list denied, forbidden-paths still apply).

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
