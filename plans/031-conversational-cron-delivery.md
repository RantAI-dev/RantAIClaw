# Plan 031: Conversational cron delivery — "message me every 7am" routes back to the origin chat

> **Context**: The killer cron use case is chat-driven — a user says *"kirimin gua
> pesan penyemangat tiap jam 7 pagi"* and the agent creates a scheduled **agent
> job** that messages them back. The plumbing exists (`cron_add` accepts a
> `delivery` config; the scheduler's `deliver_if_configured` pushes an agent job's
> output to telegram/discord/slack/mattermost), and the agent already knows the
> cron tools — **but nothing tells the agent the current chat's delivery target**,
> so it creates the job with NO `delivery` and the 7am output lands only in run
> history: the user never receives it. This plan threads the origin channel +
> reply target into the agent's context so a channel-originated "message me"
> request auto-fills `delivery` scoped to THAT chat — and makes the web/TUI-origin
> case (no push channel) behave honestly instead of silently promising a message.
>
> **Design invariant (the user's concern):** a scheduled message is delivered ONLY
> to `delivery.to` (the origin chat) via the scheduler's channel send. It does NOT
> appear in the web console or TUI — those are management surfaces that show the
> job + its run history, not the delivery channel. A job created from web/TUI chat
> has no push-back channel, so it must NOT claim it will "message you": its output
> is recorded in the Schedules run history (or the agent asks which configured
> channel to use).
>
> **Executor note**: Verify — `cargo fmt --all -- --check` · scoped clippy ·
> `cargo test --lib channels`. **Depends on**: nothing hard (cron_add + delivery
> already exist); pairs naturally with the rest of the cron effort. **Branch**:
> `feat/conversational-cron`. **Risk**: MED (touches channel prompt assembly +
> agent behavior; no exposure/schema change).

## Baseline evidence (confirmed against main, 2026-07-19)

- **`cron_add` supports delivery** (`src/tools/cron_add.rs:213-240`): an agent job
  accepts `delivery: DeliveryConfig { mode, channel, to, best_effort }` and passes
  it to `add_agent_job`. Agent already sees the cron tools
  (`src/agent/loop_.rs:2086-2125`; `cron_update`'s desc names `delivery`).
- **The scheduler delivers** (`src/cron/scheduler.rs:283-360`,
  `deliver_if_configured`): when `delivery.mode == "announce"`, it constructs the
  named channel and `channel.send(&SendMessage::new(output, to))`. Supported:
  `telegram`, `discord`, `slack`, `mattermost` (else "unsupported delivery channel").
- **The origin context is available but not injected**: in `process_channel_message`
  (`src/channels/mod.rs:1633+`), `msg.channel` is the channel name, `msg.reply_target`
  is the exact reply destination (used at `:1678,1773,1818,1841` for live sends),
  and `sender_is_owner` is computed (`:1750`). The system prompt is built at
  `:1754-1755`: `build_channel_system_prompt(ctx.system_prompt.as_str(),
  &msg.channel, sender_is_owner)`.
- **`build_channel_system_prompt`** (`src/channels/mod.rs:355-374`) currently only
  appends Telegram media-marker instructions (`channel_delivery_instructions`,
  `:334-341`) + owner context (`CHANNEL_OWNER_CONTEXT`, `:350-353`). **No cron /
  delivery-target guidance.** So the agent never learns the chat's `to`.
- **`msg.reply_target` == the delivery `to`**: the live reply path uses
  `SendMessage::new(msg.content, &msg.reply_target)`; a scheduled announce uses
  `SendMessage::new(output, to)`. Same destination semantics → `to = reply_target`
  routes the 7am message to exactly this chat, nowhere else.
- **Web/TUI don't go through this path**: the web `POST /api/v1/agent/chat` and the
  TUI call `crate::agent::run` without a channel/`reply_target`, and never call
  `build_channel_system_prompt`. So they have no push-back target — a cron created
  there can only record to run history.

## Scope
- **In**: `src/channels/mod.rs` (`build_channel_system_prompt` + a new
  `channel_cron_delivery_instructions` helper + its single call site + tests);
  `src/tools/cron_add.rs` (tighten the tool description with delivery semantics so
  the agent behaves correctly on every origin).
- **Out**: `src/cron/*` behavior (delivery already works), any new channel in
  `deliver_if_configured`, schema/exposure changes.

---

## Task 1 — Inject the origin channel + reply target into the channel system prompt

**Files:** `src/channels/mod.rs`.

- [ ] **Step 1 — Failing tests** (in the `channels` `tests` module — mirror how
  existing prompt tests are structured):

```rust
    #[test]
    fn cron_delivery_instruction_present_for_announce_channels() {
        let p = build_channel_system_prompt("BASE", "telegram", "123456789", false);
        assert!(p.contains("BASE"));
        assert!(p.contains("cron_add"), "must tell the agent how to schedule a delivered message");
        assert!(p.contains("123456789"), "must carry the reply target as delivery.to");
        assert!(p.contains("telegram"), "must name the origin channel");
    }

    #[test]
    fn no_cron_delivery_instruction_for_unsupported_channel() {
        // A channel the scheduler can't deliver to must NOT promise delivery.
        // Assert a phrase that ACTUALLY appears in the emitted cron instruction
        // (see the helper text) so the test fails if the guard is removed — not a
        // vacuous negative against a substring the text never contains.
        let p = build_channel_system_prompt("BASE", "irc", "#room", false);
        assert!(!p.contains("route the output back"), "irc has no announce delivery");
        assert!(!p.contains("\"mode\": \"announce\""), "irc must not get a delivery template");
    }
```

- [ ] **Step 2 — Run, confirm FAIL** (`build_channel_system_prompt` takes 3 args).

- [ ] **Step 3 — Implement.** Add the helper (beside `channel_delivery_instructions`):

```rust
/// Announce-capable channels — the set `deliver_if_configured`
/// (`src/cron/scheduler.rs`) can push a scheduled agent job's output to. Keep in
/// sync with that match.
fn channel_supports_announce_delivery(channel_name: &str) -> bool {
    matches!(channel_name, "telegram" | "discord" | "slack" | "mattermost")
}

/// Guidance so the agent, when the user asks for a scheduled/recurring message or
/// reminder, creates a `cron_add` agent job whose `delivery` routes the output
/// back to THIS chat — and nowhere else. Only emitted for channels the scheduler
/// can actually deliver to.
fn channel_cron_delivery_instructions(channel_name: &str, reply_target: &str) -> Option<String> {
    if !channel_supports_announce_delivery(channel_name) {
        return None;
    }
    Some(format!(
        "You are talking to this user on the '{channel_name}' channel (their delivery \
address is '{reply_target}'). When they ask you to send them a message, reminder, or \
report on a schedule (e.g. \"message me every morning\"), create it with the cron_add \
tool as an agent job and set delivery to route the output back to THEM here: \
delivery = {{ \"mode\": \"announce\", \"channel\": \"{channel_name}\", \"to\": \"{reply_target}\" }}. \
The scheduled output is delivered only to this chat — it does not appear anywhere else. \
Do not ask the user for their chat id; use the address above."
    ))
}
```

  Change `build_channel_system_prompt` to take `reply_target` and append the new
  instruction:

```rust
fn build_channel_system_prompt(
    base_prompt: &str,
    channel_name: &str,
    reply_target: &str,
    is_owner: bool,
) -> String {
    let mut prompt = if let Some(instructions) = channel_delivery_instructions(channel_name) {
        if base_prompt.is_empty() { instructions.to_string() }
        else { format!("{base_prompt}\n\n{instructions}") }
    } else {
        base_prompt.to_string()
    };

    if let Some(cron) = channel_cron_delivery_instructions(channel_name, reply_target) {
        if !prompt.is_empty() { prompt.push_str("\n\n"); }
        prompt.push_str(&cron);
    }

    if is_owner {
        if !prompt.is_empty() { prompt.push_str("\n\n"); }
        prompt.push_str(CHANNEL_OWNER_CONTEXT);
    }
    prompt
}
```

  Update the call site (`channels/mod.rs:1754-1755`):

```rust
    let system_prompt = build_channel_system_prompt(
        ctx.system_prompt.as_str(),
        &msg.channel,
        &msg.reply_target,
        sender_is_owner,
    );
```

  Grep for any OTHER `build_channel_system_prompt(` caller (incl. existing tests)
  and update them to pass a `reply_target` arg.

- [ ] **Step 4 — Run, confirm PASS.** `cargo test --lib channels`
- [ ] **Step 5 — Commit.**
  `git commit -m "feat(channels): tell the agent the chat's delivery target so scheduled messages route back to the origin chat"`

---

## Task 2 — Delivery-semantics guidance so web/TUI-origin behaves honestly

**Files:** `src/tools/cron_add.rs` (tool description).

The channel prompt (Task 1) handles channel origins. For web/TUI origins (no push
channel, no `build_channel_system_prompt`), the agent must NOT silently promise a
message it can't push. Encode the semantics in the tool description so the agent
reasons correctly regardless of origin.

- [ ] **Step 1 — Update the `cron_add` description** (`src/tools/cron_add.rs:62-64`):

```rust
    fn description(&self) -> &str {
        "Create a scheduled cron job (shell or agent) with cron/at/every schedules. \
For an agent job whose output should be SENT to the user, set `delivery` = \
{mode:'announce', channel, to}; the channel system prompt provides the correct \
channel + address when the request comes from a chat channel. Without `delivery`, \
the job still runs on schedule but its output is only recorded in run history \
(visible in the Schedules view) — it is NOT pushed anywhere. If the user asks to \
be messaged but you have no delivery address (e.g. the web console or TUI, which \
have no push channel), say the output will appear in the Schedules run history, or \
ask which configured channel to deliver to — do not imply a message will arrive."
    }
```

- [ ] **Step 2 — Verify** the tool still builds + its tests pass (description change
  only): `cargo test --lib tools::cron_add`.

- [ ] **Step 3 — Commit.**
  `git commit -m "docs(cron): document delivery semantics in cron_add so no-push-channel origins don't over-promise"`

---

## Behavior after this plan (per origin)

| Request origin | "message me every 7am" result |
|---|---|
| **Telegram/Discord/Slack/Mattermost** | Agent job `0 7 * * *` with `delivery={announce, <channel>, <reply_target>}` → output pushed to THAT chat only, every 7am. Not shown in web/TUI. |
| **Web console chat** | No push channel. Agent creates the job (recorded to run history, visible in the Schedules panel) and says so, or asks which channel to deliver to. Does not falsely promise a chat message. |
| **TUI chat** | Same as web — recorded to run history / detail panel; agent is honest about no push. |
| **CLI** | N/A (non-conversational; operator sets `delivery` explicitly if wanted). |

## Done criteria
- [ ] `build_channel_system_prompt` injects, for announce-capable channels, the
  origin `channel` + `reply_target` and instructs the agent to set `delivery`
  accordingly; tests pass. Non-announce channels get no such instruction.
- [ ] The call site passes `msg.reply_target`; all callers updated.
- [ ] `cron_add`'s description states the delivery semantics (push vs run-history;
  no-push-channel honesty).
- [ ] `cargo test --lib channels tools::cron_add` green; fmt + scoped clippy clean.
- [ ] Design invariant holds: scheduled delivery targets only `delivery.to`; never
  leaks into web/TUI. No schema/exposure change.

## STOP conditions
- If `msg.reply_target` is not the correct `to` for some channel (verify against
  `deliver_if_configured`'s `SendMessage::new(output, to)` per channel) — do not
  guess a different field; confirm the live-reply target equals the delivery target.
- If `build_channel_system_prompt` has callers beyond the one at `:1754` (e.g.
  tests), update every one; a missed caller is a compile error (good — it forces
  the update).
- Do NOT add a new channel to `deliver_if_configured` here (out of scope); the
  `channel_supports_announce_delivery` set must match the scheduler's exactly.

## Rollback
Per-commit revert. The prompt injection + description are additive; reverting
restores the pre-plan behavior (jobs created without delivery). No persisted
state/schema change.
