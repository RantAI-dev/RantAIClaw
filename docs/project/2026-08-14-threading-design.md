# Threading: one mechanism, and what each remaining platform costs

**Date**: 2026-08-14
**Plan**: `plans/147-threading-spike.md`
**Status**: step-1 decision recorded; Discord, Telegram and Mattermost implemented;
the remaining nine are designed, not built — deliberately.

---

## 1. The decision: `thread_ts` wins, recipient-string packing goes

Two mechanisms existed for one question.

- **`thread_ts`** — a typed `Option<String>` on `ChannelMessage` and
  `SendMessage` (`src/channels/traits.rs`), plumbed through **ten** dispatch
  sites in `src/channels/mod.rs` and the approval relay. Filled by exactly one
  channel (Slack).
- **Recipient-string packing** — `"channel_id:root_id"` stuffed into
  `SendMessage.recipient` and split apart in `send()`. Used by Mattermost, which
  is the only channel that actually threaded, and by Telegram for forum topics.

**`thread_ts` wins.** The reasoning, in the order it matters:

1. **The recipient must mean one thing.** Packing makes `recipient` mean
   "destination" *and* "reply anchor", and the two are not the same: a Discord
   thread is a destination, a Discord reply reference is not. Every consumer of
   `recipient` — the approval relay, history keying, typing indicators, the
   config API's display — has to know the packing convention or get it wrong.
2. **It is already plumbed.** Ten dispatch sites and the relay carry
   `thread_ts` today. Choosing packing would mean deleting all of that and
   re-plumbing a string convention through the same ten sites.
3. **Plan 118 keys conversation history by thread.** A typed field can be part
   of a key; a substring convention parsed at the edge cannot, safely.

The burden was on the change, because Mattermost worked and `thread_ts` did not.
Step 4 discharges it: Mattermost was migrated onto `thread_ts` with its
observable behaviour unchanged and its existing tests rewritten to assert the
same outcomes through the new field.

### The one place packing survives, on purpose

**Telegram forum topics stay in `reply_target`.** A forum topic is a
*destination* — `message_thread_id` on `sendMessage` says which topic the
message goes to, and a reply that omits it lands in the wrong place entirely.
`thread_ts` on Telegram therefore carries the **reply reference**
(`reply_to_message_id`), which is a different thing.

This is the trap the plan's reviewer note names: the forum id must not be
carried in both fields where they can disagree. It is not. The rule is:

> `reply_target` = **where** the message goes. `thread_ts` = **what** it attaches
> to once it is there.

Slack fits this too (`thread_ts` is the parent message, the channel is the
destination); so does Mattermost after the migration (`root_id` is the parent
post, `channel_id` is the destination).

---

## 2. The opt-out

Threading changes **where messages appear**, so it must be switchable without
switching off the channel. Mattermost already had `thread_replies: Option<bool>`.

Promoted to a shared default with a per-channel override, rather than a bespoke
flag per channel as each is built:

```toml
[channels_config]
thread_replies = true          # shared default (this is the default)

[channels_config.mattermost]
thread_replies = false         # per-channel override, wins where set
```

Enforcement is in **one** place: the inbound dispatch loop in
`src/channels/mod.rs` clears `thread_ts` before the message reaches the agent
when threading is off for that channel. Channels stay ignorant of the flag, so a
channel added later cannot forget to honour it — the failure mode a per-channel
flag invites.

---

## 3. What was implemented

| Channel | Inbound `thread_ts` | Outbound |
|---|---|---|
| Slack | already: parent `ts` | already: `thread_ts` |
| **Discord** | the prompting message's id | `message_reference: {message_id}` |
| **Telegram** | the prompting message's id | `reply_parameters: {message_id}` on text sends |
| **Mattermost** | `root_id`, else the post id | `root_id` (migrated off recipient packing) |

Discord's threads are channels, so a reply to the same `recipient` already lands
inside a thread; what was missing was the *reply reference*, which is what makes
a busy channel readable. Telegram attachments do not carry the reply reference —
text does. Both are noted as follow-ups below rather than silently skipped.

---

## 4. The remaining nine

Cost is "how much work beyond filling the field", assuming the pattern above.

| Channel | Mechanism | Shape | Cost | Notes |
|---|---|---|---|---|
| Matrix | `m.thread` relation in the event content (`rel_type: m.thread`, `event_id`) | thread object | **Medium** | needs a raw-content send path; matrix-sdk's typed `RoomMessageEventContent` helper for threads exists but the module does not compile in CI today (see the dependency write-up), so this cannot be validated |
| Nextcloud Talk | `replyTo` on `POST /ocs/v2.php/apps/spreed/api/v1/chat/{token}` | reply reference | **Cheap** | inbound message id is already parsed; one extra field on the send body |
| Lark/Feishu | `POST /open-apis/im/v1/messages/{message_id}/reply` — a **different endpoint**, not a field | reply reference | **Medium** | the send path must choose an endpoint based on `thread_ts`; two code paths to keep in sync |
| DingTalk | no reply/thread primitive in the stream-mode webhook response | — | **Not possible** | the session webhook posts into the conversation; there is nothing to anchor to. Record as unsupported rather than leaving `None` unexplained |
| QQ | `msg_id` on the send body already correlates a reply (passive message) | reply reference | **Cheap** | QQ already requires `msg_id` for passive replies within the 5-minute window; wiring `thread_ts` to it is mostly renaming |
| Signal | `quote` on `signal-cli`'s send (`--quote-timestamp`, `--quote-author`) | quote, not a thread | **Medium** | needs two values (timestamp **and** author), so `thread_ts` alone is insufficient — this is the one platform where the single-string field does not fit |
| Linq | no reply primitive in the Partner API send | — | **Not possible** | same treatment as DingTalk |
| IRC | none (a reply is a new PRIVMSG; some clients read `+draft/reply` message tags) | — | **Not worth it** | the tag is a draft spec and most clients ignore it |
| iMessage | AppleScript bridge has no reply-to | — | **Not possible** | |
| Email | `In-Reply-To` / `References` headers | reply reference | **Cheap** | genuinely useful — it is what makes a mail client thread the conversation. `mail_parser` already exposes `Message-ID` inbound |

### Recommended order

1. **Nextcloud Talk, QQ, Email** — cheap, and Email is the one with the largest
   user-visible payoff outside chat platforms.
2. **Lark** — medium, but its endpoint split is contained.
3. **Signal** — requires widening the field (or a second one) to carry
   `(timestamp, author)`. Do it only after deciding whether `thread_ts` becomes
   a small struct; that decision belongs with whoever owns the trait.
4. **Matrix** — blocked behind the matrix-sdk build decision
   (`docs/project/2026-08-14-dependency-decisions.md`). Building against a
   module no CI job compiles is how `matrix.rs` missed the whole `render_target`
   rollout.
5. **DingTalk, Linq, IRC, iMessage** — record as unsupported in
   `docs/reference/channels.md`, with the reason. An unexplained `thread_ts:
   None` is indistinguishable from an oversight, which is exactly how this
   backlog formed.

---

## 5. What this spike did not verify

Threading is a **placement** behaviour: a green test shows the request body
carried the right field, not where the platform put the message. The plan asks
for a live drive on a real Discord server and a real Telegram forum group.

**That drive was not performed** — this environment has no Discord bot token,
guild, or Telegram forum group to drive, and fabricating an observation would be
worse than not having one. What exists instead:

- request-shape tests asserting the exact JSON each platform receives
  (`message_reference`, `reply_parameters`, `root_id`), each proved load-bearing
  by mutation;
- Mattermost's pre-existing threading assertions, rewritten against the new
  mechanism and still passing, which is the migration's evidence.

Before this ships to users, someone with a real server should run the plan's
step 6: reply in a thread, reply in the main channel, toggle the opt-out.
