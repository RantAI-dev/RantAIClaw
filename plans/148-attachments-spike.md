# Plan 148: Spike — generalize attachments beyond Telegram

> **Executor instructions**: This is a **design spike**, not a build-everything plan.
> Its output is inbound image support on two channels, a written media policy, and a
> per-platform cost estimate for the rest. Stop at the boundary in step 6 rather than
> continuing. If anything in "STOP conditions" occurs, stop and report.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs src/channels/traits.rs src/channels/telegram.rs`
>
> **Line numbers WILL have drifted** if earlier plans merged first. Relocate by symbol
> name and continue. STOP only if the *code itself* no longer matches the "Current
> state" excerpt semantically.

## Status

- **Priority**: P3
- **Effort**: L
- **Risk**: MED
- **Depends on**: plans/131 (it adds the image node to the render AST this work needs outbound)
- **Category**: direction
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

A user sends the bot a screenshot on Discord, WhatsApp, Slack or Matrix — the four
platforms where sending a screenshot is the normal way to ask a question — and it is
dropped without acknowledgement.

The agent can already reason about images: the multimodal path exists and Telegram
proves it end to end. It simply cannot **receive** them anywhere people send them. And
outbound is the same story — the agent can produce a chart or a file and only Telegram
users get it.

The architecture is nearly free. The outbound seam is a one-arm `match`; the inbound
normal form (`[IMAGE:...]` markers) is already documented as a general capability and
already works. What is missing is per-platform media fetch and upload — and, more
importantly, **a policy for downloading attacker-supplied bytes into the agent's
context**, which does not exist yet and must not be invented per channel.

## Current state

`src/channels/mod.rs:331-338` — the outbound seam, one arm:

```rust
fn channel_delivery_instructions(channel_name: &str) -> Option<&'static str> {
    match channel_name {
        "telegram" => Some(/* [IMAGE:]/[DOCUMENT:]/[VIDEO:]/[AUDIO:]/[VOICE:] markers */),
        _ => None,
    }
}
```

Inbound is nearly as thin. `src/channels/telegram.rs:2252` converts a photo to a data
URI; `src/channels/linq.rs:125` converts a `media` part to an `[IMAGE:]` marker;
`src/channels/signal.rs:331` merely **detects** attachments in order to skip them;
`src/channels/email_channel.rs:191` substitutes the literal text
`[Attachment: name]`.

The marker protocol is already documented as general —
`docs/reference/channels.md:55-69` "Inbound Image Marker Protocol" — and the vision
path is provider-enforced (`:68`).

`docs/reference/channels.md:63` — remote fetch is already gated behind
`[multimodal].allow_remote_fetch`, so a policy hook exists to extend rather than
invent.

Plan 131 adds `Inline::Image` to the render AST, which is what makes outbound image
links survive rendering at all — today they are dropped by every channel.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/traits.rs` (the seam), `src/channels/mod.rs` (the
`match` → trait method), `src/channels/discord.rs` and `src/channels/whatsapp.rs`
(inbound images only), a written media policy under `docs/`.

**Out of scope**: outbound media on any channel but Telegram; inbound on the other
fourteen; non-image media types. Each of those multiplies the policy surface, and the
policy is the deliverable.

## Git workflow

- Branch: `feat/attachments-spike`
- Conventional commits, e.g. `feat(discord): accept inbound images as multimodal content`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Write the media policy first

**Before any download code.** The policy must answer:

- What **size** cap, and what happens past it — reject, truncate, or fetch a thumbnail?
- What **MIME types** are accepted? Images only, to start — and how is the type
  established, from the platform's metadata or from sniffing the bytes? (A platform's
  claimed type is attacker-influenced.)
- Where do bytes **land** — memory only, or disk? If disk, under what path, with what
  permissions, and who cleans up?
- How does this interact with `[multimodal].allow_remote_fetch`, which already gates
  remote fetches?
- What happens when the fetch **fails** — drop silently, or tell the user?
- Is there a **per-sender rate limit**? Inbound media is an unmetered cost lever for
  anyone allowed to chat.

This is downloading attacker-supplied bytes into the agent's context on the operator's
machine. Writing the policy per-channel while implementing is how each channel ends up
with a different answer.

**Verify**: the policy document is committed before step 3.

### Step 2: Turn the outbound seam into a trait method

Replace `channel_delivery_instructions`'s `match` with a `Channel` trait method:

```rust
    fn delivery_instructions(&self) -> Option<&'static str> { None }
```

Telegram overrides it with its existing markers; every other channel inherits `None`.
This is behaviour-preserving and it is what lets each channel declare its own marker
support without editing a central `match`.

**Verify**: `cargo test --lib channels::` → all pass; no behaviour change.

### Step 3: Implement inbound images on Discord and WhatsApp Cloud

For each: fetch the media (an authenticated fetch by file id), convert to the
`[IMAGE:]` normal form Telegram already produces, and apply the step-1 policy.

Pick these two deliberately — Discord's fetch is a plain authenticated URL, WhatsApp's
is a two-step media-id resolution. Between them they cover both shapes, so the cost
estimate in step 6 rests on real experience rather than guesswork.

**Verify**: `cargo test --lib channels::discord channels::whatsapp` → all pass.

### Step 4: Make the failure path visible

A dropped image must not be silent. When a fetch fails, is too large, or has an
unaccepted type, the user should learn that — a note in the forwarded content is
enough. Today Signal detects and skips with no signal at all, which is the behaviour to
avoid generalising.

**Verify**: a test asserts the rejection note reaches the content.

### Step 5: Drive it

Send a screenshot from a real Discord client and a real WhatsApp account. Confirm the
agent can describe it. Then send something oversized and something of a rejected type,
and confirm the user is told rather than ignored.

Record what you observed. **This is the spike's primary evidence** — the whole finding
is that images are dropped silently, and only a drive shows they are not any more.

### Step 6: Stop, and write the cost estimate

Do **not** continue into the other twelve channels. Write up:

- per platform: the media-fetch mechanism, whether it needs a new API surface, and a
  rough cost
- which platforms cannot support it at all
- what the drive showed, including anything the policy did not anticipate
- a recommended order, and whether outbound media is worth doing at all beyond Telegram

**Verify**: the estimate is committed.

## Test plan

1. `discord_inbound_image_becomes_an_image_marker`.
2. `whatsapp_inbound_image_becomes_an_image_marker`.
3. `oversized_media_is_rejected_with_a_note`.
4. `unaccepted_mime_is_rejected_with_a_note` — including when the platform's claimed
   type and the sniffed bytes disagree.
5. `fetch_failure_is_reported_not_silent`.
6. `delivery_instructions_default_is_none` — the step-2 move is behaviour-preserving.
7. `media_fetch_respects_allow_remote_fetch`.

**Mutation check (required).** For test 3, remove the size cap and confirm it **fails**.
For test 4, trust the platform's claimed MIME type and confirm it **fails**. Restore
both.

**Verify**: `cargo test --lib channels::` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` passes, including the seven new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] The media policy is committed **before** the fetch code
- [ ] `channel_delivery_instructions`'s central `match` is gone
- [ ] The step-5 drive observations are in the PR body
- [ ] The per-platform cost estimate covers all twelve remaining channels
- [ ] No channel outside the two in scope gained media handling (`git status`)
- [ ] `plans/README.md` status row for 148 updated

## STOP conditions

Stop and report back if:

- Plan 131 has not landed — outbound image links are dropped by the renderer until it
  does, so half the story cannot be demonstrated.
- The policy in step 1 cannot answer the disk-persistence question without a decision
  about where agent-received files live. That is an operator-facing choice; surface it.
- A platform's media fetch requires credentials beyond what the channel already holds.
  Report it; do not extend the credential surface inside a spike.
- You find yourself implementing a third channel. That is the boundary.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 131 makes outbound image URLs survive rendering;
  plan 123 covers WhatsApp Web (a different channel from WhatsApp Cloud, which this
  spike uses); plan 144 documents the marker protocol.
- **What a reviewer should scrutinise**: that the MIME check does not trust the
  platform's claimed type, and that nothing lands on disk without an explicit path and
  permission decision.
- **Why this is L and a spike**: the architecture is nearly free but the policy is not,
  and the policy is what would be gotten wrong fourteen separate times if each channel
  were built independently.
