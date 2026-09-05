# Plan 146: Resolve the three config keys nothing reads

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. Step 1 asks
> for a **decision per key** before any code changes. If anything in "STOP conditions"
> occurs, stop and report. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/config/schema.rs src/onboard/ docs/reference/channels.md`
>
> **Line numbers WILL have drifted** if earlier plans merged first. Relocate by symbol
> name and continue. STOP only if the *code itself* no longer matches the "Current
> state" excerpt semantically.

## Status

- **Priority**: P3
- **Effort**: S to delete, M to implement
- **Risk**: LOW
- **Depends on**: plans/133 (owns the provisioner prompts these keys are collected by)
- **Category**: tech-debt
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Three config keys are prompted by the wizard, documented in the channel reference,
redacted as secrets by the config API — and read by no code anywhere.

Two of them make an operator hand over a **real credential** for zero function. The
third is worse than useless: `webhook.port = 8080` tells an operator to open a
firewall port that nothing listens on, and then the webhook silently never arrives.

Three no-op keys in one config section means the schema and the runtime have drifted
apart with nothing checking. That is the part worth fixing — plan 143 adds the guard;
this plan clears the backlog it would otherwise fire on immediately.

Two of the three are worth *building* rather than deleting, and that is a product call,
not a cleanup call.

## Current state

**`SlackConfig.app_token`** — `src/config/schema.rs:2865` describes it as "Slack
app-level token for Socket Mode". Prompted at
`src/onboard/provision/channels/slack.rs:86-102` (`secret: true`) and
`src/onboard/wizard.rs:3816`; written at `slack.rs:194` and `wizard.rs:3818`;
documented at `docs/reference/channels.md:209`; redacted at
`src/gateway/config_api.rs:1091`.

The only non-test, non-redaction hits for `app_token` in `src/` are **Nextcloud
Talk's** identically-named field. Slack itself polls `conversations.history`
(`src/channels/slack.rs:181`), so Socket Mode is never established.

**`LarkConfig.encrypt_key`** — `src/config/schema.rs:3106-3108`. Prompted at
`src/onboard/provision/channels/lark.rs:172-189` (`secret: true`), written at `:275`,
documented at `docs/reference/channels.md:353`. Only other hits are
`src/config/mod.rs:60`, the redaction fixture at `src/gateway/config_api.rs:1098`, and
two tests writing `None`.

The CLI wizard path correctly writes `encrypt_key: None` (`src/onboard/wizard.rs:4827`)
and never asks — only the TUI provisioner collects it.

**`WebhookConfig.port`** — `src/config/schema.rs:2898`, "Port to listen on for incoming
webhooks", documented as `port = 8080` at `docs/reference/channels.md:308`. The gateway
binds its own port (`src/gateway/mod.rs:876`) and reads only `webhook.secret` (`:596`).

Fields checked and confirmed to **have** readers, so they are not part of this plan:
`LarkConfig.port` (`src/channels/lark.rs:1107`), `LarkConfig.verification_token`
(`:209`), `MatrixConfig.device_id`, `NextcloudTalkConfig.webhook_secret`,
`WhatsAppConfig.verify_token`, `IrcConfig.{server,nickserv,sasl}_password`,
`SignalConfig.ignore_stories`, `EmailConfig.imap_folder`, `WhatsAppConfig.pair_code`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Config tests | `cargo test --lib config::` | all pass |
| Schema drift | `cargo test --test schema_drift` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/config/schema.rs`, the wizard and provisioner prompts for these
three keys, `docs/reference/channels.md`'s entries for them, and — if Socket Mode or
Lark decryption is chosen — the relevant channel file.

**Out of scope**: any other config key. The redaction list in `config_api.rs` should be
updated only for a key actually removed. Plan 143's guard test itself.

## Git workflow

- Branch: `refactor/resolve-dead-config-keys`
- One commit per key, so a build decision on one does not hold up a deletion on
  another.
- Conventional commits, e.g. `feat(slack): implement Socket Mode using the collected app token`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Decide each key, in one sitting

Write the decision into the PR **before** changing code. The honest framing for each:

- **`SlackConfig.app_token` — recommend building.** Socket Mode converts Slack from
  polling to real-time push, removing reply latency and continuous
  `conversations.history` spend. The transport cost is zero: `tokio-tungstenite 0.28`
  is already wired for four other channels. This is the highest-value of the three.
- **`LarkConfig.encrypt_key` — recommend building, conditionally.** It is required by
  any Feishu tenant with event encryption enabled, i.e. most enterprise CN
  deployments — the target audience for that channel. AES-256-CBC over the key; only an
  AES crate would be new. **If plan 124 already made a configured-but-unread
  `encrypt_key` a startup error, that is an acceptable resting state** — check before
  duplicating work.
- **`WebhookConfig.port` — recommend deleting.** The gateway owns its port. There is no
  version of this key that is not misleading.

Removing a key is a soft break (they are `Option`), but it is still a public config
contract per CLAUDE.md §6.4 — a removed key needs a CHANGELOG entry, and the wizard
prompt must go with it or operators keep being asked for something that no longer
exists.

**Verify**: three decisions, with rationale, in the PR.

### Step 2: Delete what was decided deleted

For each: remove the schema field, the wizard and provisioner prompts, the docs entry,
and the redaction-list entry if it had one. Bump the config schema version if the
repo's drift gate requires it — `cargo test --test schema_drift` will tell you.

Add a CHANGELOG entry naming the removed key and what an operator should do if they
have it set (nothing — it was never read).

**Verify**: `cargo test --test schema_drift` → passes; `grep -rn '<key>' src/ docs/`
returns nothing.

### Step 3: Build what was decided built

**Slack Socket Mode**: follow `src/channels/discord.rs`'s gateway loop as the template.
Make it a config switch rather than a swap — an operator on polling today should not
have their delivery mode change under them on upgrade. Update
`docs/reference/channels.md`'s delivery-mode row, which plan 144 is separately
correcting to say "polling"; coordinate so the two do not fight.

**Lark decryption**: decrypt the event payload before parsing. Note this interacts
directly with plan 124, which adds signature verification to the same handler —
decryption must happen in the right order relative to verification, and 124 owns that
file. **If 124 has not merged, stop and do the Slack half only.**

**Verify**: the built feature has tests; the scoped test commands pass.

### Step 4: Make the class non-recurring

Plan 143 runs first and has already added the check that every `ChannelsConfig` leaf
field has a reader — with these three keys as its known exceptions. Your job is to
**remove those exceptions** as you resolve each key, so the check ends up with an empty
exception list.

That empty list, not the three individual decisions, is the real deliverable.

The check must **fail closed**: a new field nobody registered should fail, not pass
silently.

**Verify**: the check passes on the cleaned tree and fails when you add an unread field.

## Test plan

1. `no_channels_config_field_is_unread` — the guard from step 4.
2. If Socket Mode was built: a connection-lifecycle test following Discord's, plus a
   config test asserting the mode switch defaults to the current behaviour.
3. If Lark decryption was built: an encrypted-payload round trip, and a test that a
   configured `encrypt_key` with an undecryptable payload fails loudly rather than
   silently dropping the event.
4. If a key was deleted: `schema_drift` passes, and a config containing the old key
   still loads (it is `Option`, so it should be ignored, not rejected).

**Mutation check (required).** For test 1, add an unread field and confirm the guard
**fails**. Restore.

**Verify**: the scoped test commands → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib config::` and `cargo test --test schema_drift` pass
- [ ] Three decisions with rationale are in the PR body
- [ ] Every deleted key is gone from schema, prompts, docs and redaction list
- [ ] A CHANGELOG entry exists for each removed key
- [ ] The step-4 guard passes and fails closed
- [ ] The mutation check was performed
- [ ] `git log --oneline` shows one commit per key
- [ ] `plans/README.md` status row for 146 updated with the three decisions

## STOP conditions

Stop and report back if:

- Plan 133 has not merged — it owns the provisioner prompts.
- Building Lark decryption requires editing the handler plan 124 owns and 124 has not
  merged. Do the Slack half and report.
- Removing a key trips the schema-drift gate in a way that needs a version bump you are
  not sure about. The gate fingerprints defaults; ask rather than bumping blind.
- The step-4 guard cannot be made to fail closed. Ship the deletions, report the
  limitation, and do not present a fail-open check as a guarantee.
- Any of the three keys turns out to **have** a reader you did not find. That would
  mean the audit was wrong; report it before deleting anything.

## Maintenance notes

- **What interacts with this**: plan 143 wires the reader-guard into CI; plan 144
  documents these keys' current state and will need updating if you build or delete
  one; plan 124 owns the Lark handler.
- **What a reviewer should scrutinise**: that a *deleted* key's wizard prompt went with
  it. Leaving the prompt means operators keep supplying a credential for a field that
  no longer exists — which is the same defect in a new shape.
- **Why building beats deleting for two of three**: both were collected because someone
  intended the feature. Deleting is cheaper and leaves the operator's original need
  unmet; the decision belongs to whoever owns the roadmap, which is why step 1 is a
  decision step rather than a foregone conclusion.
