# Handoff — inbound-media follow-ups (items 2–4)

**Date**: 2026-08-15
**Repo**: `~/project/rantai/RantAIClaw`, branch `main` at `b191653`, CI green.
**Written for**: a fresh session picking up where the channels effort stopped.

---

## Where things stand

The whole `plans/` backlog is merged. `plans/README.md` has **zero** TODO rows.
Nothing is broken; what follows is work that was consciously deferred, not
debt discovered later.

Shipped in the last stretch (all merged, main green):

| PR | What |
|---|---|
| #509–#517 | plan 121 decomposition, all 9 rows. `src/channels/mod.rs` 9,222 → 936 lines |
| #518 | Linq inbound images moved onto the shared media policy |
| #519 | **the minimax brick fix** + Telegram photos onto the media policy |

The media policy itself is `docs/security/inbound-media-policy.md`; the shared
implementation is `src/channels/media.rs` (`fetch_image`, `fetch_image_bytes`,
`accept_image_bytes`, `sniff_image_mime`, `MediaOutcome`, `ImageBytes`).

Channels already on the policy: **Discord, WhatsApp Cloud, Linq, Telegram**.

---

## Item 2 — extract Discord's `listen()` loop

**Why**: the allowlist gate *and* the `thread_ts` capture live inside the socket
loop, which no test enters. Both are currently covered only by source-position
assertions that say, in their own doc comments, that they prove the call exists
and not that it decides anything.

**Pattern to copy**: `src/channels/slack.rs` → `SlackChannel::classify_inbound`
returning a `SlackInbound` enum, with `listen()` matching on it. That extraction
is already merged and reviewed; do the same shape.

**Anchors**:
- `src/channels/discord.rs:300` — `async fn listen(`
- `src/channels/discord.rs:517` — where `attachment_markers` is already called
  inside the loop (the per-message body starts around there)
- `src/channels/mod_tests.rs` — the wiring table
  (`every_channel_listen_path_calls_its_allowlist_gate`) has a `discord` row;
  once a real behavioural test exists, that row can stay as the wiring check but
  the doc comment should stop implying Discord is uncovered.

**Done when**: an allowed/denied pair exists that fails when the gate line is
removed (mutation, not assertion).

Estimate: ~60–75 min including CI.

---

## Item 3 — inbound images on Email

**Why**: cheapest remaining channel by a wide margin and the highest value —
`mail_parser` already hands over the bytes. **No network, no credential, no
two-step lookup.** Every other remaining channel needs a fetch.

**Anchors**:
- `src/channels/email_channel.rs:436` — `for part in parsed.attachments()`
- `src/channels/email_channel.rs:442` — the current behaviour: substitutes the
  literal string `[Attachment: {name}]` and moves on

**Shape**: for each attachment part, run the bytes through
`media::accept_image_bytes(bytes, part_mime, media::max_bytes(&self.multimodal))`.
On `Ok`, emit `[IMAGE:data:…]`; on `Rejected`, emit the note. Add a `multimodal`
field + `with_multimodal` builder to `EmailChannel` and wire it in
`src/channels/factory.rs` (Discord/Telegram/Linq/WhatsApp all show the pattern).

**Watch**: email is the one channel where the sender is trivially forged. The
`From:` authentication work from plan 125 is already in place
(`require_authenticated_sender`, and the owner path refuses unauthenticated mail
unconditionally) — do not weaken it while touching this file.

Estimate: ~75–90 min.

---

## Item 4 — per-sender media budget

**Why**: the policy names this as its own known gap. Inbound media is an
unmetered cost lever for anyone the allowlist admits, and on a group channel
that is a wider set than the operator pictures.

**The decision to make first** (this is why it was not done blind): where does
the counter live?

- **In the dispatch loop's per-sender tracking** — `src/channels/dispatch.rs:834`
  (`in_flight_by_sender`, `supervisor::InFlightSenderTaskState`). One place,
  every channel, but the media fetch happens *before* dispatch, so the loop
  would have to gate on markers already produced — i.e. the bytes were already
  downloaded. Cheapest to write, does not actually save the download.
- **In `media::`, keyed by sender** — a small registry consulted by
  `fetch_image`/`fetch_image_bytes`. Saves the download, but every caller has to
  pass a sender key, and the registry is process-global state.

Recommendation: the second one, with the budget as a **constant** rather than a
new config key (a config key means a schema bump and a drift-snapshot; YAGNI
until someone asks). Document it in the policy.

Estimate: ~60–90 min once the shape is chosen.

---

## Traps that cost time this session — all verified, not folklore

1. **Check four feature combinations before pushing**, not just the default:
   `--no-default-features`, `--features hardware`, `--features browser-native`,
   `--features channel-lark`. PR #515 went red because a `use super::X;` named a
   type that only exists under `whatsapp-web`. Reference feature-gated types
   through `super::` at the use site instead of importing them.
2. **`cargo test --lib` does not run `tests/*.rs`.** PR #506 went red on
   `tests/reply_target_field_regression.rs`, which bans the literal `reply_to:`
   anywhere in `src/` — a new *parameter* named `reply_to` tripped it.
3. **A moved file is 100% "changed lines" to the strict delta gate**, so it
   reports every pre-existing lint in that file as newly introduced. Expect to
   fix a handful per extraction, and say so in the PR — a "pure move" should not
   quietly contain edits.
4. **Run the delta gate AFTER committing**, and read *its* verdict line, not the
   docs gate's. Once in this session I read the wrong output and pushed a branch
   that failed lint in CI.
5. **`#[cfg(test)]` helpers among production methods break source-scanning
   tests** that split on the first `#[cfg(test)]`. Split on
   `"\n#[cfg(test)]\nmod tests"` instead. Bit both `telegram.rs` and
   `whatsapp.rs`.
6. **Disk is 57 GB.** Never a bare `cargo test`. `rm -rf target/debug/incremental`
   frees ~20 GB and is safe. A full-disk build produces errors that look like
   real compile failures.
7. **PR ritual** (proved necessary by #487/#488): create PR → label
   `gh api repos/RantAI-dev/RantAIClaw/issues/<n>/labels -f "labels[]=ci:full"`
   → `gh pr close <n> && gh pr reopen <n>` → then watch by **run ID**, not
   `gh pr checks`. Labelling at creation time is not enough; the `opened`
   payload is already fixed.
8. **`plans/` is never committed.** Update its rows, do not `git add` it. One
   `git add -A` in this session swept 146 files into a commit and had to be
   unwound.

---

## Still waiting on the maintainer (do not start these)

1. **matrix-sdk** — patch a fork / wait with an expiry date / pin older / drop
   the channel. Today it compiles nowhere and ships in no release binary.
   Measured options: `docs/project/2026-08-14-dependency-decisions.md`.
2. **`whatsapp-web` in `default`** — costs a measured 4.5 MiB (14% of the
   binary) and 55 crates.
3. **Replace `wa-rs-ureq-http` with a reqwest transport** — removes `ureq`
   outright and brings WhatsApp Web traffic under `[proxy]`, which it currently
   bypasses. Recommended.
4. **Live drives** — threading (#506) and inbound media (#507) were never driven
   against real accounts; both plans call that their primary evidence. Needs a
   Discord server, a Telegram forum group, a WhatsApp Business account.
5. **Three CI items needing `.github/workflows/**`** (this effort was instructed
   not to touch it): the §9.1 identity gate does not run on docs-only PRs (the
   one-line patch is in #502's body); no `channel-matrix` entry in the Features
   matrix; the release config does not state whether Matrix ships.

## Deliberately not worth doing

Slack Socket Mode and Lark's `X-Lark-Signature` — neither can be verified
without a live tenant, and an unverifiable gate that *presents* as working is
worse than its absence. Threading for Signal (needs a wider field than a single
string), Matrix (blocked on #1 above), DingTalk/Linq/IRC/iMessage (no platform
primitive). `channel verify` harness (#508's matrix delivers most of the value;
a per-PR job depending on 17 third-party services would be red more than green).
