# Plan 122: Bind every approval to the request and the channel it answers

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/approval_relay.rs src/security/pending.rs src/approval/mod.rs src/channels/cli.rs src/channels/mod.rs`
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
- **Depends on**: plans/116 (it owns the dispatch-site edit in `channels/mod.rs`)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Chat approval is a real privilege boundary: an owner can authorise a shell command
from a phone. The audit found **six independent defects** in it, and they compose.

The core one needs no identity spoofing at all. A guest in chat A triggers a gated
tool. The prompt is posted into chat A — the *triggering* chat, not the owner's.
The owner, chatting normally in chat B on a different channel, types `ok`. That
resolves chat A's request, because resolution matches on the command basename and
consults neither the request id nor the originating channel, both of which the
request already carries. On the shell path the grant is durable, not per-call: the
attacker-chosen basename is written into the process-wide allowlist.

Around it: the prompt promises a five-minute auto-deny that the registry behind it
does not implement; the half of the relay that would have *told* an owner a request
is pending was never wired up; `/allow` widens the allowlist before checking that
anything was pending; and `approval_owners = ["*"]` makes every sender on every
channel an owner, with no warning anywhere and no mention in any doc.

After this plan, an approval answers the request it names, in the chat it came
from, within a deadline the prompt states truthfully.

## Current state

### 1. Resolution ignores the fields that would make it safe

`src/channels/mod.rs:2169-2182` — the dispatch site passes neither channel nor
reply target:

```rust
        let approval_reply = approval_relay::try_handle_tool_reply(
            &msg.content,
            ctx.tool_approvals.as_ref(),
            approver,
            &live_owners,
        )
        .or_else(|| {
            approval_relay::try_handle_reply(
                &msg.content,
                ctx.security.as_ref(),
                approver,
                &live_owners,
            )
        });
```

`src/security/pending.rs:212-224` — and resolution filters on basename alone:

```rust
    pub fn resolve_by_basename(&self, basename: &str, decision: Decision) -> Option<Uuid> {
        let id = {
            let snap = self.inner.snapshot.lock();
            let matches: Vec<Uuid> = snap
                .values()
                .filter(|r| r.basename == basename)
                .map(|r| r.id)
                .collect();
            if matches.len() != 1 {
                return None;
            }
            matches[0]
        };
```

`PendingRequest` carries both an `id: Uuid` and an originating `channel: String`
(`src/security/pending.rs:44-55`). Neither is read on the resolve path.

### 2. The prompt promises a deadline the registry does not have

`src/channels/approval_relay.rs:41-50`:

```rust
pub fn format_approval_message(basename: &str, full_command: &str) -> String {
    format!(
        "🔒 Approval needed: `{basename}` (full command: `{full_command}`).\n\
         …
         Auto-deny in 5 min."
    )
}
```

`src/channels/mod.rs:3264` — the shell registry, built with no timeout:

```rust
    let pending = Arc::new(crate::security::PendingApprovals::default());
```

`src/channels/mod.rs:3742` — the tool registry, built correctly:

```rust
        tool_approvals: Arc::new(crate::security::PendingApprovals::new(Some(
```

`src/security/pending.rs:233-241` documents that `default()` means *no timeout*, and
that this is deliberate **for the TUI**. Only the channels construction site is wrong.

### 3. Nobody is ever told a shell approval is pending

`grep -rn 'spawn_relay' src/ tests/` returns exactly two hits: the definition at
`src/channels/approval_relay.rs:222` and a reference in the module's own doc
comment at `:6`. Zero call sites. The reply half is wired, so an owner who somehow
knows a request exists can answer it — they are simply never told.

### 4. `/allow` widens the allowlist before it knows anything was pending

`src/channels/approval_relay.rs:174-201` calls
`security.add_runtime_command(basename, persist)` and only afterwards attempts
`resolve_by_basename`, returning a success acknowledgement either way. The read
path matches on the **basename of any path** (`src/security/policy.rs:786-802`), so
a grant authorises that name anywhere on `PATH`.

### 5. A wildcard nobody documented

`src/approval/mod.rs:254-268`:

```rust
pub fn can_approve_any<'a>(
    owners: &[String],
    identities: impl IntoIterator<Item = &'a str>,
) -> bool {
    fn normalize(s: &str) -> &str {
        s.trim().trim_start_matches('@')
    }
    if owners.iter().any(|o| o == "*") {
        return true;
    }
```

The gateway warns when `allowed_users` contains `*` (`src/gateway/config_api.rs:634-638`).
Nothing warns for `approval_owners`, and no doc says the value is accepted.

### 6. The CLI's identity is a bare string in a shared namespace

`src/channels/cli.rs:53` sets `sender: "user"`, and `can_approve_any` above compares
raw strings with no channel qualification. `src/approval/permissions.rs:176` prints
`"(none — only the CLI/console operator is an owner)"`, which invites an operator to
add `"user"` to `approval_owners` — after which a Telegram account renamed to `user`,
or an IRC nick `user`, is an owner.

### 7. Two parsing gaps

`src/channels/approval_relay.rs:71-80` — the non-owner refusal returns **before**
checking whether anything is pending, so ordinary chat containing "allow him" is
consumed and answered with a refusal that is also a membership oracle (it is emitted
only to non-owners).

`src/channels/approval_relay.rs:148-156` — verbs are matched case-sensitively, while
`parse_bare_verb` at `:394` lowercases. `Allow brew` — what a phone keyboard produces —
matches neither and is forwarded to the model as chat.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Relay tests | `cargo test --lib channels::approval_relay` | all pass |
| Security tests | `cargo test --lib security::pending` | all pass |
| Approval tests | `cargo test --lib approval::` | all pass |

**Do not run a bare `cargo test`** — it writes ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/security/pending.rs` — add id-and-scope-aware resolution
- `src/channels/approval_relay.rs` — reply parsing, prompt text, `/allow` ordering, `spawn_relay` decision
- `src/approval/mod.rs` — the `*` wildcard and channel-qualified matching
- `src/channels/cli.rs` — the CLI identity
- `src/channels/mod.rs` — **only** the two construction sites (`:3264`) and the
  dispatch call at `:2169-2182`. Plan 116 owns everything else in this file and
  must land first.

**Out of scope**:
- `PendingApprovals::default()`'s no-timeout behaviour — it is correct for the TUI
  and documented as such. Change the channels *construction site*, not the default.
- The TUI's own approval handling (resolve-by-basename there, the `A` key, the
  truncated pane) — plan 136.
- `src/security/policy.rs`'s basename matching semantics. Narrowing what a grant
  authorises is a separate, larger change; this plan only stops a grant being
  written when nothing was pending.
- The owner-identity questions on individual channels (email `From:`, IRC nick,
  Telegram username) — plans 125, 126, 127.

## Git workflow

- Branch: `fix/approval-bound-to-request`
- Conventional commits, e.g. `fix(security): resolve approvals by request id and origin`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Make a pending request carry where it can be answered

Extend `PendingRequest` (`src/security/pending.rs:44-55`) with the reply target
alongside the `channel` it already has. Populate both wherever a request is
registered.

**Verify**: `cargo test --lib security::pending` → all pass.

### Step 2: Add scoped resolution and keep the old path for the TUI

Add to `PendingApprovals`:

```rust
    /// Resolve a specific request by id. Returns false if it is no longer pending.
    pub fn resolve_by_id(&self, id: Uuid, decision: Decision) -> bool
```

and a scoped basename variant that only considers requests whose
`(channel, reply_target)` match the reply's:

```rust
    pub fn resolve_by_basename_in(
        &self,
        basename: &str,
        channel: &str,
        reply_target: &str,
        decision: Decision,
    ) -> Option<Uuid>
```

Keep the existing `resolve_by_basename` — the TUI uses it and plan 136 owns that
call site.

**Verify**: `cargo test --lib security::pending` → all pass.

### Step 3: Put a short request handle in the prompt, and thread the scope through

- `format_approval_message` and `format_tool_approval_message` gain the first 6
  characters of the request uuid and the requesting sender, so the owner can see
  *what* they are approving and can name it explicitly.
- The reply grammar accepts that handle (`/allow a1b2c3`) in addition to the
  basename.
- `try_handle_reply` / `try_handle_tool_reply` take the replying message's channel
  and reply target and pass them to the scoped resolver.
- The bare-verb path (`ok`, `y`) stays valid **only** when the reply arrives in the
  same channel and reply target as the request. That preserves the deliberate
  forgiving-reply UX for the common single-chat case while closing the cross-chat
  hole.

Update the dispatch call at `src/channels/mod.rs:2169-2182` to pass `msg.channel`
and `msg.reply_target`.

**Verify**: `cargo test --lib channels::approval_relay` → all pass.

### Step 4: Give the channels shell registry the deadline its prompt promises

At `src/channels/mod.rs:3264`, construct with an explicit 300-second deadline,
matching `:3742`. Derive the "Auto-deny in N min" text in
`format_approval_message` from that duration rather than hardcoding it, so the two
cannot drift again.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 5: Only grant after a successful resolve

In the `Allow` branch (`src/channels/approval_relay.rs:174-201`), move
`add_runtime_command` **after** the resolve, and only call it when the resolve
succeeded. When nothing matched, say so plainly instead of reporting success.

Keep a `--force` escape hatch for the deliberate pre-authorisation workflow if you
judge one is needed, but it must be explicit, never the default.

**Verify**: `cargo test --lib channels::approval_relay` → all pass.

### Step 6: Decide `spawn_relay` — wire it or delete it

It has no callers. Both outcomes are acceptable; leaving it is not, because it
reads as working machinery.

- **If deleting**: remove `spawn_relay` and update the module doc at
  `src/channels/approval_relay.rs:4-10`, which currently describes it as one of the
  module's "two halves". Note in the PR that shell approvals over chat are then
  reply-only and the operator must be told a request is pending by some other
  means.
- **If wiring**: it broadcasts prompts to *every* configured channel's default
  recipient, which is a disclosure decision. Gate it on `approval_owners` being
  non-empty, send only to owner-reachable targets, and say so in the PR.

Prefer deleting unless the operator says otherwise — wiring it is a product
decision, not a bug fix.

**Verify**: `grep -rn 'spawn_relay' src/ tests/` matches your chosen outcome.

### Step 7: Close the two parsing gaps

- Move the pending-registry lookup **above** the authorization branch, so an
  unauthorized-but-real approval attempt still gets its refusal while ordinary chat
  falls through to `None`. This also removes the membership oracle.
- Lowercase the multi-character verbs (`allow`, `deny`, `approve`, `reject`) before
  matching. **Leave `y`/`Y` and `n`/`N` case-sensitive** — `Y` means persist there,
  so case is load-bearing.

**Verify**: `cargo test --lib channels::approval_relay` → all pass.

### Step 8: Handle the `approval_owners` wildcard and the CLI identity

- In `can_approve_any`, decide the `*` case deliberately. Recommended: keep it
  working (removing it silently would lock out anyone relying on it) but emit a
  startup `tracing::warn!` naming it as an all-senders grant, and add it to the
  config-API warning set alongside the existing `allowed_users` `*` warning.
  Document it in the config reference — plan 144 owns the doc, so record the text
  in your PR notes for that plan to pick up.
- Give the CLI a channel-qualified identity so it cannot collide with a remote
  sender's chosen name. The CLI already has its own interactive approval backend
  (`src/approval/mod.rs:313-319`), so it does not need to be an owner *by string* at
  all — prefer making that structural. If you keep a string, namespace it
  (`cli:local`) and accept the old `"user"` as a deprecated alias for one release,
  with a warning.
- Correct the hint at `src/approval/permissions.rs:176` so it no longer implies
  adding `"user"` is the way to grant the console owner rights.

**Verify**: `cargo test --lib approval::` → all pass.

## Test plan

New tests. Model them on the existing relay tests in
`src/channels/approval_relay.rs` (they already build registries and drive replies).

1. `bare_ok_from_another_channel_does_not_resolve` — request registered on
   `telegram`/chat A; owner replies `ok` on `discord`/chat B; assert the request is
   **still pending** and the allowlist was not modified.
2. `bare_ok_in_a_different_chat_on_the_same_channel_does_not_resolve` — same
   channel, different reply target.
3. `reply_naming_the_request_handle_resolves_it` — two pending requests with the
   same basename; a reply naming one handle resolves exactly that one.
4. `allow_with_nothing_pending_does_not_touch_the_allowlist` — assert the runtime
   allowlist is unchanged and the reply reports no match.
5. `unanswered_shell_request_denies_at_the_deadline` — mirror the existing tool-side
   timeout test at `src/channels/approval_relay.rs:1022`.
6. `capitalised_allow_is_recognised` — `Allow brew` resolves; `Y` still means persist
   and `y` still means once.
7. `non_owner_chat_that_looks_like_an_approval_is_not_consumed` — with nothing
   pending, a guest message "allow him" returns `None` (falls through to the agent)
   rather than a refusal.
8. `wildcard_owner_is_warned_about` — `can_approve_any(&["*".into()], …)` is `true`
   **and** the warning path fires.
9. `cli_identity_does_not_match_a_remote_sender` — a Telegram message whose sender
   is `user` must not satisfy `can_approve_any` against the CLI's owner entry.

**Mutation check (required).** For test 1, remove the scope comparison you added in
step 3 and confirm the test **fails**. For test 4, move `add_runtime_command` back
before the resolve and confirm it **fails**. Restore both afterwards. Findings
T2-10 and T2-11 in the report exist because tests in this exact module asserted
things that were true without the code under test.

**Verify**: `cargo test --lib channels::approval_relay`,
`cargo test --lib security::pending`, `cargo test --lib approval::` → all pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] The three scoped test commands pass, including all nine new tests
- [ ] Both mutation checks were performed and failed as expected
- [ ] `grep -n 'PendingApprovals::default()' src/channels/mod.rs` returns only
      test-module hits, not the runtime construction at `:3264`
- [ ] `grep -rn 'spawn_relay' src/ tests/` matches the step-6 decision
- [ ] The "Auto-deny in N min" text is derived from the configured duration, not a literal
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 122 updated

## STOP conditions

Stop and report back if:

- Plan 116 has not landed. It owns `channels/mod.rs`; starting here first will
  conflict.
- `PendingRequest` turns out to be constructed somewhere that cannot supply a reply
  target — that would mean a request can exist with no answerable scope, and the
  design needs revisiting before you add the field.
- Scoping the bare-verb path breaks an existing test that asserts cross-chat
  resolution **deliberately** (read the test's name and comment before assuming it
  is wrong). If the repo intended cross-chat approval, this plan's premise is wrong
  and the operator must decide.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 136 fixes the TUI's own resolve-by-basename
  path, which has the same shape plus an ordering bug of its own. The two should
  end up calling the same scoped API; if 136 lands first, reuse whatever it added
  rather than adding a parallel one.
- **What a reviewer should scrutinise**: that the bare-verb convenience path is
  still usable in the ordinary single-chat case (over-tightening it makes chat
  approval unpleasant enough that operators reach for `autonomous_tools`, which is
  strictly worse), and that step 5 did not turn a legitimate pre-authorisation
  workflow into an error without an escape hatch.
- **Deliberately deferred**: narrowing what a basename grant authorises
  (`src/security/policy.rs:786-802` matches the basename of any path, so allowlisting
  an interpreter-class name effectively lifts the allowlist). That is a real
  finding, it is recorded in the report under T1-15, and it needs its own plan with
  a compatibility story — do not attempt it here.
