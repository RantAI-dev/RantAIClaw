# Plan 124: Lark — authenticate the webhook, fix sender identity, fix the mention gate

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/lark.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/115 (adds the `apply_allowed_senders` trait method this plan implements)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Lark carries the highest concentration of defects of any single channel, and they
stack into something worse than their parts.

In webhook mode the event endpoint authenticates nothing and binds every network
interface — so anyone who can reach the port drives the agent, and the pre-allowlist
pairing interception gives them unlimited code guesses too. The one gate that would
otherwise remain is keyed on a sender identity the channel gets wrong: it reports the
**chat id** where every other channel reports the person. That breaks owner
authority outright, and the obvious operator workaround — pasting the chat id into
`approval_owners` — silently promotes every member of that group to owner.

Two smaller defects finish the picture: the bot answers whenever *anyone* is
@-mentioned in a group, and a character is deleted from the start of every message
that has no space after the mention placeholder — which is the normal case in CJK,
Lark's primary market.

**This file is never compiled by CI.** `channel-lark` is not in any CI job, so 1,774
lines including a hand-rolled websocket codec and an axum server are unbuilt and
unlinted on every PR. Plan 143 adds the CI job; if it has not landed, you must build
locally with the feature to verify anything here.

## Current state

### 1. The webhook authenticates nothing

`src/channels/lark.rs:1060-1073` — the token is checked **only** inside the
URL-verification challenge branch, and a missing token passes:

```rust
            if let Some(challenge) = payload.get("challenge").and_then(|c| c.as_str()) {
                // Verify token if present
                let token_ok = payload
                    .get("token")
                    .and_then(|t| t.as_str())
                    .map_or(true, |t| t == state.verification_token);

                if !token_ok {
                    return (StatusCode::FORBIDDEN, "invalid token").into_response();
                }
                …
            }
```

`:1075-1104` — the ordinary event path runs with no check of any kind, and reaches
the pairing interception before the allowlist:

```rust
            // Intercept on-demand store-minted `/bind`/`/claim` pairing codes
            // before the allowlist gate (in `parse_event_payload`) …
            if state
                .channel
                .try_handle_store_pairing_payload(&payload)
                .await
            {
```

`:1127` — and it binds every interface unconditionally:

```rust
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
```

This bypasses the gateway's own `allow_public_bind` guard
(`src/gateway/mod.rs:865-871`) and contradicts CLAUDE.md §3.6, which classifies
network exposure as deny-by-default.

`encrypt_key` is accepted in config (`src/config/schema.rs:3106-3108`), prompted by
the wizard, documented — and read by no code anywhere.

### 2. The sender is the conversation, not the person

`src/channels/lark.rs:648` (websocket path) and `:960` (webhook path):

```rust
                        sender: lark_msg.chat_id.clone(),
                        reply_target: lark_msg.chat_id.clone(),
```

while the allowlist gate two lines above uses the person:

```rust
                        tracing::warn!("Lark WS: ignoring {sender_open_id} (not in allowed_users)");
```

and `/claim` persists the **open_id** into `approval_owners` (`:706`). The two can
never match, so no Lark owner is ever recognised.

### 3. The mention gate does not check who was mentioned

`src/channels/lark.rs:1236-1239`:

```rust
/// In group chats, only respond when the bot is explicitly @-mentioned.
fn should_respond_in_group(mentions: &[serde_json::Value]) -> bool {
    !mentions.is_empty()
}
```

Compare `src/channels/mattermost.rs:440`, which takes both `bot_user_id` and
`bot_username` and checks the post metadata as well as the text.

### 4. An off-by-one eats a character

`src/channels/lark.rs:1216-1226`:

```rust
    while let Some((_, ch)) = chars.next() {
        if ch == '@' {
            let rest: String = chars.clone().map(|(_, c)| c).collect();
            if let Some(after) = rest.strip_prefix("_user_") {
                let skip =
                    "_user_".len() + after.chars().take_while(|c| c.is_ascii_digit()).count();
                for _ in 0..=skip {
                    chars.next();
                }
                if chars.peek().map(|(_, c)| *c == ' ').unwrap_or(false) {
                    chars.next();
```

The `'@'` was already consumed by the `while let`, so only `skip` characters remain
to be dropped — `0..=skip` drops `skip + 1`. Traced by hand:

- `@_user_1 hello` → the extra step eats the space, result `hello` — **correct by accident**.
- `@_user_1帮我看看` → the extra step eats `帮`, result `我看看` — **a character is lost**.

The `rest` binding also allocates a copy of the entire remaining message at every `@`.

### 5. The allowlist is a plain `Vec`, so pairing never takes effect

`src/channels/lark.rs:211` holds `allowed_users: Vec<String>`. A `/claim` validates
the code, spends it, appends to config, replies "you're paired" — and the next
message is rejected against the stale in-memory list. Eleven other channels hold
this behind `Arc<RwLock<..>>` with a mutator.

## Commands you will need

`channel-lark` is not in the default feature set, so every command needs the flag.

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --features channel-lark --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --features channel-lark --lib channels::lark` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/channels/lark.rs` — everything in this plan

**Out of scope**:
- `src/config/schema.rs` — if `encrypt_key` ends up needing a schema note, record it
  in the PR for plan 144 (docs) or 146 (dead config keys) rather than editing the
  schema here.
- The CI job that would build this file — plan 143.
- `src/onboard/provision/channels/lark.rs` — the dead `if false` region branch and
  the hardcoded `use_feishu: false` are real, and they are plan 133's.
- Any other channel file.

## Git workflow

- Branch: `fix/lark-webhook-auth-and-identity`
- Conventional commits, e.g. `fix(lark): verify event callbacks and bind to a configured host`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Verify event callbacks, and refuse to start without a secret

Move authenticity checking **above** everything else in `handle_event` — before the
pairing interception and before `parse_event_payload`.

- Verify Lark's callback signature over the **raw body** (`X-Lark-Request-Timestamp`,
  `X-Lark-Request-Nonce`, `X-Lark-Signature`). Take the body as `Bytes` and hash the
  bytes you received, not a re-serialized value — plan 130 fixes the same
  verify-what-you-parse mistake in the gateway, and this file must not repeat it.
- Replace the `==` comparison on `verification_token` with a constant-time compare
  (the repo already uses `constant_time_eq` elsewhere — grep for it).
- Remove the `.map_or(true, ..)` fallback: an absent token is **not** valid.
- Refuse to start webhook mode at all when no signing secret / verification token is
  configured, mirroring the gateway's fail-closed pattern at
  `src/gateway/mod.rs:1946-1956`.

Wire `encrypt_key` decryption if the tenant has event encryption enabled, or — if
you judge that too large for this plan — make an configured-but-unread `encrypt_key`
a **startup error** rather than silently ignoring it. Silently accepting a key you
do not use is what produced this finding.

**Verify**: `cargo test --features channel-lark --lib channels::lark` → all pass.

### Step 2: Bind to a configured host, not every interface

Add a bind-host setting for the Lark callback server defaulting to `127.0.0.1`, and
gate any non-loopback bind on the same `allow_public_bind` check the gateway uses.
Do not leave `0.0.0.0` reachable without that gate.

Add the rate-limit / body-limit / timeout layers the gateway router carries
(`src/gateway/mod.rs:826-843`) — an unauthenticated-by-default pairing endpoint with
no limiter is how the code-guessing exposure arises.

**Verify**: `cargo clippy --features channel-lark --all-targets -- -D warnings` → exit 0.

### Step 3: Report the person as the sender

Set `sender` to `sender_open_id` (websocket path, `:648`) and `open_id` (webhook
path, `:960`). Leave `reply_target` as the chat id — that is correct and the send
path depends on it.

If anything downstream relied on the old value, put the chat id into
`sender_aliases` rather than leaving it in `sender`.

Add a startup diagnostic: if any `approval_owners` entry has the chat-id shape
(`oc_…`) rather than the user shape (`ou_…`), warn that it will match every member
of that conversation. Operators who worked around the bug the obvious way need to
be told.

**Verify**: `cargo test --features channel-lark --lib channels::lark` → all pass.

### Step 4: Check that the bot was mentioned

Pass the bot's own identity into `should_respond_in_group` and match it against each
mention entry's id/name, following `src/channels/mattermost.rs:440`'s two-source
approach (text span **and** metadata id).

**Verify**: `cargo test --features channel-lark --lib channels::lark` → all pass.

### Step 5: Fix the off-by-one and the per-`@` allocation

Change `0..=skip` to `0..skip`.

Replace the `chars.clone().collect::<String>()` lookahead with a byte-offset check
against the original `text`, using the `char_indices` offset the `while let` is
currently discarding. That removes the quadratic allocation.

**Verify**: `cargo test --features channel-lark --lib channels::lark` → all pass,
including the new tests below.

### Step 6: Make the allowlist runtime-mutable

Convert `allowed_users` to `Arc<RwLock<Vec<String>>>`, add an
`add_allowed_identity_runtime` mutator, call it after a successful pairing, and
implement the `apply_allowed_senders` trait method plan 115 added.

Copy the shape from `src/channels/slack.rs:40` — do not invent a variant. Use the
same lock flavour the majority of channels use; if the file already imports
`parking_lot`, match that.

**Verify**: `cargo test --features channel-lark --lib channels::lark` → all pass.

### Step 7: Read the rest of the file

~765 production lines of this file were never read by the audit — `listen_ws`
(roughly `:415-668`), the token-refresh path, and the protobuf frame handling.
Read them now and report anything you find in the PR body. Do not fix what you find
unless it is a one-line safety issue; new findings belong in a follow-up plan so
this one stays reviewable.

## Test plan

New tests in this file's test module. Note the existing tests do not run in CI —
say so in the PR until plan 143 lands.

1. `event_without_a_valid_signature_is_rejected` — a POST with no signature headers
   returns 403 and reaches neither the pairing handler nor `parse_event_payload`.
   Assert on a call counter, not just the status.
2. `challenge_without_a_token_is_rejected` — the `.map_or(true, ..)` case.
3. `webhook_mode_refuses_to_start_without_a_secret`.
4. `sender_is_the_open_id_not_the_chat_id` — two different open_ids in one chat
   yield two different `sender` values, on **both** the websocket and webhook paths.
5. `group_message_mentioning_someone_else_is_ignored` — and its twin, that a mention
   of the bot is answered.
6. `mention_strip_preserves_the_first_character_without_a_space` — assert
   `@_user_1帮我看看` yields `帮我看看`. Add the with-space case too, so the accidental
   correctness is pinned rather than relied on.
7. `pairing_grants_immediate_access` — mirror
   `add_allowed_identity_runtime_grants_immediate_access` from
   `src/channels/discord.rs:819`.

**Mutation check (required).** For test 1, delete the signature verification and
confirm it **fails**. For test 6, restore `0..=skip` and confirm it **fails**.
Restore both. This repo has shipped guards whose tests passed without them.

**Verify**: `cargo test --features channel-lark --lib channels::lark` → all pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --features channel-lark --all-targets -- -D warnings` exits 0
- [ ] `cargo test --features channel-lark --lib channels::lark` passes, including all
      seven new tests
- [ ] Both mutation checks were performed and failed as expected
- [ ] `grep -n 'map_or(true' src/channels/lark.rs` returns nothing
- [ ] `grep -n '0, 0, 0, 0' src/channels/lark.rs` returns nothing, or only inside the
      `allow_public_bind`-gated branch
- [ ] `grep -n '0..=skip' src/channels/lark.rs` returns nothing
- [ ] Step 7's read-through is reported in the PR body, with any new findings listed
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 124 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — step 6 implements a trait method that does not exist yet.
- You cannot determine Lark's callback signature scheme from the code and the
  configured fields alone. Do not guess an HMAC construction; a wrong one fails
  closed for legitimate traffic and looks like a working gate.
- `channel-lark` does not compile at all on this checkout. It should — its only
  extra dependency is `prost` — but it has never been built by CI, so accumulated
  drift is plausible. Report what broke rather than fixing unrelated code.
- Changing `sender` breaks a test that asserts the chat-id value **deliberately**.
  Read the test name and comment first; if the repo intended chat-scoped identity on
  Lark, this plan's premise is wrong and the operator must decide.

## Maintenance notes

- **What interacts with this**: plan 143 adds the CI job that builds this file. Until
  it lands, nothing here is protected against a refactor elsewhere in the repo.
  Plan 133 fixes the Lark *provisioner*, which currently hardcodes the wrong region
  and never prompts for it — an operator can therefore land here with a config
  pinned to the International endpoint even after this plan.
- **What a reviewer should scrutinise**: that step 1's verification happens before
  the pairing interception (the ordering *is* the fix), and that step 3 left
  `reply_target` alone.
- **Deliberately deferred**: routing this channel's send path through `format::split`
  (Lark posts the whole rendered reply in one request, so a long answer fails
  entirely) — that is plan 129, which owns message splitting across the remaining
  platforms.
