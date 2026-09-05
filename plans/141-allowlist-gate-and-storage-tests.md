# Plan 141: Test the allowlist gate as an applied guard, not as a predicate

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat f189422..HEAD -- src/channels/`
>
> **Line numbers WILL have drifted** — plans 124–129 merge before this one. Relocate by
> symbol name and continue. STOP only if the *code itself* no longer matches the
> "Current state" excerpt semantically.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/129 (all per-platform plans must be merged first)
- **Category**: tests
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The per-channel allowlist is the primary authorization boundary for every chat
platform, and it is protected by tests of a helper function the production path is
free to stop calling.

The predicate is well covered everywhere. Only **two** channels test that it is
actually applied to inbound traffic. On the other twelve the gate sits inside
`listen()` or a network handler no test reaches — delete the gate line in any of them
and every test in that file still passes. Nextcloud Talk is the sharpest case: its
gate *is* in a tested function, but every fixture uses an allowlisted actor, so there
is no negative case at all.

Alongside it, four more coverage gaps in code this effort has been changing: the
WhatsApp session store (1,345 lines, 3 tests, none covering identity, session, prekey,
sender-key or sync-key storage — the persistence layer for end-to-end crypto), the
pairing dispatcher (1 of 14 channel arms covered, and its `AllowlistField` argument is
consumed by `let _ = field;` so no test *can* catch a mismatch), durable channel
history (never exercised end to end), and nine Telegram tests that make real network
calls and assert only `is_err()`.

## Current state

Applied-gate coverage exists only at `src/channels/linq.rs:618` ("Unauthorized senders
should be filtered") and `src/channels/whatsapp.rs:534` — both via a pure
`parse_webhook_payload`.

`src/channels/nextcloud_talk.rs:218` — the gate is in a tested function, but
`make_channel()` (`:388`) sets `allowed_users: vec!["user_a"]` and every payload uses
`actorId: "user_a"`. No negative case.

Gates unreachable from any test because they live inside `listen()`:
`dingtalk.rs:319`, `discord.rs:404`, `irc.rs:597`, `lark.rs:639` and `:898`,
`matrix.rs:638`, `mattermost.rs:285` and `:396`, `qq.rs:441` and `:487`,
`signal.rs:342`, `slack.rs:218`, `whatsapp_web.rs:337`/`:610`/`:638`,
`email_channel.rs:433`, `imessage.rs:297`-region.

`src/channels/whatsapp_storage.rs:1262-1345` — the whole test module: three tests,
covering database creation, one LID round trip, and expired-token deletion. The file
implements ~53 store methods.

`src/channels/pairing.rs:117-133` — a 14-arm match; all five `apply_pairing` tests
(`:305-384`) use `"telegram"`. `:115-116`, `:137` — `field` is consumed by
`let _ = field;`, so passing the wrong `AllowlistField` for a channel is undetectable
by construction.

Every `ChannelRuntimeContext` in the `mod.rs` test module sets `history_store: None`
— so making the write-through a no-op fails nothing, and history silently stops
surviving restarts, which is the bug the module exists to fix.

`src/channels/telegram.rs:2981-3001` and eight siblings — tests that call
`https://api.telegram.org` with a fake token and assert only
`result.is_err()` plus a disjunction matching essentially any `reqwest` error.

`src/channels/approval_relay.rs` — 14 real `sleep(20–30ms)` races; the suite survives
only because CI forces `--test-threads=1`. `src/channels/mod.rs:7355-7379` uses a
fixed global component name where its two neighbours deliberately uuid-suffix theirs.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |
| Lark-gated | `cargo test --features channel-lark --lib channels::lark` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: test modules across `src/channels/`, plus the **minimum production
extraction** needed to make a gate testable — moving a per-message body out of a
socket loop into a pure `handle_inbound(payload) -> Option<ChannelMessage>`, following
the shape Linq, WhatsApp and Nextcloud Talk already have.

**Out of scope**: behaviour changes. An extraction must be a move; if a gate's
behaviour changes while you extract it, that is a bug in the extraction. Anything
plans 124–129 already own in those files — they are merged, so build on them.

## Git workflow

- Branch: `test/allowlist-gate-and-storage-tests`
- **One commit per channel extraction**, plus separate commits for the storage,
  pairing and history test groups. Extractions are reviewed differently from new
  assertions.
- Conventional commits, e.g. `test(nextcloud): assert the allowlist gate rejects an unlisted actor`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: The free one — Nextcloud Talk's negative case

Add one payload with a non-allowlisted `actorId` and assert the parse yields nothing.
No extraction needed; the gate is already in a tested function.

Then delete the gate line and confirm the new test **fails**. Restore.

**Verify**: `cargo test --lib channels::nextcloud_talk` → all pass.

### Step 2: Extract the per-message body, channel by channel

For each of the twelve listen-based channels, move the per-message handling out of the
socket loop into a pure function taking the platform payload and returning
`Option<ChannelMessage>`. Copy the shape Linq and WhatsApp already have.

Then add the allowed/denied pair per channel.

Do them **one commit at a time**. If a channel's loop cannot be extracted without
restructuring its transport handling, skip it, record which, and move on — twelve
partial wins beat one stalled branch.

**Verify**: `cargo test --lib channels::` → all pass after each.

### Step 3: Prove each gate is load-bearing

For every channel you covered, delete its gate line and confirm **its** test fails.
Restore after each.

Record the per-channel result in the PR. A channel whose test still passes without its
gate has not been covered, whatever the test name says.

**Verify**: each deletion fails its test; the tree is restored.

### Step 4: Cover the WhatsApp session store

Add a round-trip test per store family — identity, session, prekey, signed prekey,
sender key, sync key, mutation MAC — each in the shape put → get → overwrite → delete →
get-returns-`None`. Add a prekey-consumption test, and a schema test asserting that
reopening an existing DB file preserves its rows.

This is volume, not difficulty: the three existing tests show the pattern, and it is
plain SQLite over a temp file. Plan 123 changed several of these methods — assert the
behaviour it merged, not what this plan assumed.

**Verify**: `cargo test --lib channels::whatsapp_storage` → all pass.

### Step 5: Make `apply_pairing`'s field argument load-bearing, and cover all 14 arms

Either make `field` assert that it matches the resolved list and return an error on
mismatch, or delete the parameter. A parameter that is consumed by `let _ = field;`
cannot be tested and gives false assurance that it is checked.

Then add a table-driven test over all fourteen `(channel, field, expected-vec)` tuples,
and a `try_handle_pairing` test with an absent config section asserting the reply is a
**failure**, not "✅ You're now an owner".

**Verify**: `cargo test --lib channels::pairing` → all pass.

### Step 6: Exercise durable history end to end

Build one context with a real `ChannelHistoryStore` over a tempdir and assert:
the append writes through; crossing `MAX_CHANNEL_HISTORY` evicts oldest-first in both
the map and the DB; compaction persists; and a fresh `load_all` reproduces the live
map.

Then make the write-through a no-op and confirm the test **fails**.

Note plan 118 changed the history key shape — assert against what it merged.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 7: Stop the flaky and network-dependent patterns

- Give `TelegramChannel` an injectable API base and point the nine `is_err()`-only
  tests at a local server, asserting the request path, multipart field names and
  values. Delete the ones that only assert "should not panic" or convert them to
  assert the **specific** error.
- Replace the 14 `sleep(20–30ms)` races in `approval_relay.rs` with a bounded poll on
  the registry reaching the expected length.
- UUID-suffix the fixed `"test-supervised-fail"` component name, matching its two
  neighbours.

CI currently forces `--test-threads=1`; this step is what makes restoring parallelism
possible later.

**Verify**: `cargo test --lib channels::` → all pass, with no test making an outbound
network call.

## Test plan

The plan *is* the test work. What must hold:

1. Every channel with an extractable handler has an allowed/denied pair.
2. Step 3's mutation was performed per channel and each failed.
3. The WhatsApp store has a round trip per family.
4. `apply_pairing` covers 14 arms and its `field` argument is load-bearing.
5. Durable history is exercised end to end.
6. No test in `src/channels/` makes an outbound network call.

**Verify**: the scoped commands → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] The scoped test commands pass
- [ ] Step 3's per-channel mutation results are recorded in the PR
- [ ] `grep -rn 'api.telegram.org' src/channels/telegram.rs` returns no hit inside a test
- [ ] `grep -n 'let _ = field' src/channels/pairing.rs` returns nothing
- [ ] Channels that could not be extracted are listed in the PR with the reason
- [ ] `git log --oneline` shows one commit per extraction
- [ ] `plans/README.md` status row for 141 updated

## STOP conditions

Stop and report back if:

- Plans 124–129 have not all merged. This plan touches every platform file.
- An extraction changes behaviour. Back it out; report what diverged.
- A newly-added test fails against merged code for a reason none of 124–129 intended —
  that is a regression in one of them.
- Step 3's deletion does not fail a test you expected it to.
- Making `field` load-bearing (step 5) reveals an existing channel/field mismatch. That
  is a live bug — report it before fixing it, because it means a pairing has been
  writing to the wrong list.

## Maintenance notes

- **What interacts with this**: plans 124–129 changed these files; plan 134 does the
  equivalent for the provisioning smoke harness. Plan 123 rewrote much of
  `whatsapp_storage.rs`.
- **What a reviewer should scrutinise**: that step 2's extractions are moves, and that
  step 3 was performed rather than asserted. The finding this plan closes is
  specifically that a green suite proved nothing.
- **Deliberately deferred**: restoring `--test-threads` parallelism in CI. Step 7
  removes the blockers; flipping the flag is a CI change and belongs with plan 143.
