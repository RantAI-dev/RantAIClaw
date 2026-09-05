# Plan 147: Spike — finish threading (the seam is plumbed, one channel fills it)

> **Executor instructions**: This is a **design spike**, not a build-everything plan.
> Its output is a working reference implementation on two channels plus a written
> design for the rest. Follow the steps in order, run every verification command, and
> stop at the boundary in step 5 rather than continuing into the remaining channels.
> If anything in "STOP conditions" occurs, stop and report.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/traits.rs src/channels/mod.rs src/channels/mattermost.rs`
>
> **Line numbers WILL have drifted** if earlier plans merged first. Relocate by symbol
> name and continue. STOP only if the *code itself* no longer matches the "Current
> state" excerpt semantically.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW-MED
- **Depends on**: plans/118 (it changes the conversation key this work interacts with)
- **Category**: direction
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The threading abstraction is already built and threaded through the entire dispatch
path — and exactly one channel fills it.

In every group chat on Discord, Telegram forum topics, Matrix, Lark and Nextcloud
Talk, the bot's replies land flat in the channel instead of under the message that
prompted them. In a busy room that is the difference between a usable teammate and
noise, and it is the most-requested behaviour for a bot in a shared channel. Every one
of those platforms supports it natively.

The reason this is a spike rather than a straight build: Mattermost **does** thread,
through a second, incompatible mechanism — packing `"channel_id:root_id"` into the
recipient string. So the codebase holds two answers to one question, and the first
real decision is which one survives. Building nine channels on top of an unresolved
answer is how the codebase ended up with two in the first place.

## Current state

`src/channels/traits.rs:15` puts `thread_ts` on `ChannelMessage`, `:41` on
`SendMessage`, `:71` adds `in_thread()`.

`src/channels/mod.rs` threads it through **ten** dispatch sites: `:1132`, `:1696`,
`:1708`, `:1832`, `:1901`, `:2022`, `:2029`, `:2076`, `:2097`, `:2124`, `:2186` — plus
the approval relay at `src/channels/approval_relay.rs:302-330`.

Only Slack fills it: `src/channels/slack.rs:83` derives it inbound, `:115` sends it
outbound.

Hardcoded `thread_ts: None` in: `discord.rs:483`, `matrix.rs:662`, `telegram.rs`,
`lark.rs:656` and `:965`, `qq.rs:456` and `:502`, `nextcloud_talk.rs:269`,
`dingtalk.rs:364`, `signal.rs:373`, `linq.rs:301`, `irc.rs:649`, `imessage.rs:320`,
`email_channel.rs:454`.

Mattermost's parallel mechanism: `mattermost.rs:117-121`, `:180-195`, `:413` — the
recipient carries `"channel_id:root_id"`. It also has the only per-channel opt-out that
exists: `thread_replies: Option<bool>` at `src/config/schema.rs:2888`.

Platform support: Discord `message_reference`, Telegram
`message_thread_id`/`reply_to_message_id`, Matrix `m.thread` relations, Lark reply
endpoints, Nextcloud Talk `replyTo`.

Note plan 118 rekeys conversation history to include `thread_ts`, so threading and
history scoping interact directly — that plan must land first or this spike will be
measuring the wrong thing.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/discord.rs`, `src/channels/telegram.rs`,
`src/channels/mattermost.rs` (migration only), `src/config/schema.rs` (promoting the
opt-out), and a written design document under `docs/`.

**Out of scope**: the other nine channels. That is the boundary this spike exists to
establish — see step 5. Changing `thread_ts`'s type or the ten dispatch sites; they are
already correct.

## Git workflow

- Branch: `feat/threading-spike`
- Conventional commits, e.g. `feat(discord): reply in-thread when the inbound message was threaded`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Resolve the two mechanisms

Decide, and write the reasoning down before coding: does `thread_ts` win, or does the
recipient-string packing?

The recommendation is `thread_ts` — it is the typed field, it is already plumbed
through ten dispatch sites and the approval relay, and the recipient-string form makes
the recipient mean two things at once. But Mattermost works today and `thread_ts` does
not, so the burden is on the change.

**Verify**: the decision and its reasoning are committed before any channel change.

### Step 2: Generalise the opt-out

Mattermost already has `thread_replies: Option<bool>`. Promote it to a shared
`[channels_config]` default with per-channel override, rather than adding a bespoke
flag per channel as each one is built.

Threading changes **where messages appear**, so an operator must be able to turn it off
without turning off the channel.

**Verify**: `cargo test --lib config::` → all pass.

### Step 3: Implement Discord and Telegram

Populate `thread_ts` inbound and honour it in `send()`.

- Discord: `message_reference`.
- Telegram: `message_thread_id` for forum topics, `reply_to_message_id` for plain
  replies. Note Telegram currently puts the forum thread id into `reply_target` — read
  `telegram.rs:1130-1173` carefully so the two do not end up carrying the same
  information in different places.

These two cover most usage and exercise both shapes: a native thread object and a
reply-to reference.

**Verify**: `cargo test --lib channels::discord channels::telegram` → all pass.

### Step 4: Migrate Mattermost onto the trait field

Move it off recipient-string packing so there is one mechanism. This is the step that
proves the step-1 decision was implementable, and it is why Mattermost is in scope
while the other nine are not.

**Verify**: `cargo test --lib channels::mattermost` → all pass.

### Step 5: Stop, and write the design

Do **not** continue into the remaining nine channels. Write up instead:

- the per-platform mechanism for each of the nine (API field, and whether it is a
  thread object or a reply reference)
- which are cheap and which need a client change
- what the drive in step 6 showed
- a recommended order

The spike's value is the pattern plus an honest per-platform cost estimate. Nine more
channels built on an unvalidated pattern is what this plan is structured to avoid.

**Verify**: the design document is committed.

### Step 6: Drive it

Threading is a placement behaviour; a green test does not show where a message landed.

On a real Discord server and a real Telegram forum group: send a message in a thread,
confirm the reply lands **in that thread**; send in the main channel, confirm the reply
does not create a spurious thread; toggle the opt-out and confirm behaviour reverts.

Record what you observed. **This is the plan's primary evidence.**

## Test plan

1. `discord_inbound_thread_id_is_captured` and `discord_reply_targets_the_thread`.
2. `telegram_forum_topic_is_captured` and `telegram_reply_targets_the_topic`.
3. `mattermost_threading_survives_the_migration` — same observable behaviour, new
   mechanism.
4. `thread_replies_opt_out_disables_threading` — at the shared default and the
   per-channel override.
5. `non_threaded_inbound_does_not_produce_a_threaded_reply`.

**Mutation check (required).** For test 1, drop the inbound capture and confirm it
**fails**. For test 4, ignore the opt-out and confirm it **fails**. Restore both.

**Verify**: `cargo test --lib channels::` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` passes, including the five new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] The step-1 decision is committed **before** the channel changes
- [ ] Mattermost uses `thread_ts`, not recipient-string packing
- [ ] The design document covers all nine remaining channels with a cost estimate
- [ ] The step-6 drive observations are in the PR body
- [ ] No channel outside the three in scope was modified (`git status`)
- [ ] `plans/README.md` status row for 147 updated

## STOP conditions

Stop and report back if:

- Plan 118 has not landed — it changes the conversation key that includes `thread_ts`,
  and building on the old key means redoing this.
- The step-1 decision comes out **against** `thread_ts`. That inverts the plan: the
  right move is then to remove the unused field and its ten dispatch sites, which is a
  different change and needs its own plan.
- Migrating Mattermost (step 4) changes its observable behaviour in any way. It works
  today; a migration that degrades it is not a migration.
- You find yourself implementing a fourth channel. That is the boundary; write it down
  in the design instead.

## Maintenance notes

- **What interacts with this**: plan 118 rekeys history by thread, so once threading is
  populated on more channels, thread-scoped history starts doing real work — which is
  the intended outcome, and is why 118 comes first.
- **What a reviewer should scrutinise**: that Telegram's forum-thread id is not now
  carried in **both** `reply_target` and `thread_ts` with the two able to disagree.
- **Why this is a spike**: the abstraction exists and one channel uses it, so the
  question is not "can this be done" but "which of the two mechanisms is right, and
  what does each remaining platform cost". Those are answerable in three channels.
