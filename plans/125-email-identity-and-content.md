# Plan 125: Email — authenticate the sender, stop leaking the password, fix HTML and timestamps

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/email_channel.rs`
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
- **Depends on**: plans/115 (adds the `apply_allowed_senders` trait method)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Email is the sharpest identity surface in the fleet. Every webhook channel has a
cryptographic binding between the transport and the claimed sender; email has none.
The `From:` header is taken verbatim as the sender identity, with no SPF, DKIM,
DMARC or `Authentication-Results` check anywhere in the file — so anyone who can
deliver mail into the configured mailbox controls who the agent thinks is talking.

If an owner's address is in `approval_owners`, a message claiming that address
satisfies both the chat gate and the owner gate. And because `reply_target` is the
same forged address, the agent's answer goes to the **real** owner: the attacker
acts without receiving output, which is exactly the shape that makes the cross-chat
approval timing attack in plan 122 practical.

Around it: the config struct derives `Debug` over a plaintext mailbox password, so
one `tracing::debug!(?config)` writes the credential to the log stream; SMTP sends
that password in the clear when `smtp_tls = false`, with no warning; unparseable
mail is either lost silently or refetched forever; `<script>` and `<style>` bodies
are injected whole into the agent's prompt; and every inbound timestamp is wrong by
up to ±14 hours.

## Current state

`src/channels/email_channel.rs:167-175` — the identity, taken verbatim:

```rust
    fn extract_sender(parsed: &mail_parser::Message) -> String {
        parsed
            .from()
            .and_then(|addr| addr.first())
            .and_then(|a| a.address())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".into())
    }
```

`grep -c 'Authentication-Results\|DKIM\|Received-SPF\|dmarc' src/channels/email_channel.rs`
returns **0**.

That value becomes both `sender` and `reply_target` (`:256`, `:447-452`), is matched
against `allowed_senders` by `is_sender_allowed` (`:123-143`, which additionally
accepts bare-domain and `@domain` entries — a widening the channel reference does
not document), and is what `can_approve` compares against `approval_owners`.

`:39` and `:60` — the credential in a `Debug` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmailConfig {
    …
    pub password: String,
```

The existing test `email_config_debug_output` (`:970-978`) formats the whole struct
and asserts nothing about the password.

`:466-478` — `create_smtp_transport` attaches `Credentials::new(username, password)`
and, when `smtp_tls` is false, builds via `SmtpTransport::builder_dangerous(..)` —
no TLS, no STARTTLS upgrade, no warning. The IMAP side (`:207-216`) is correct.

`:302-308` — the `\Seen` flag is applied to the **entire** fetched UID set built at
`:242-246`, including UIDs whose parse returned `None` at `:255`. `:438-441` —
`seen_messages` is a `HashSet` that is only ever inserted into.

`:144-164` — `strip_html` removes tags but keeps their content:

```rust
    pub fn strip_html(html: &str) -> String {
        let mut result = String::new();
        let mut in_tag = false;
        for ch in html.chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => result.push(ch),
                _ => {}
            }
        }
```

Reached from `extract_text` (`:181-183`) whenever there is no text part.

`:265-283` — the timestamp is rebuilt field-by-field into a `NaiveDate` and
`.and_utc()`, discarding the parsed offset.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::email_channel` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/email_channel.rs`

**Out of scope**:
- `src/config/schema.rs` — the other five channel configs (`SlackConfig`,
  `MattermostConfig`, `DingTalkConfig`, `QQConfig`, `LarkConfig`) have the same
  `Debug`-over-a-credential shape. Fix **only** `EmailConfig` here; the others belong
  to their own platform plans, and a shared `Redacted<String>` newtype — if you
  introduce one — must live somewhere both can reach without this plan editing their
  files.
- The `approval_owners` comparison itself — plan 122.
- Message splitting at any email size limit — plan 129.

## Git workflow

- Branch: `fix/email-identity-and-content`
- Conventional commits, e.g. `fix(email): require an authenticated sender before honouring From:`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Stop the password reaching a log

Replace the `Debug` derive on `EmailConfig` with a hand-written impl that renders
the password as `"[redacted]"`. If you introduce a shared `Redacted<String>` newtype
instead, place it somewhere the other five configs can adopt later **without**
editing them now.

Flip the existing `email_config_debug_output` test into a negative assertion: the
formatted output must **not** contain the password. That converts a test that
asserts nothing into a real guard.

Recommend in the PR that any mailbox password which may already sit in retained logs
be rotated.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

### Step 2: Require an authenticated sender before honouring `From:`

Parse `Authentication-Results` (and/or `Received-SPF` / `DKIM-Signature`) from the
fetched message. Require `dmarc=pass`, or an aligned `spf=pass` / `dkim=pass` for the
`From:` domain, before treating the header as an identity.

Gate it behind a config field — `require_authenticated_sender` — that defaults to
**on for any address listed in `approval_owners`** and off for plain chat. That
tightens the exposure boundary without breaking casual use from a relay that strips
the header.

Independently, and regardless of that setting: **an email must not satisfy the owner
gate unless the message authenticated.** The simplest safe form is to exclude
`channel == "email"` from the owner path until authentication is available; do that
if the header work turns out larger than expected, and say so.

Drop the message with a warning when authentication is required and absent — do not
fall back to `"unknown"`, which is itself a shared identity that multiple senders
collapse into.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

### Step 3: Do not send credentials over a plaintext transport

Branch `create_smtp_transport` three ways: implicit TLS (`relay`), STARTTLS
(`starttls_relay`), and plaintext **without credentials**. Bail with a clear error
when credentials are configured alongside a genuinely plaintext transport.

A credential-less local relay on port 25 is a legitimate setup, so keep
`builder_dangerous` reachable — but only when `username` and `password` are empty.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

### Step 4: Stop losing mail, and stop refetching it forever

Build the `\Seen` UID list from **successfully parsed** messages only. Handle the
unparseable ones explicitly — move them out of UNSEEN with a distinct marker, or
flag them separately — so they neither vanish nor loop.

Bound `seen_messages` using the `VecDeque` + `HashSet` pattern already used at
`src/channels/matrix.rs:231`.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

### Step 5: Keep script and style bodies out of the prompt

Skip content between `<script …>`/`</script>` and `<style …>`/`</style>` while
stripping, and decode the common HTML entities on the way out.

Prefer delegating to a maintained HTML-to-text crate over growing this
hand-rolled one — but weigh it against this project's dependency-weight goal
(CLAUDE.md §2.3) and say which you chose and why in the PR.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

### Step 6: Keep the timezone

Use `mail_parser`'s own `to_timestamp()`, which accounts for the offset, and delete
the manual `NaiveDate` reconstruction.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

### Step 7: Implement the runtime allowlist trait method

Implement `apply_allowed_senders` (added by plan 115) so a console or CLI allowlist
edit reaches this channel without a restart. Preserve the existing domain-matching
semantics of `is_sender_allowed` — and record in the PR that those semantics are
undocumented, so plan 144 can add them to the channel reference.

**Verify**: `cargo test --lib channels::email_channel` → all pass.

## Test plan

1. `unauthenticated_from_is_not_an_owner` — a message whose `From:` matches an
   `approval_owners` entry, with no passing authentication, must not satisfy the
   owner gate. **This is the plan's primary test.**
2. `mismatched_authentication_results_is_dropped`.
3. `debug_output_does_not_contain_the_password` — the flipped existing test.
4. `plaintext_smtp_with_credentials_is_refused` — and its twin, that a
   credential-less plaintext relay still builds.
5. `unparseable_message_is_not_marked_seen` — seed two messages, one parseable and
   one not; assert only the parseable UID is flagged.
6. `all_unparseable_batch_does_not_loop` — assert the second poll does not refetch
   the same UIDs indefinitely.
7. `script_and_style_bodies_are_stripped` — and that entities are decoded.
8. `timestamp_honours_the_offset` — a `Date:` header with a non-UTC offset yields the
   correct epoch second.
9. `allowlist_edit_reaches_the_channel` — mirror plan 115's channel test.

**Mutation check (required).** For test 1, remove the authentication requirement and
confirm it **fails**. For test 5, restore the whole-UID-set flagging and confirm it
**fails**. Restore both.

**Verify**: `cargo test --lib channels::email_channel` → all pass, including all nine.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::email_channel` passes, including the nine new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n '#\[derive(Debug' src/channels/email_channel.rs` shows no derive over
      the credential-bearing struct
- [ ] `grep -n 'builder_dangerous' src/channels/email_channel.rs` shows it reachable
      only on the credential-less branch
- [ ] The PR body states the password-rotation recommendation and the
      dependency-vs-hand-rolled decision from step 5
- [ ] No files outside `src/channels/email_channel.rs` are modified (`git status`)
- [ ] `plans/README.md` status row for 125 updated

## STOP conditions

Stop and report back if:

- Plan 115 has not landed — step 7 implements its trait method.
- `mail_parser` does not expose the authentication headers in a usable form. Do not
  hand-roll a header parser for a security decision; report it and fall back to the
  step-2 alternative (exclude email from the owner path entirely).
- Requiring authentication would reject mail from a relay the operator legitimately
  uses and you cannot tell that apart from a forgery. Surface the tradeoff rather
  than choosing a default that silently admits forgeries.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 122 fixes the owner gate itself; this plan makes
  the identity it compares meaningful. Both are needed — a correctly-scoped approval
  over a forgeable identity is still forgeable.
- **What a reviewer should scrutinise**: that step 2's default genuinely fails closed
  for owners and open for plain chat, and that step 4 cannot mark an unparseable
  message seen under any ordering of the batch.
- **Deliberately deferred**: the other five channel configs with the same
  `Debug`-over-a-credential shape. Each belongs to its own platform plan so no two
  plans edit `src/config/schema.rs`; if you introduce a shared newtype, note its
  location in the PR so those plans can adopt it.
