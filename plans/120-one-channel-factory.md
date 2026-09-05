# Plan 120: One channel factory; `channels doctor` can no longer miss a channel

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs src/cron/scheduler.rs`
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
- **Depends on**: plans/119 (serialized chain over `src/channels/mod.rs`; 119 deletes the third copy)
- **Category**: tech-debt
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Channel construction is written out separately in several places, and they have
already drifted with a user-visible consequence: `channels doctor` never probes
Mattermost. An operator whose Mattermost bot token expired runs the diagnostic, is
told everything is healthy, and has a channel that silently never answers.
`MattermostChannel::health_check` has **no live caller at all**.

`channel_roster` is documented as "the single source of truth… so the two surfaces
can never disagree on which channels exist" — and the surfaces disagree anyway,
because the roster is a fourth hand-maintained list rather than something derived
from the construction that actually happens.

There is also a copy outside the subsystem: the cron scheduler hand-constructs four
channels for delivery and `bail!`s for the other eleven, so an operator can provision
Signal or Matrix, point a cron job at it, and only discover at runtime that delivery
is unsupported.

After this plan, adding a channel means editing one table, and a test fails if any
surface falls out of step.

## Current state

`src/channels/mod.rs:2748-2959` — `doctor_channels` builds probes for Telegram,
Discord, Slack, iMessage, Matrix, Signal, WhatsApp (cloud + web), Linq, Nextcloud
Talk, Email, IRC, Lark, DingTalk, QQ. **There is no Mattermost branch.**

`src/channels/mod.rs:3402-3582` — `start_channels_with_cancellation` builds fifteen,
including Mattermost at `:3431-3440`.

`src/channels/mod.rs:2621-2648` — the roster, with the count in the return type and
display names rather than the lowercase keys everything else uses:

```rust
pub(crate) fn channel_roster(config: &Config) -> [(&'static str, bool); 16] {
    let c = &config.channels_config;
    [
        ("Telegram", c.telegram.is_some()),
        …
        ("Nextcloud Talk", c.nextcloud_talk.is_some()),
```

It also lists `"Webhook"`, which is not a `Channel` implementer, and omits `cli`.

`src/channels/mod.rs:2998` — the doctor summary counts only the probed set, so a
configured-but-broken Mattermost is invisible in the totals too.

`src/cron/scheduler.rs:377-435` — `deliver_if_configured` hand-constructs
`TelegramChannel`, `DiscordChannel`, `SlackChannel`, `MattermostChannel` and then:

```rust
        other => anyhow::bail!("unsupported delivery channel: {other}"),
```

The same constructor argument lists are repeated at `src/channels/mod.rs:2919-2921`,
`:3144-3146` (removed by plan 119) and `:3549-3551`, so every channel signature
change must be made in lockstep across all of them.

Note `src/channels/mod.rs:350-355` — `channel_supports_announce_delivery` carries a
"keep in sync" comment pointing at `deliver_if_configured`, and the audit confirmed
the two lists currently **match**. Do not break that while unifying.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |
| Cron tests | `cargo test --lib cron::` | all pass |
| Doctor test | `cargo test --test doctor_checks` | all pass |

Feature-gated channels need their flags to be covered:
`cargo clippy --features channel-lark --all-targets -- -D warnings`.

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/channels/mod.rs` — the factory, `doctor_channels`, `start_channels_with_cancellation`, `channel_roster`
- `src/cron/scheduler.rs` — `deliver_if_configured` consumes the factory

**Out of scope**:
- Decomposing `mod.rs` — plan 121, next in the chain. Put the factory where 121's map
  says `factory.rs` will go if that is convenient, but do not move other code.
- Per-channel construction *arguments* changing — the factory must build exactly what
  the current call sites build.
- Adding cron delivery support to the eleven channels that lack it. That is a
  feature; this plan only makes the gap visible and non-duplicated.

## Git workflow

- Branch: `refactor/one-channel-factory`
- Conventional commits, e.g. `refactor(channels): build every channel from one table`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Write the factory

Add one function that is the only place a channel is constructed from config:

```rust
pub(crate) fn build_configured_channels(
    cc: &ChannelsConfig,
) -> Vec<(&'static str, Arc<dyn Channel>)>
```

Requirements:
- Keys are the lowercase names already used by `channels_by_name` and by
  `Channel::name()` — `telegram`, `nextcloud_talk`, `imessage`, `dingtalk`, not
  display names.
- Feature gating for `channel-matrix`, `channel-lark` and `whatsapp-web` is preserved
  **exactly** as the current call sites have it. Getting this wrong silently drops a
  channel from a build.
- It builds only sections that are `Some`.

**Verify**: `cargo clippy --all-targets -- -D warnings` and
`cargo clippy --features channel-lark --all-targets -- -D warnings` → exit 0.

### Step 2: Route the two in-subsystem callers through it

Rewrite `doctor_channels` and `start_channels_with_cancellation` to consume the
factory. They differ in what they do afterwards and in their log wording — the
doctor says "skipping health check" for a feature-gated channel, the starter says
"skipping runtime startup". Preserve both; pass the verb in, or have each caller log
its own message over the factory's output. Do not flatten them into one message.

**Verify**: `cargo test --lib channels::` and `cargo test --test doctor_checks` → all pass.

### Step 3: Derive the roster instead of maintaining it

Replace `channel_roster`'s fixed-size array with a `Vec<(&'static str, bool)>` keyed
on the same lowercase names, derived from the same section list the factory walks.

`rantaiclaw channel list` prints these, so keep a separate key → display-name lookup
rather than changing what the operator sees. Decide what to do about the `"Webhook"`
entry, which is not a channel, and the missing `cli` — state your choice in the PR
rather than silently keeping or dropping either.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 4: Make cron delivery use the factory

Replace the four hand-rolled constructions in `deliver_if_configured` with a lookup
by key against the factory. The `bail!` for unsupported channels stays — but it now
covers whatever the factory does not produce, rather than a separately-maintained
list of four.

Preserve the `channel_supports_announce_delivery` ↔ cron-delivery agreement. If the
factory makes more channels deliverable than that function advertises, the two are
now out of sync in the *other* direction; add the agreement test in the test plan
before changing behaviour, and if they diverge, stop and report rather than silently
widening what cron can deliver to.

**Verify**: `cargo test --lib cron::` → all pass.

## Test plan

1. `every_configured_channel_is_probed_by_doctor` — build a config with **every**
   section populated; assert the doctor's probe set covers the factory's key set.
   **This is the test that would have caught Mattermost**, and it is the primary
   deliverable of this plan.
2. `roster_matches_the_factory` — assert `channel_roster`'s key set equals the
   factory's for a fully-populated config.
3. `factory_respects_feature_gates` — assert the gated channels are present or absent
   according to the compiled features. Run it under both default and
   `--features channel-lark`.
4. `announce_list_agrees_with_cron_delivery` — assert
   `channel_supports_announce_delivery` and what `deliver_if_configured` accepts name
   the same set. This pins the invariant the "keep in sync" comment asks for and
   nothing currently enforces.
5. `cron_delivery_reports_unsupported_channels_by_name`.

**Mutation check (required).** For test 1, delete one channel's branch from the
factory and confirm the test **fails**. Restore. This is exactly the drift that
produced the finding, so the test must detect it.

**Verify**: `cargo test --lib channels::`, `cargo test --lib cron::`,
`cargo test --test doctor_checks` → all pass, including the five new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0, and again with `--features channel-lark`
- [ ] The three scoped test commands pass, including the five new tests
- [ ] The mutation check was performed and test 1 failed as expected
- [ ] `grep -c 'MattermostChannel::new' src/` returns 1 — the factory
- [ ] `channel_roster` no longer has a count in its return type
- [ ] The `"Webhook"` / `cli` roster decision is stated in the PR body
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 120 updated

## STOP conditions

Stop and report back if:

- Plan 119 has not landed — it deletes the third construction copy, and unifying two
  is materially simpler than unifying three.
- The doctor and the starter turn out to construct a channel with **different
  arguments** (not just different logging). That is a behavioural difference hiding in
  what looked like duplication, and it must be understood before it is unified.
- `channel_supports_announce_delivery` and the factory disagree after step 4 — report
  the difference; do not widen cron delivery to fix a test.
- A feature-gated channel disappears from a build after step 1. Stop immediately; a
  silently dropped channel is worse than the duplication.
- The mutation check still passes after you delete a factory branch.

## Maintenance notes

- **What interacts with this**: plan 121 moves the factory into its own module; plan
  135 fixes the TUI, which keeps **two more** private roster copies (and omits QQ
  entirely). Once this factory exists, 135 should consume it rather than fixing its
  copies in place — note that in the PR so 135's executor sees it.
- **What a reviewer should scrutinise**: the feature-gate preservation in step 1, and
  that test 1 uses a fully-populated config. A test that only populates the channels
  the doctor already probes proves nothing.
- **Deliberately deferred**: making the other eleven channels cron-deliverable. The
  gap is now visible and single-sourced, which is the prerequisite; whether to close
  it is a product call.
