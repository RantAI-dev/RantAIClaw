# Plan 133: Provisioning fails closed, and every probe means something

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/onboard/provision/ src/onboard/wizard.rs src/tui/app.rs src/main.rs`
>
> **Line numbers in this plan WILL have drifted** — plan 132 merges before it. That is
> expected and is not a stop condition. Relocate by symbol name and continue. STOP only
> if the *code itself* no longer matches the "Current state" excerpt semantically.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/132 (serialized over `src/onboard/wizard.rs` and the provisioners)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Plan 132 stopped provisioning from *exposing* credentials. This plan stops it from
*lying* about them.

Every one of the eleven probing modules follows the same shape: probe, warn on
failure, and **persist anyway**. So a typo'd, expired or revoked token is written to
`config.toml` with the same "✅ configured" state as a working one, and the failure
resurfaces later as a silently dead channel. The gateway's equivalent path is explicit
about the opposite policy — "fail closed so we never save a credential that doesn't
work" — so the correct behaviour is already decided elsewhere in this codebase.

Worse, an **aborted** provisioner returns `Ok(())`. Both drivers read that as success
and unconditionally install the core skill and save the config, producing exactly the
false "channel is set up" signal the module's own doc says the ordering prevents.

Then nine provisioners are wrong in ways that make setup quietly useless: an empty
allowlist answer is written as allow-anyone under a prompt that says the opposite; a
Signal option writes a sentinel the runtime reads as a group id; the Lark TUI path
hardcodes the wrong region behind a dead `if false`; IRC advertises SASL and never
asks; iMessage checks a path that exists on no macOS system.

## Current state

`src/onboard/provision/channels/telegram.rs:106-126` then `:168` — the shape,
repeated in `discord.rs`, `slack.rs`, `matrix.rs`, `mattermost.rs`, `lark.rs`,
`dingtalk.rs`, `nextcloud_talk.rs`, `qq.rs`, `whatsapp_cloud.rs`, `linq.rs`:

```rust
            Ok(_) => … Severity::Warn, "Bot token may be invalid."
            Err(e) => … "Continuing…"
```

…and the config write follows unconditionally.

`src/gateway/config_api.rs:591-612` — the reference policy: "fail closed so we never
save a credential that doesn't work."

Every channel module emits `ProvisionEvent::Failed` then `return Ok(())` on a missing
required field — `telegram.rs:73-82`, `email.rs:73-82`, `matrix.rs:210-219`,
`lark.rs:98-107`, and eleven more. `src/tui/app.rs:3894-3910` matches on `Ok(())` and
runs `install_core_skills_after_channel` then `config.save()`; `src/main.rs:3047` does
the same headless.

`src/onboard/provision/mod.rs:36-38` states the invariant that breaks: the skill
install "runs *after* a successful `run` so a channel that failed to configure does
not leave the skill behind as a false signal."

`src/onboard/provision/channels/email.rs:246-256` — the prompt says one thing, the
code does another:

```rust
        let allowed_senders: Vec<String> =
            if allowed_raw.trim().is_empty() || allowed_raw.trim() == "*" {
                vec!["*".to_string()]
```

Same mapping in `whatsapp_cloud.rs:209-219` and `signal.rs:131-141`. Five modules
pre-fill `default: Some("*")`: `email.rs:240`, `whatsapp_cloud.rs:204`,
`signal.rs:125`, `irc.rs:227`, `linq.rs:183`. No module warns on a `"*"` result;
`src/gateway/config_api.rs:634-638` warns for both empty and `"*"`.

`signal.rs:158-164` maps "Direct messages only" to `group_id = Some("dm")`, which
`src/channels/signal.rs:245-254` reads as an inclusion filter for a group whose id is
the literal `dm`. `signal.rs:109-117` prints "Checking signal-cli daemon at …" and
performs no check — the module does not import the probe helper.

`lark.rs:119-123` — `let token_url = if false { …feishu… } else { …larksuite… };` and
`:278` writes `use_feishu: false` with no prompt. The CLI wizard asks
(`src/onboard/wizard.rs:4675-4681`).

`irc.rs:13` advertises "TLS, NickServ/SASL passwords"; `:250` writes
`sasl_password: None` with no prompt. `imessage.rs:116` checks
`/Users/Library/Messages/chat.db`; the runtime uses `home_dir().join("Library/…")`.
`nextcloud_talk.rs:121-127` probes with Basic auth and an empty username while the
runtime uses Bearer.

`email.rs:96-100`, `:158-162`, `:270-274`, `irc.rs:100`, `lark.rs:246`,
`wizard.rs:4488`, `:4800` — unparseable numeric input silently replaced by a default;
Lark's yields `None`, which fails at runtime instead of at setup.

`src/onboard/section/channels.rs:45-50` calls `prompt_owners_and_guest_ceiling` and
`print_owner_claim_guidance`; `grep approval_owners src/onboard/provision/` returns
nothing, and `src/tui/app.rs:3894-3912` calls neither.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Provision tests | `cargo test --lib onboard::provision` | all pass |
| Setup tests | `cargo test --test setup_orchestration` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/onboard/provision/**`, `src/onboard/wizard.rs` (numeric parsing
only), and the two driver call sites in `src/tui/app.rs` and `src/main.rs`.

**Out of scope**: the credential-exposure items (plan 132, already merged); the shared
IO-helper extraction and the smoke harness (plan 134); the real-name identity strings
in `wizard.rs` (plan 142); `src/channels/**` — if a provisioner reveals a runtime bug,
record it, do not fix it here.

## Git workflow

- Branch: `fix/provisioning-fail-closed`
- Conventional commits, e.g. `fix(onboard): refuse to persist a credential the probe rejected`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Distinguish "aborted" from "succeeded"

Introduce a typed outcome — `ProvisionOutcome { Configured, Aborted(String) }`, or a
`ProvisionError::Aborted` — and have every early-return path yield the abort variant.

Gate `install_core_skills_after_channel` **and** `config.save()` on `Configured` only,
in both drivers.

A deliberate user skip must not render as a crash: the TUI's overlay-freeze protection
at `src/tui/app.rs:3960-3970` assumes `Err` is exceptional, which is why this needs a
distinct variant rather than a bare `Err`.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 2: Make the probe block the write — but only on an authenticated rejection

Split the current three arms into:

- 2xx → persist
- an explicit auth rejection (401/403, or an auth-shaped error body) → emit `Failed`,
  **do not write**, and offer an explicit "save anyway" confirmation
- a transport error (DNS, timeout, offline) → warn and offer the same confirmation

A hard failure on transport error would break air-gapped and offline installs, so the
distinction is load-bearing. `src/onboard/wizard.rs:4066-4072` already models the
confirm-to-proceed pattern.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 3: Make empty mean empty, and warn on `"*"`

In `email.rs`, `whatsapp_cloud.rs` and `signal.rs`, stop mapping an empty answer to
`vec!["*"]` so the prompt's own label becomes true. Change the five `default: Some("*")`
values to empty.

Emit a `Severity::Warn` whenever the resulting list contains `"*"`, reusing the wording
already written at `src/gateway/config_api.rs:635-637`.

This is a **default widening in reverse** — it tightens an exposure surface — so per
CLAUDE.md §3.6 it belongs in the CHANGELOG. Say so in the PR.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 4: Fix the nine wrong provisioners

- **Signal**: remove the "Direct messages only" option, or map it to `group_id = None`
  with a note — there is no DM-only predicate in the runtime for it to mean anything.
  Either add a real daemon probe or delete the "Checking…" message; do not leave a
  claim with no check.
- **Lark**: add the region prompt before the probe (mirroring `wizard.rs:4675-4681`),
  and derive both the probe URL and `use_feishu` from it. Delete the `if false`.
- **IRC**: add the `secret: true` SASL prompt next to the NickServ one and wire it
  through, or remove SASL from the description.
- **iMessage**: resolve the chat.db path through the same helper the channel uses.
- **Nextcloud Talk**: probe with Bearer and the `OCS-APIRequest` header, matching the
  runtime.
- **Numeric prompts** (seven sites): on a non-empty unparseable value, warn and
  re-prompt rather than substituting a default. Reserve the default for empty input.
  One shared `parse_or_reprompt` helper covers all seven.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 5: Tell the operator how to become an owner

The TUI provisioning path — the one the module doc calls "the one users take" — never
prompts for or seeds `approval_owners` and never prints the claim guidance the CLI
section path does. So a TUI-provisioned channel comes up with every gated tool
auto-denying and no explanation, which reads as broken rather than as the secure
default it is.

Extend the shared post-channel hook (`install_core_skills_after_channel`) into a
`finalize_channel` that also emits the owner-claim guidance as a
`ProvisionEvent::Message` when `approval_owners` is empty.

**Do not** seed an owner automatically. The empty default is correct; only the
explanation is missing.

**Verify**: `cargo test --lib onboard::provision` → all pass.

## Test plan

1. `aborted_provisioner_writes_no_config_and_installs_no_skill` — **the plan's primary
   test**; assert `config.channels_config.<ch>` is `None` and no skill was installed.
2. `auth_rejection_does_not_persist_the_credential`.
3. `transport_error_offers_a_confirmation_rather_than_blocking`.
4. `empty_allowlist_answer_yields_an_empty_list` — per affected module.
5. `wildcard_allowlist_warns`.
6. `signal_dm_option_does_not_write_a_group_id`.
7. `lark_feishu_selection_sets_use_feishu_and_the_feishu_probe_host`.
8. `irc_sasl_prompt_is_reachable`.
9. `imessage_probe_path_matches_the_channel_path`.
10. `unparseable_port_reprompts_instead_of_defaulting`.
11. `owner_guidance_is_emitted_when_approval_owners_is_empty`.

**Mutation check (required).** For test 1, restore the `Failed`-then-`Ok(())` shape and
confirm it **fails**. For test 4, restore the empty→`"*"` mapping and confirm it
**fails**. Restore both.

**Verify**: `cargo test --lib onboard::provision` and
`cargo test --test setup_orchestration` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] Both scoped test commands pass, including all eleven new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -rn 'if false' src/onboard/provision/` returns nothing
- [ ] `grep -rn 'vec!\["\*".to_string()\]' src/onboard/provision/` returns nothing
- [ ] No provisioner returns `Ok(())` after emitting `Failed`
- [ ] The CHANGELOG entry for the step-3 default change is written
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 133 updated

## STOP conditions

Stop and report back if:

- Plan 132 has not merged — this is serialized over the same files.
- The typed outcome in step 1 cannot be threaded through both drivers without touching
  more of `src/tui/app.rs` than the two call sites. Plans 135/136 own that file;
  report rather than expanding.
- You cannot distinguish an auth rejection from a transport error for a given
  platform's probe. Default that platform to the warn-and-confirm path and say which.
- Removing the Signal DM option turns out to break an operator workflow that currently
  works by accident. Report it.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 134 replaces the smoke harness that currently
  cannot fail — until it lands, **none of this plan's production changes are covered
  by the existing suite**, which is why this plan writes eleven of its own tests.
  Plan 142 removes the identity strings from the same file and is serialized after
  this one.
- **What a reviewer should scrutinise**: step 2's auth-vs-transport split, since
  getting it wrong either blocks offline installs or keeps persisting bad credentials;
  and that step 5 did not quietly seed an owner.
- **Deliberately deferred**: the 15-way duplication of the IO helpers, which is why
  every fix in step 4 had to be applied per module. Plan 134 extracts them — doing it
  first would have been better, but it depends on the smoke harness existing.
