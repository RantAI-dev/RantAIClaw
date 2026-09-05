# Plan 127: Telegram — per-recipient typing, char-based draft gate, stable identity

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat f189422..HEAD -- src/channels/telegram.rs`
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged first.
> That is expected and is not a stop condition. Relocate by symbol name and continue.
> STOP only if the *code itself* no longer matches the "Current state" excerpt
> semantically — i.e. the logic changed, not its position.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: plans/115 (Telegram already implements `apply_allowed_senders` there — do not re-add it)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Telegram is the most-used channel and the only one ever verified against a live
platform. Three defects, none catastrophic, all user-visible.

Two concurrent conversations fight over one typing handle, so starting typing for
chat B silently kills chat A's indicator — and with the runtime's parallel message
path, concurrent chats are the normal case, not an edge case. Streaming drafts are
gated in **bytes** against a character limit, so CJK and emoji-heavy replies are
truncated at roughly a third of the intended length — and the neighbouring function
reasons this out correctly, so the fix was worked out once and never carried over.
And the sender identity prefers the changeable `@username`, which pairing writes into
`approval_owners`; Telegram usernames can be released and re-registered, so whoever
takes the handle inherits owner authority.

## Current state

`src/channels/telegram.rs:313` — one typing slot for the whole channel:

```rust
    typing_handle: Mutex<Option<JoinHandle<()>>>,
```

`:2327-2330` — and `stop_typing` ignores the recipient entirely:

```rust
    async fn stop_typing(&self, _recipient: &str) -> anyhow::Result<()> {
        let mut guard = self.typing_handle.lock();
        if let Some(handle) = guard.take() {
            handle.abort();
        }
```

`src/channels/discord.rs:525-526` and `:531-537` — the working reference: a
`HashMap<String, JoinHandle>` keyed by recipient, removed by key.

`:2302-2320` — `start_typing` also spawns its **own** 4-second refresh loop, while
the runtime's `spawn_scoped_typing_task` (`src/channels/mod.rs:1620-1648`) already
calls `start_typing` on a 4-second interval. Every tick aborts the task spawned four
seconds earlier and spawns another.

`:1916` — the draft gate, in bytes:

```rust
        if rendered.len() > TELEGRAM_MAX_MESSAGE_LENGTH {
```

`:1979-1984` — `finalize_draft`, done correctly, with a comment saying why:

```rust
        // Gate on the rendered length in `chars` — the splitter's unit —
        // not raw `len()` bytes.
```

`:1085-1098` — identity selection:

```rust
        let sender_identity = if username == "unknown" {
            sender_id.clone().unwrap_or_else(|| "unknown".to_string())
        } else {
            username.clone()
        };
```

`:1104-1108` — the numeric id **is** carried in `sender_aliases`, so an owner recorded
by numeric id is not demoted. The exposure is the other direction: pairing
(`src/channels/pairing.rs:141-149`) writes **every** supplied identity into
`approval_owners`, including the mutable handle.

`:148-150` — `strip_tool_call_tags`'s doc justifies itself by "Telegram's Markdown
parser will reject them (causing status 400 errors)", but the channel now returns
`RenderTarget::TelegramHtml` (`:1837-1839`) and `escape_html` turns a literal `<`
into `&lt;`, so an unstripped tag can no longer produce a 400. `:240-241` — when a tag
is unterminated, the function pushes the raw tags back into the output, contradicting
its own contract.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::telegram` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/telegram.rs`

**Out of scope**: `src/channels/pairing.rs` (what pairing writes to `approval_owners`
is plan 122's territory — this plan changes only which form Telegram *reports*);
`src/channels/mod.rs`'s `spawn_scoped_typing_task` (plan 116/121 chain);
`src/channels/discord.rs` and `src/channels/mattermost.rs`, which have the same
single-slot typing bug — Mattermost is plan 129's.

## Git workflow

- Branch: `fix/telegram-typing-draft-and-identity`
- Conventional commits, e.g. `fix(telegram): key typing indicators by recipient`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Key typing by recipient

Change `typing_handle` to `Mutex<HashMap<String, JoinHandle<()>>>`, insert and remove
by recipient, and make `stop_typing` use its argument. Copy the shape from
`src/channels/discord.rs:506-537`.

**Verify**: `cargo test --lib channels::telegram` → all pass.

### Step 2: Remove the inner refresh loop

The runtime refreshes on a 4-second cadence already. Make `start_typing` a single
fire-and-forget POST rather than a self-refreshing task, so the two layers stop
fighting.

Confirm the runtime's cadence is shorter than Telegram's ~5-second indicator expiry
before deleting the inner loop — it is (4s), but check rather than assume.

The existing tests at `:2491-2531` assert the single-slot shape and will need
updating. **Update them to assert the new behaviour, not to make them pass.**

**Verify**: `cargo test --lib channels::telegram` → all pass.

### Step 3: Gate the draft in characters

Change `:1916` to `rendered.chars().count()` and cut on `char_indices()` against a
character budget, matching the reasoning already written at `:1979-1984`.

**Verify**: `cargo test --lib channels::telegram` → all pass.

### Step 4: Prefer the immutable identity

Make `sender` the numeric `from.id` and move the username into `sender_aliases`.
`can_approve_any` already matches any alias form, so an owner listed by username
stays recognised after the swap — but the primary form becomes the one that cannot be
transferred.

**This changes the conversation-scope key**, so existing Telegram threads start a
fresh history. Note it in the PR and in the CHANGELOG; do not let it be a surprise.

Add a `permissions show` warning for any Telegram `approval_owners` entry that is not
numeric, so an operator can see which of their owner entries are transferable.

**Verify**: `cargo test --lib channels::telegram` → all pass.

### Step 5: Correct the tool-tag stripper's rationale and its fallback

Update the doc comment at `:148-150` to state the real reason — hiding internal tool
syntax from the user, not avoiding a parse error, which the HTML render target
already prevents. Make the unterminated-tag branch **drop** the tag text rather than
re-emitting it, so the function honours its contract.

Do **not** delete the function. Leaked tool-call XML is ugly even when it does not
error, and removing it is a product decision.

**Verify**: `cargo test --lib channels::telegram` → all pass.

## Test plan

1. `concurrent_typing_handles_are_independent` — mirror
   `src/channels/discord.rs:731`: starting typing for B must not stop A.
2. `stop_typing_only_stops_that_recipient`.
3. `draft_gate_counts_characters_not_bytes` — a CJK string just under the character
   limit but over the byte limit must not be truncated.
4. `sender_is_the_numeric_id_and_the_username_is_an_alias`.
5. `owner_listed_by_username_is_still_recognised` — the alias path must keep working.
6. `unterminated_tool_tag_is_dropped_not_reemitted`.

**Mutation check (required).** For test 1, restore the single-slot handle and confirm
it **fails**. For test 3, restore `rendered.len()` and confirm it **fails**. Restore
both.

**Verify**: `cargo test --lib channels::telegram` → all pass, including all six.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::telegram` passes, including the six new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'typing_handle: Mutex<Option' src/channels/telegram.rs` returns nothing
- [ ] `grep -n 'rendered.len() >' src/channels/telegram.rs` returns nothing
- [ ] The history-reset consequence of step 4 is stated in the PR and CHANGELOG
- [ ] No files outside `src/channels/telegram.rs` are modified (`git status`)
- [ ] `plans/README.md` status row for 127 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — Telegram's `apply_allowed_senders` is added there, and
  re-adding it here creates a conflict.
- Removing the inner typing loop (step 2) makes the indicator visibly lapse when
  driven against a real bot. The runtime cadence is 4s against a ~5s expiry, so it
  should not — but this is the one channel that can be live-verified, so verify it.
- Step 4's identity change breaks a test asserting the username form **deliberately**.
  Read the test's name and comment before assuming it is wrong.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 122 decides what pairing writes into
  `approval_owners`; this plan only changes which form the channel reports. Both are
  needed to close the handle-takeover path.
- **What a reviewer should scrutinise**: that step 2's deletion did not leave the
  indicator dependent on a cadence that only holds for Telegram, and that step 4's
  history-scope consequence was actually written down rather than discovered by a
  user.
- **Deliberately deferred**: the ~1,235 unread production lines in this file (the
  media-send family, `listen`, `parse_update_message`). They are the largest remaining
  unread surface in the subsystem; read them under a follow-up rather than expanding
  this plan.
