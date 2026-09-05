# Plan 118: Scope conversation history by chat and thread, not by sender alone

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs src/channels/history_store.rs src/channels/conversation.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/117 (serialized chain over `src/channels/mod.rs`)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Conversation history is keyed by `channel_sender` alone. One person's private DM,
every group the bot shares with them, and every forum topic collapse into a single
thread — so turns from a private conversation are injected verbatim into the prompt
when that same person next speaks in a public group. That is a content leak between
chats, not a UX wart, and it is persisted to `brain.db` so it survives restarts.

Three scoping schemes disagree inside one function: the interruption key already
includes the reply target, the memory scope is already thread-aware, and history is
neither. The `ConversationKey` type that exists to solve this documents the intent
and history never received it.

Two adjacent defects in the same data path: `ConversationKey::resolve` joins its
parts with `:` while Matrix user ids contain a colon, so two distinct conversations
can produce one id; and the durable history table is rewritten whole on every turn
and never pruned, so cost per turn grows with conversation length and rows
accumulate for every sender that ever messaged the bot.

## Current state

`src/channels/mod.rs:318-320` — the history key:

```rust
fn conversation_history_key(msg: &ChannelMessage) -> String {
    format!("{}_{}", msg.channel, msg.sender)
}
```

`:322-324` — the sibling key in the same file, which *does* scope by chat:

```rust
fn interruption_scope_key(msg: &ChannelMessage) -> String {
    format!("{}_{}_{}", msg.channel, msg.sender, msg.reply_target)
}
```

`:1707-1709` — and the memory scope, which *is* thread-aware:

```rust
    let conversation_scope = ConversationKey::new(&msg.channel, &msg.sender)
        .in_thread(msg.thread_ts.as_deref());
```

`src/channels/conversation.rs:43-47` documents the intent history never got:

```rust
/// Attach a thread/topic sub-scope so it resolves to its own conversation…
/// Discord/Slack threads resolve to their own conversation instead of being
/// merged into the parent channel.
```

`src/channels/conversation.rs:50-61` — and the ambiguous join:

```rust
/// Deterministic and collision-free across surfaces.
    pub fn resolve(&self) -> String {
        match &self.thread {
            Some(t) => format!("{}:{}:{}", self.surface, self.sender, t),
            None => format!("{}:{}", self.surface, self.sender),
        }
    }
```

Matrix senders are `@localpart:homeserver` (`src/channels/matrix.rs:161-172`), so
`matrix:@bob:example.org` is produced by two different inputs.

`src/channels/telegram.rs:1130-1173` — Telegram puts the chat id and forum
`message_thread_id` only into `reply_target`, and hardcodes `thread_ts: None`. So
one user's DM and every group they share with the bot share `telegram_<user>`.

`src/channels/history_store.rs:109-131` — `save()` serialises the entire turn list
and upserts the whole blob, once per message; `:76-103` — `load_all()` reads every
row at boot; `updated_at` is written and read by nothing.

`src/channels/mod.rs:409-440` — `normalize_cached_channel_turns` ends in
`_ => {}`, silently discarding any turn whose role is not `user` or `assistant`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |
| Memory restart test | `cargo test --test memory_restart` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/channels/mod.rs` — the history key, route-override key, `normalize_cached_channel_turns`
- `src/channels/conversation.rs` — the join and its docstring
- `src/channels/history_store.rs` — retention and the per-turn write shape

**Out of scope**:
- Populating `thread_ts` on channels that currently hardcode `None`. That is a
  feature (plan 147's spike) and this plan must work correctly whether or not
  `thread_ts` is present — keying on `reply_target` is what fixes the leak.
- Dead code (119), the factory (120), decomposition (121).
- The memory subsystem's own scoping — it is already correct; only read it.

## Git workflow

- Branch: `fix/scope-history-by-chat-and-thread`
- Conventional commits, e.g. `fix(channels): scope conversation history by chat, not by sender`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Make the conversation id unambiguous

Percent-encode `:` in `sender` and `thread` before joining in
`ConversationKey::resolve`, or switch to a length-prefixed join. Correct the
docstring — today it claims a collision-free property the function does not have,
which invites callers to rely on it.

**Verify**: `cargo test --lib channels::conversation` → all pass.

### Step 2: Key history by the conversation, not the person

Replace `conversation_history_key` with a `ConversationKey`-derived value that
includes the reply target and the thread:

```rust
    ConversationKey::new(&msg.channel, &msg.reply_target)
        .in_thread(msg.thread_ts.as_deref())
        .resolve()
```

Use the **same** value for `route_overrides` (`:1682`, `:809-829`) so `/model`
pinning follows the conversation rather than following the person across every chat
they are in. There must be one scheme in this file when you are done, not three.

Decide explicitly whether a group conversation is per-chat or per-sender-per-chat.
Per-chat matches the memory scope and is the recommended default; say which you
chose in the PR.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 3: Migrate the persisted keys

Existing rows in `channel_history` are keyed by the old shape. Do not leave them to
rot and do not silently orphan live threads.

At startup, delete rows whose key matches the legacy `channel_sender` shape, logging
the count once. A one-time reset of channel history is acceptable and honest; a
silent orphan is not. Put the reasoning in a comment so the next reader knows the
deletion is deliberate.

**Verify**: `cargo test --test memory_restart` → all pass.

### Step 4: Stop rewriting the whole history per turn, and add retention

- Change `save()` from a whole-blob upsert to an append, or keep the blob but cap the
  stored turn count per key so the write is bounded.
- Add an age-based prune at startup using the `updated_at` column that already exists
  for exactly this and is currently read by nothing. Choose a default retention
  window, make it a config key only if there is a concrete reason to vary it (§3.2 —
  do not add a knob speculatively).

**Verify**: `cargo test --lib channels::history_store` → all pass.

### Step 5: Make the dropped-turn case explicit

Replace `_ => {}` in `normalize_cached_channel_turns` with an arm that either passes
the turn through or logs the dropped role at debug. Nothing writes non-user/assistant
turns today, so this is a trap rather than a live bug — but the store is a general
message vector and the loss would be permanent after the next compaction.

**Verify**: `cargo test --lib channels::` → all pass.

## Test plan

1. `dm_and_group_history_do_not_merge` — same sender, two different reply targets;
   assert the two histories are independent. **This is the test that pins the leak.**
2. `threads_resolve_to_their_own_conversation` — same sender and reply target,
   different `thread_ts`.
3. `matrix_sender_does_not_collide_with_a_thread` — assert
   `ConversationKey::new("matrix", "@bob").in_thread(Some("example.org"))` and
   `ConversationKey::new("matrix", "@bob:example.org")` resolve differently.
4. `route_override_follows_the_conversation` — a `/model` pin in one chat does not
   apply in another chat with the same sender.
5. `legacy_history_rows_are_removed_at_startup` — seed a row with the old key shape;
   assert it is gone and the count was logged.
6. `history_write_is_bounded` — assert the per-turn write does not scale with the
   number of stored turns (instrument the serialised byte count behind `#[cfg(test)]`).
7. `old_rows_are_pruned` — seed a row with an old `updated_at`; assert it is removed.
8. `non_standard_role_is_not_silently_dropped`.

**Mutation check (required).** For test 1, restore `format!("{}_{}", msg.channel,
msg.sender)` and confirm it **fails**. For test 3, revert step 1's encoding and
confirm it **fails**. Restore both.

**Verify**: `cargo test --lib channels::` and `cargo test --test memory_restart` →
all pass, including all eight new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` and `cargo test --test memory_restart` pass,
      including the eight new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'format!("{}_{}", msg.channel, msg.sender)' src/channels/mod.rs`
      returns nothing
- [ ] History and route overrides use one key scheme, not two
- [ ] The per-chat vs per-sender-per-chat decision is stated in the PR body
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 118 updated

## STOP conditions

Stop and report back if:

- Plan 117 has not landed — this chain is serialized over `mod.rs`.
- A channel produces a `reply_target` that is **not** stable for a conversation
  (i.e. it varies per message). Keying history on it would then fragment every
  thread. Check Telegram, Discord, Slack and Matrix before proceeding; report which
  ones fail.
- The legacy-row deletion would remove history an operator would reasonably expect to
  keep, and no migration path exists. Surface it rather than deciding for them.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 147 populates `thread_ts` on more channels; once
  it lands, thread scoping starts doing real work on those platforms, and this plan's
  key must already handle it — which is why step 2 includes it now rather than later.
- **What a reviewer should scrutinise**: that route overrides and history ended up on
  the **same** key. Leaving them different reintroduces the class in a quieter form.
- **Deliberately deferred**: the channel-side memory budget difference (channels use
  4 entries where the agent path uses 5). It is real, it is recorded, and it is not
  a scoping bug.
