# Plan 136: TUI — approval and autonomy cannot change by accident

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/tui/app.rs src/tui/commands/pairing.rs src/tui/commands/allowlist.rs src/approval/permissions.rs`
>
> **Line numbers in this plan WILL have drifted** — plan 135 merges before it. That is
> expected and is not a stop condition. Relocate by symbol name and continue. STOP only
> if the *code itself* no longer matches the "Current state" excerpt semantically.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: plans/135 (serialized over `src/tui/app.rs`), plans/122 (provides the scoped resolve API)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Four ways the TUI moves a security control without the operator meaning to.

**Shift+Tab cycles the autonomy level**, and one of the four rungs is "no prompts".
The approval handler requires unmodified keys and `BackTab` carries Shift, so the
binding fires **from inside an open approval prompt** — the operator is looking at a
gate while removing it. Each press also force-rewrites the policy files, clobbering
hand-edited allowlists, per the function's own doc.

**The inline approval widens the allowlist before it knows the approval resolved.**
The grant runs first, then resolution is attempted by basename — which returns
nothing when two requests share a basename. The agent loop runs tool calls in
parallel (the code says so), so two pending `curl` calls are ordinary: pressing `A`
permanently allowlists `curl`, clears the prompt, resolves neither call, prints a
message that says the request "was no longer pending" — which is false — and hangs
the turn.

**`/pair` writes a plaintext owner-granting code into the session database**, which
is full-text indexed and long outlives the code's fifteen-minute window. The default
code is owner-capable and unlimited-use.

**`autonomous_tools` — the flag that voids everything `/permissions` displays — appears
on no TUI surface at all.** An operator can read "Owners (none)" and reasonably
conclude channel senders cannot trigger tools, while every channel message executes
them unprompted.

## Current state

`src/tui/app.rs:1201-1204` — the binding, guarded only against the two setup modals:

```rust
            KeyCode::BackTab if self.setup_overlay.is_none() && self.first_run_wizard.is_none()
                => self.cycle_autonomy_preset(),
```

`:900` — the approval handler requires `key.modifiers.is_empty()`, and `BackTab`
carries `SHIFT`, so it falls through. `:584-585` — the cycle walks
`Manual → Smart → Strict → Off`. `:576-580` — the function's own doc records that it
writes with `force=true`, "the same call `rantaiclaw setup approvals --force` makes,
so any hand-edits … are clobbered".

`:586-614` — on a failed live reload it prints a warning and **still** sets
`self.context.autonomy_preset = Some(next)` and reports the new level.

`:684-695` — the approval ordering:

```rust
            if let Err(e) = security.add_runtime_command(&basename, persist_flag) { … }
        }
        let resolved = pending.resolve_by_basename(&basename, decision).is_some();
```

`src/security/pending.rs:212-224` — `resolve_by_basename` returns `None` when
`matches.len() != 1`. `src/tui/app.rs:1941-1943` — the code's own comment: "The agent
loop can run tool calls in parallel, so several shell commands may block at once."
`:728-730` — the misleading failure message. `:742-749` — advancing to the next queued
request is gated on `resolved`, so nothing is resurfaced.

`src/tui/app.rs:5204-5210` — the pane truncates the command at 197 characters,
justified by two comments (`:5199-5203`, `:1930-1932`) that are both wrong about where
the full command was shown: the scrollback line at `:1933-1936` prints only the
basename. `:5245-5259` — `req.channel` is never rendered.

`src/tui/commands/pairing.rs:42`, `:100`, `:143` — `grant_owner` defaults true,
`max_uses` is `None`, and the plaintext code is returned as `CommandResult::Message`,
which `src/tui/context.rs:603-615` writes into the session store. `:39-82` — the
channel argument is accepted with no validation against any known channel list.

`grep -rn 'autonomous_tools' src/tui/` returns nothing.
`src/approval/permissions.rs:167-217` renders owners, owner commands, guest tools and
guest commands — and not `autonomous_tools`, which
`src/config/schema.rs:2741` documents as making `approval_owners` irrelevant.

`src/tui/commands/allowlist.rs:68`, `:94-96` — `/allow --persist` mutates the **TUI
agent's** `SecurityPolicy`, a different instance from the channel runtime's
(`src/tui/app.rs:2153` builds its own), and reports "persistent allowlist" with no
scope qualifier.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| TUI tests | `cargo test --lib tui::` | all pass |
| Approval tests | `cargo test --lib approval::` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/tui/app.rs`, `src/tui/commands/pairing.rs`,
`src/tui/commands/allowlist.rs`, `src/approval/permissions.rs` (the render only).

**Out of scope**: `src/security/pending.rs` — plan 122 adds the scoped resolve API;
consume it. The channel-side approval relay — plan 122. `PendingApprovals::default()`'s
no-timeout behaviour, which is **correct for the TUI** and documented as such.

## Git workflow

- Branch: `fix/tui-approval-and-autonomy-safety`
- Conventional commits, e.g. `fix(tui): stop Shift+Tab changing autonomy from an approval prompt`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Make the autonomy binding safe

- Gate the `BackTab` arm on `self.pending_approval.is_none()` and on not being
  mid-stream, so it can never fire while a gate is on screen.
- Either make the cycle skip `Off` — reachable only via the explicit `/autonomy off` —
  or require a confirming second keypress when the next rung is `Off`.
- Stop passing `force=true` from the keybinding path so hand-edited policy files
  survive an accidental press.
- On a failed live reload, **do not** update `autonomy_preset` or report the new
  level; restore the previous rung so the status bar cannot assert a level that is not
  in force.

**Verify**: `cargo test --lib tui::` → all pass.

### Step 2: Resolve the approval by id, and grant only after it resolves

Call plan 122's `resolve_by_id(req.id, decision)` instead of the basename form. Move
`add_runtime_command` **after** a successful resolve. On failure, restore
`self.pending_approval` or advance to the next queued request rather than leaving the
box empty, and replace the "no longer pending" text with what actually happened.

**Verify**: `cargo test --lib tui::` → all pass.

### Step 3: Show the operator what they are approving

- Put the full command in the scrollback line at `:1933` — which makes the pane's own
  justifying comment true for the first time — and keep the pane's truncation.
- Render `req.channel` in the pane title, so an approval arriving from a chat is
  distinguishable from a local one.
- Relabel the `always (persist)` chip to say it grants the **basename**, not the shown
  command. Allowlisting one long `curl` invocation permits every future `curl`, and
  the current label does not say so.

**Verify**: `cargo test --lib tui::` → all pass.

### Step 4: Stop persisting the pairing code, and validate the channel

- Render the code to the screen but store a **redacted** form in the session message,
  so the plaintext never reaches `sessions.db`.
- Reject an unknown channel with the list of valid surfaces, using the shared roster
  from plan 120 rather than a new list.
- Default `grant_owner` to **false** (requiring an explicit `--owner`) and give
  `max_uses` a default of one. The gateway path already mints `Some(1)`; this brings
  the CLI into line rather than inventing a policy.

State in the PR that any code previously minted through `/pair` should be treated as
recorded in `sessions.db` and superseded rather than relied on.

**Verify**: `cargo test --lib tui::` → all pass.

### Step 5: Surface the flag that voids the rest

Add `autonomous_tools` to `permissions::render` — prominently, at the top, since when
it is true everything below it is irrelevant. Add an approval-boundary line to the
`/channels` panel showing it plus the owner count.

**Verify**: `cargo test --lib approval::` → all pass.

### Step 6: Qualify the `/allow` scope

`/allow --persist` reports a persistent grant against an instance the running channel
listeners do not share. Qualify the message — "applies to this TUI session; running
channel listeners pick it up on the next restart" — and show a scope column in
`/allowlist`.

Do **not** attempt to share one `SecurityPolicy` between the local terminal and remote
channels here. That merges two trust domains and needs its own design.

**Verify**: `cargo test --lib tui::` → all pass.

## Test plan

1. `backtab_is_inert_while_an_approval_is_pending` — **the plan's primary test**.
2. `autonomy_cycle_does_not_reach_off_without_confirmation`.
3. `failed_autonomy_write_leaves_the_previous_rung`.
4. `two_same_basename_requests_resolve_the_right_one` — and the allowlist is untouched
   when resolution fails.
5. `approval_denial_message_matches_what_happened`.
6. `pair_code_is_not_written_to_the_session_store`.
7. `pair_rejects_an_unknown_channel`.
8. `pair_defaults_to_single_use_and_non_owner`.
9. `permissions_render_shows_autonomous_tools`.

**Mutation check (required).** For test 1, restore the ungated `BackTab` arm and
confirm it **fails**. For test 4, move `add_runtime_command` back before the resolve
and confirm it **fails**. Restore both.

**Verify**: `cargo test --lib tui::` and `cargo test --lib approval::` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] Both scoped test commands pass, including the nine new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'resolve_by_basename' src/tui/app.rs` returns nothing
- [ ] `grep -rn 'autonomous_tools' src/tui/` returns at least one hit
- [ ] The pairing-code supersession note is in the PR body
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 136 updated

## STOP conditions

Stop and report back if:

- Plan 122 has not landed — step 2 calls the scoped API it adds.
- Plan 135 has not landed — this is serialized over `src/tui/app.rs`.
- Changing `/pair`'s defaults breaks a documented onboarding flow that relies on
  multi-use owner codes. Report it; the default is a policy call.
- Gating `BackTab` breaks reverse-tab navigation somewhere it currently works. The
  `preventDefault`-equivalent behaviour is part of the finding, so the fix should
  *restore* reverse-tab, not break it further.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 122 fixes the same resolve-by-basename shape on
  the channel side; both should end up calling one scoped API rather than two. Plan
  135 owns the same file and must land first.
- **What a reviewer should scrutinise**: that step 2's failure path leaves the operator
  with a prompt they can still act on, rather than an empty box and a hung turn; and
  that step 4 redacts at the point the message is **stored**, not merely where it is
  displayed.
- **Deliberately deferred**: sharing one `SecurityPolicy` between the TUI agent and the
  channel runtime. It is the real fix for step 6 and it is a trust-domain decision, not
  a bug fix.
