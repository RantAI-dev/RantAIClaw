# Plan 144: Correct `channels.md` where it contradicts the code; document the approval model

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- docs/reference/channels.md docs/reference/config.md .env.example`
>
> **Content, not line numbers, is what matters here.** Several plans in this effort
> change the behaviour this document describes. Before writing, check which of
> 115–143 have merged and describe **what the code does now**, not what this plan
> assumed.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none, but see "Sequencing" below
- **Category**: docs
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

`docs/reference/channels.md` is a runtime-contract reference — CLAUDE.md §4.1 requires
it to track behaviour changes. It contradicts the code on four points, and two of them
cost an operator a working channel.

The most expensive: WhatsApp's `app_secret` is documented as "optional but
recommended", while the gateway returns 401 before parsing when it is absent. Follow
the setup section verbatim and you get a channel that connects, passes `channel
doctor`, and silently rejects every inbound message. The troubleshooting checklist does
not mention it, and the log-keyword table files the rejection strings under the
*generic* Webhook row rather than WhatsApp's.

Then: the document states the default feature set is empty while it has four entries
including a channel — and the same page warns about that channel's account risk, so a
reader concludes the reverse-engineered WhatsApp client is opt-in when it ships
enabled. Slack is described as "events API" when it polls. Lark's verification token is
described as providing "callback authenticity" when it is checked only on the challenge
branch.

Separately, **Linq is entirely absent** — the only channel with both a
verified-signature webhook and inbound image support has no setup section, no delivery
row and no allowlist field listed. And the chat-approval security model is documented
only in a file nothing links to, so the secure default (no owners ⇒ nobody approves ⇒
privileged tools auto-deny) reads as "the bot refuses to do anything", and the
documented alternative is one unlinked directory away.

## Current state

`docs/reference/channels.md:77`:

> Default builds are lean (`default = []`) and do not include Matrix/Lark.

`Cargo.toml:253` — `default = ["tui", "whatsapp-web", "remote-install", "kb"]`.
`README.md:141` correctly says WhatsApp Web is default-on, so the two canonical docs
contradict each other.

`channels.md:264` — `app_secret = "your-app-secret"     # optional but recommended`
against `src/gateway/mod.rs:1943-1956`, which 401s without it, with the comment
"fail-closed, per deny-by-default for exposure surfaces".

`channels.md:107` — "| Slack | events API |" against `src/channels/slack.rs:5` and
`:181`, which poll `conversations.history`. `channels.md:209` documents `app_token`,
which no code reads.

`channels.md:372` — the Lark wizard's "optional webhook verification token prompt
(recommended for stronger callback authenticity checks)" against
`src/channels/lark.rs:1062-1069`, where the token is checked only inside the challenge
branch and `.map_or(true, …)` passes when absent. `channels.md:353` documents
`encrypt_key`, which no code reads.

Linq: no row in the §2 delivery table (`:102-119`), no `allowed_senders` entry in the
§3 field list (`:164-170`), no §4 subsection — while `src/channels/linq.rs` is a full
implementation with a signed webhook and `README.md:145` advertises it.

`docs/security/per-role-permissions.md:17-51` documents `approval_owners`,
`guest_allowed_tools` and `guest_allowed_commands` accurately. Zero references to it
from `channels.md`, `config.md`, the runbook, troubleshooting, `README.md` or
`CLAUDE.md`. `config.md:570-598` documents `[channels_config]` without any of the four
security fields.

`channels.md` §3 claims allowlist semantics are "empty / `*` / explicit list" for all
channels, while `src/channels/email_channel.rs:132-141` also accepts bare-domain and
`@domain` entries and `src/channels/matrix.rs:170` matches case-insensitively.

`RANTAICLAW_LINQ_SIGNING_SECRET`, `RANTAICLAW_NEXTCLOUD_TALK_WEBHOOK_SECRET` and
`RANTAICLAW_WHATSAPP_APP_SECRET` can be supplied by environment; `.env.example` lists
none, and `channels.md` mentions only the Nextcloud one (`:413`).

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Markdown lint | the repo's usual docs check (see `dev/ci.sh`) | exit 0 |
| Link check | the repo's link-integrity check | exit 0 |

No Rust build is needed. This is a docs-only plan.

## Scope

**In scope**: `docs/reference/channels.md`, `docs/reference/config.md`, `.env.example`,
and cross-links from `docs/start/troubleshooting.md` and `README.md` where they point
at the corrected material.

**Out of scope**: production code — if the doc and the code disagree and you judge the
**code** wrong, that is a finding for another plan; document what the code does and
note the disagreement. `docs/security/per-role-permissions.md` is accurate; link it,
do not rewrite it. Multilingual parity — this is an English-only doc system per
CLAUDE.md §4.1 and promising translations the repo does not ship is itself a defect.

## Sequencing

This plan describes behaviour that several other plans change. Before writing, check
which have merged:

- **124** — Lark's webhook gains real authenticity checking; the §4.11 text changes
  completely depending on whether it has landed.
- **133** — provisioning defaults change (empty allowlist stops meaning `*`).
- **145** — may move `whatsapp-web` out of the default feature set, which changes the
  §"Build Feature Toggles" correction.
- **146** — may delete or implement `app_token`, `encrypt_key` and `webhook.port`.

If those are still pending, document **current** behaviour and add a dated note saying
which plan will change it. Do not document a future state.

## Git workflow

- Branch: `docs/channels-doc-corrections`
- Conventional commits, e.g. `docs(channels): correct the default feature set and app_secret requirement`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Fix the four contradictions

- Replace the `default = []` claim with the actual list, and state plainly that
  WhatsApp Web ships **enabled**, cross-linking the account-risk warning already on the
  page at `:278-292`.
- Mark `app_secret` **required for Cloud API mode**, state the 401 behaviour, quote the
  exact warn line from `src/gateway/mod.rs:1950` so it is greppable, and add it to the
  §6 checklist and the troubleshooting keyword table under WhatsApp's row.
- Change Slack's delivery mode to "polling (`conversations.history`)" and note the
  latency and rate-limit implications. Mark `app_token` accepted-but-unused pending
  Socket Mode (or reflect plan 146's outcome).
- Rewrite the Lark verification-token text to describe what is actually checked. If 124
  has not merged, say the event path has no verification and recommend fronting it with
  an authenticating reverse proxy.

**Verify**: each claim you write can be traced to a `file:line` you have read.

### Step 2: Add a per-channel inbound-authenticity column

Add a column to the §2 table with four values: HMAC-verified / shared-secret / none /
n/a-outbound-transport.

Today a reader cannot tell WhatsApp, Linq and Nextcloud Talk (all HMAC-verified) apart
from Lark (nothing). That distinction is the single most useful thing this document
can carry for an operator deciding what to expose.

**Verify**: every row's value matches the code you checked.

### Step 3: Give Linq a section

Add a §2 row, `allowed_senders` to the §3 field list, and a §4 subsection with its
config keys from `LinqConfig` in the schema, its signed-webhook requirement, and
`RANTAICLAW_LINQ_SIGNING_SECRET`.

**Verify**: an operator could configure Linq from this document alone.

### Step 4: Make the approval model findable

Add an "Approval & roles" section to `channels.md` summarising owner-versus-guest in
about five lines and linking `docs/security/per-role-permissions.md`. Link the same
document from `config.md`'s `[channels_config]` section and from troubleshooting's
"Channel issues".

State the failure mode explicitly: with no `approval_owners`, privileged tools
auto-deny, and the fix is to add an owner — **not** to enable `autonomous_tools`. That
sentence is the whole point of this step.

Also document `autonomous_tools` and, per plan 122, the `approval_owners = ["*"]`
wildcard — an accepted value that no document currently mentions.

**Verify**: the links resolve; the link-integrity check passes.

### Step 5: Correct §3's allowlist semantics

State the per-channel matching rules rather than one blanket rule: email's bare-domain
and `@domain` matching, Matrix's case-insensitivity, and Telegram's normalisation. An
allowlist that is more permissive than documented is a security-boundary
misdescription.

**Verify**: each rule traced to the code.

### Step 6: Complete `.env.example`

Add the three webhook-secret environment variables. Keeping secrets out of
`config.toml` is the right pattern and it is currently undiscoverable.

**Verify**: `.env.example` lists all three with a one-line comment each.

## Test plan

Docs have no unit tests; the checks are:

1. Every factual claim added or changed traces to a `file:line` you read — list them in
   the PR.
2. The markdown lint and link-integrity checks pass.
3. `grep -n 'default = \[\]' docs/reference/channels.md` returns nothing.
4. `grep -n 'optional but recommended' docs/reference/channels.md` returns nothing on
   the `app_secret` line.
5. `grep -rn 'per-role-permissions' docs/reference/ docs/start/` returns at least three
   links.

**Verify**: the repo's docs checks → exit 0.

## Done criteria

- [ ] The repo's markdown lint and link checks pass
- [ ] All four contradictions from step 1 are corrected
- [ ] The §2 table has an inbound-authenticity column with a value for every row
- [ ] Linq has a delivery row, an allowlist field entry and a setup subsection
- [ ] `docs/security/per-role-permissions.md` is linked from at least three places
- [ ] `autonomous_tools` and the `approval_owners` wildcard are documented
- [ ] `.env.example` lists the three webhook-secret variables
- [ ] The PR body lists the `file:line` evidence for each changed claim
- [ ] No production file is modified (`git status`)
- [ ] `plans/README.md` status row for 144 updated

## STOP conditions

Stop and report back if:

- The code and the doc disagree and you believe the **code** is wrong. Document current
  behaviour, note the disagreement, and report it — do not "fix" the doc to describe a
  behaviour nobody implemented.
- A plan listed in "Sequencing" is mid-flight, so the behaviour is about to change.
  Document current behaviour with a dated note naming the plan.
- You cannot determine a channel's inbound-authenticity value from the code. Mark it
  unknown and say so rather than guessing — a wrong value here would be worse than the
  missing column.

## Maintenance notes

- **What interacts with this**: 124, 133, 145 and 146 all change behaviour this
  document describes. If any lands after this plan, its own PR should update the
  affected section — note that expectation in each of their maintenance sections when
  you touch `plans/README.md`.
- **What a reviewer should scrutinise**: that step 1's `app_secret` correction reached
  **all three** places it needs to be — the setup section, the §6 checklist and the
  troubleshooting table. Fixing only the first leaves the operator's recovery path
  broken.
- **Why the authenticity column is worth more than the corrections**: the four
  contradictions are point fixes. The column is the thing that makes the next
  discrepancy visible without an audit.
