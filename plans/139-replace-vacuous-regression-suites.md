# Plan 139: Replace the vacuous regression suites with tests that can fail

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- tests/channel_routing.rs tests/tui_setup_overlay.rs tests/tui_integration.rs`
>
> **Line numbers WILL have drifted** if earlier plans merged first. Relocate by symbol
> name and continue. STOP only if the *code itself* no longer matches the "Current
> state" excerpt semantically.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: tests
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

`tests/channel_routing.rs` is the repo's headline regression suite for its
most-reported bug class. Its header claims it prevents "Pattern 3 — Channel message
routing & identity bugs (17% of user bugs)" and cites five issue numbers.

It imports no concrete channel. Its headline test builds a `ChannelMessage` struct
literal and asserts the literal back — it tests Rust struct initialisation. Swap
`sender` and `reply_target` in every real inbound path and all fourteen tests still
pass. Delete `src/channels/telegram.rs` entirely and they still compile and pass.

That is worse than having no suite, for a specific reason: its existence suppresses
the instinct to write the real test. Two of the five issues it names are exactly the
field-swap bugs it cannot detect.

`tests/tui_setup_overlay.rs` has the same shape in miniature — two of its tests build
a `CommandResult` and assert its fields — and `tests/tui_integration.rs`'s headline
test asserts `!config.model.is_empty()` on a default, which is structurally
unfalsifiable. The repo already recognises that pattern: `src/tui/app.rs:7396-7398`
calls an equivalent check "structurally unfalsifiable" and removed it.

## Current state

`tests/channel_routing.rs:11` — the only import:

```rust
use rantaiclaw::channels::traits::{Channel, ChannelMessage, SendMessage};
```

`:62-87` — the headline test:

```rust
fn channel_message_fields_not_swapped() {
    // Guards against #496 (Telegram) and #483 (Discord) field swap bugs
    let msg = ChannelMessage {
        …
        sender: "sender_value".into(),
        reply_target: "target_value".into(),
        …
    };

    assert_eq!(
        msg.sender, "sender_value",
        "sender field should not be swapped"
    );
```

`:18-38` asserts a hardcoded `"123456789"` is all-ASCII-digits — an assertion that
cannot fail. `:90-110` asserts `#[derive(Clone)]` works. `:189-201` and `:218-234` —
`CapturingChannel::listen` hardcodes `sender: "test_sender"` and the test asserts the
same constant.

Only three of the fourteen touch production code:
`channel_health_check_default_returns_true` (`:265`),
`channel_typing_defaults_succeed` (`:274`), `channel_draft_defaults` (`:281`) — and
those exercise the `traits.rs` default methods, which is legitimate.

The real parsers are pure and callable without a network:
`telegram::parse_update_message`, `linq::parse_webhook_payload`,
`nextcloud_talk::parse_webhook_payload`.

`tests/tui_setup_overlay.rs:7-29` — `command_result_open_setup_overlay_none_passes` and
`_some_passes` build a `CommandResult::OpenSetupOverlay { … }`, match it, and assert
the field equals what was just written.

`tests/tui_integration.rs:7-12` — `tui_config_has_sensible_defaults` asserts
`!config.model.is_empty()` and `resume_session.is_none()` on `TuiConfig::default()`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Routing suite | `cargo test --test channel_routing` | all pass |
| TUI suites | `cargo test --test tui_setup_overlay --test tui_integration` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `tests/channel_routing.rs`, `tests/tui_setup_overlay.rs`,
`tests/tui_integration.rs`.

**Out of scope**: production code. If a rewritten test **fails** against current
production behaviour, that is a finding — report it; do not fix the code here and do
not weaken the test to make it pass. Webhook handler auth tests (plan 140) and the
allowlist-gate applied tests (plan 141), which need handler extraction this plan does
not do.

## Git workflow

- Branch: `test/replace-vacuous-regression-suites`
- Conventional commits, e.g. `test(channels): assert real payload parsing instead of struct literals`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Capture a real payload fixture per channel

For each channel with a pure parse function, add a captured platform JSON payload as a
fixture: Telegram, Discord, Slack, Mattermost, WhatsApp, Linq, Nextcloud Talk, Lark,
DingTalk, QQ, Matrix, Signal.

Use `tests/fixtures/` if it already holds fixtures of this kind; otherwise put them
beside the test.

**Redact before committing.** Real payloads carry user ids, display names, phone
numbers and chat ids. Per CLAUDE.md §9.1 these must be replaced with project-scoped
placeholders (`rantaiclaw_user`, `user_a`) and reserved-range numbers. A fixture is
committed code.

**Verify**: fixtures parse — a throwaway assertion is fine at this stage.

### Step 2: Rewrite `channel_routing.rs` around the real parsers

For each fixture, feed it to that channel's real parse function and assert that
`sender` and `reply_target` land on the **platform-correct** fields — the sender is
the person, the reply target is where a reply goes.

That is the assertion the file has always claimed to make. For Lark it will currently
**fail**, because Lark reports the chat id as the sender — plan 124 fixes that. If 124
has merged, the test passes; if not, mark it `#[ignore]` with a comment naming plan
124 rather than weakening it.

Delete the struct-literal tests, the all-digits assertion, and the `Clone` test. Keep
the three `traits.rs` default-method tests — they exercise real code.

Rewrite the file header to describe what it now actually guards.

**Verify**: `cargo test --test channel_routing` → all pass (or the documented ignore).

### Step 3: Prove the new suite can fail

For each channel, swap `sender` and `reply_target` in that channel's real construction
site and confirm **its** test fails. Restore after each.

This is the whole point of the plan. Record in the PR which channels you verified this
way — all of them, ideally; if a channel's construction cannot be reached from a test,
say so explicitly rather than leaving it implied.

**Verify**: each swap fails its test; the tree is restored afterwards.

### Step 4: Fix the two TUI suites

Delete `command_result_open_setup_overlay_none_passes` and `_some_passes`, and the
unfalsifiable default check in `tui_integration.rs`.

Replace them with something that exercises production code — the setup-overlay state
machine's transitions, or the command routing that produces `CommandResult`. If
nothing in reach is testable without the harness plan 135/136 would need, say so and
leave the file with only its three genuine `SessionStore` tests rather than
substituting new decorative ones.

**An honest gap is better than a test that lies.** Record the gap in the PR.

**Verify**: `cargo test --test tui_setup_overlay --test tui_integration` → all pass.

## Test plan

The plan *is* the test work. What must hold when it is done:

1. Every channel with a pure parse function has a fixture-driven test asserting
   `sender` and `reply_target` land correctly.
2. Step 3's mutation was performed for each such channel and each failed.
3. No test in these three files asserts a value it just wrote.
4. No fixture contains real identity data.

**Verify**: the scoped test commands → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] The scoped test commands pass
- [ ] Step 3's per-channel mutation results are recorded in the PR
- [ ] `grep -n 'sender: "sender_value"' tests/channel_routing.rs` returns nothing
- [ ] `tests/channel_routing.rs` imports at least one concrete channel module
- [ ] No fixture contains a real name, phone number, or account id
- [ ] No production file is modified (`git status`)
- [ ] `plans/README.md` status row for 139 updated

## STOP conditions

Stop and report back if:

- A rewritten test fails against current production code for a reason **other** than a
  finding this effort already recorded. That is a new bug; report it rather than
  absorbing it.
- A channel's parse function is not reachable without a network. Note which, and cover
  what you can — plan 141 does the handler extraction that would make the rest
  testable.
- You cannot obtain a realistic payload shape for a platform. A hand-written payload is
  acceptable **if** it is derived from that platform's documented schema; say which you
  used.
- Step 3's swap does not fail a test you expected it to. That means the rewritten test
  is still not asserting what you think.

## Maintenance notes

- **What interacts with this**: plan 124 fixes Lark's sender identity, which this
  suite will start asserting; plans 140 and 141 cover the other two test gaps
  (webhook handler auth, and the allowlist gate as an applied guard).
- **What a reviewer should scrutinise**: that step 3 was actually performed per channel
  rather than claimed, and that no fixture carries real identity data — this repo has
  had push protection reject fixtures before.
- **Why this is P1 despite being tests-only**: it is the suite that was supposed to
  catch the field-swap class, and several plans in this effort change exactly those
  fields. Landing it early means those plans have a net beneath them.
