# Plan 149: Publish per-channel verification status before building any harness

> **Executor instructions**: Follow this plan step by step. Step 1 is the whole
> deliverable and ships on its own; step 4 is a separable second phase that should
> only begin once step 1 is merged. Run every verification command. If anything in
> "STOP conditions" occurs, stop and report. When done, update the status row for this
> plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- README.md docs/reference/channels.md`
>
> **Content, not line numbers, is what matters here** — this plan describes the state
> of other plans' work. Before writing, check which of 115–148 have merged and record
> what is true now.

## Status

- **Priority**: P3
- **Effort**: S (the status matrix) + L (the harness, if pursued)
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Sixteen of seventeen wired channels have **never been verified against a live
platform**, and four independent findings in this audit are downstream of that: the
render-target rollout skipped Matrix; pairing does not take effect on five channels;
Lark's webhook has no authenticity check; three config keys are no-ops. None of it was
caught by a test, because none of it is the kind of thing a test catches.

Eighteen platforms is the product's headline claim. Breadth without a verification
story converts into support burden — which is why `channels.md` has grown a sixteen-row
log-keyword triage appendix and an FAQ for one channel's most common failure.

The repo already publishes exactly this distinction for a different subsystem.
`docs/reference/kb-providers.md:33` says: *"Transport exists and is unit-tested against
a stubbed endpoint; not verified against live Cohere."* That sentence is the model.
Adopting it for channels costs a day, needs no infrastructure, and converts "18
channels, unknown quality" into a prioritised worklist.

**The status matrix is the deliverable. The harness is optional.** They are separated
here because the matrix delivers most of the value immediately and the harness is the
kind of scope that quietly becomes an E2E platform.

## Current state

`README.md:129` — "Connect your agent to any combination of channels simultaneously."
`README.md:141` and `docs/reference/channels.md:73` present Matrix and Lark as a build
flag away.

`.github/workflows/ci-run.yml:129-137` — no CI job builds `channel-matrix` or
`channel-lark` (plan 143 adds Lark). `.github/workflows/pub-release.yml:320` builds
with default features, so **neither ships in a release binary** — while the docs imply
otherwise.

Live-verification status as established by this audit: **Telegram** is the only channel
confirmed against a real platform (during the markdown-renderer effort). Every other
wiring rests on documentation and review.

`docs/reference/kb-providers.md:33` — the vocabulary to reuse.

`src/channels/mod.rs:2796` — `channel doctor` already establishes the per-channel
health-command shape a `verify` subcommand would extend.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Docs lint | the repo's markdown check (see `dev/ci.sh`) | exit 0 |
| Link check | the repo's link-integrity check | exit 0 |

Step 1 needs no build.

## Scope

**In scope**: `README.md`, `docs/reference/channels.md`, and — only if step 4 is
approved — a `channel verify` subcommand and a manually-dispatched workflow.

**Out of scope**: fixing anything the matrix reveals as unverified. The matrix's job is
to make the gap visible and honest, not to close it. Automating verification in
per-PR CI — it needs real platform accounts and credentials, and belongs in a
manually-triggered workflow if it exists at all.

## Git workflow

- Branch: `feat/publish-channel-verification-status`
- **Two separate PRs**: the matrix, then (only if approved) the harness. Do not bundle.
- Conventional commits, e.g. `docs(channels): publish per-channel verification status`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Publish the status matrix

Add a `Verified` column to the channel tables in `README.md` and
`docs/reference/channels.md`, using `kb-providers.md`'s vocabulary. Four honest values:

- **live-verified** — confirmed against a real platform account; say when
- **built and unit-tested** — compiles, has tests, never driven live
- **built, not tested by CI** — compiles only under a feature flag no CI job runs
- **unbuildable** — cannot compile at all (Matrix, pending plan 145)

Seed it from this audit's findings. Telegram is live-verified. Everything else is one
of the other three — check each against the current state of CI and the release build
rather than assuming.

Two more corrections belong in the same PR because they are the same claim:

- `README.md:141` and `channels.md:73` imply Matrix and Lark ship behind a build flag.
  **Release binaries include neither.** Say so.
- `channels.md:141` describes the Matrix render target as "deferred". It shipped; only
  the four-line wiring is blocked. Correct it (or defer to plan 144, which owns the
  channel-reference corrections — coordinate so you do not both edit the same line).

**Verify**: docs checks pass; every row's value traces to something you checked.

### Step 2: Say what "verified" would mean

One short section defining the bar, so the column cannot drift into decoration: a
scripted round trip — send a message, receive it, assert the echo — against a real
account, with the date and the platform's account type recorded.

Without a definition, "verified" becomes whatever the last person to touch the row
thought it meant.

**Verify**: the definition is in the document.

### Step 3: Stop and ship

Merge the matrix on its own. It is the deliverable, it needs no infrastructure, and it
is useful the day it lands.

**Only proceed to step 4 if the maintainer asks for it.**

### Step 4: Design `channel verify <name>` — only if approved

Scope it to **one scripted round trip per channel**: send, receive, assert echo. Reuse
credentials from the existing secret store. Extend the shape `channel doctor` already
establishes.

Wire it to a **manually-dispatched** workflow, not per-PR CI. It needs real platform
accounts, and a per-PR job that depends on eighteen third-party services would be red
more often than green — at which point it gets ignored, and an ignored check is worse
than none.

Resist the pull toward a full E2E platform. If the design starts needing fixtures,
replay, or a test-account provisioning story, that is the signal to stop and write it
down instead.

**Verify**: the design is committed; implementation is a separate decision.

## Test plan

Step 1 is documentation, so:

1. Every row's value traces to a checked fact — CI config, release config, or this
   audit's record. List the evidence per row in the PR.
2. Docs lint and link checks pass.
3. No row claims live-verified without a date and an account type.

If step 4 is built:

4. `channel verify <name>` fails clearly when credentials are absent, rather than
   hanging or reporting a false pass.
5. It exits non-zero on a failed round trip.

**Verify**: docs checks → exit 0.

## Done criteria

For the step 1–3 PR, ALL must hold:

- [ ] The repo's markdown lint and link checks pass
- [ ] Both `README.md` and `docs/reference/channels.md` carry the `Verified` column
- [ ] Every channel has a value, and the PR lists the evidence per row
- [ ] The "what verified means" definition is published
- [ ] The release-binary contents claim is corrected
- [ ] No production file is modified (`git status`)
- [ ] `plans/README.md` status row for 149 updated

## STOP conditions

Stop and report back if:

- You cannot establish a channel's status from CI config, release config and this
  audit's record. Mark it **unknown** and say so — a guessed value in a column whose
  entire purpose is honesty defeats the column.
- Plan 144 is mid-flight and editing the same lines in `channels.md`. Coordinate; do
  not both rewrite the same table.
- The maintainer has not approved step 4. Ship steps 1–3 and stop. That is a complete,
  useful outcome.
- The step-4 design starts requiring test-account provisioning or replay
  infrastructure. Write down what it would take and stop.

## Maintenance notes

- **What interacts with this**: plan 143 adds the Lark CI job, which moves Lark from
  "built, not tested by CI" to "built and unit-tested". Plan 145 decides Matrix's fate,
  which determines whether "unbuildable" stays a row. Plan 144 owns the other
  channel-reference corrections. Every one of those should update this column in its
  own PR — note that expectation when you touch `plans/README.md`.
- **What a reviewer should scrutinise**: that no row was marked live-verified on the
  strength of a unit test. That conflation is the thing this plan exists to prevent.
- **Why the matrix is worth more than the harness**: the harness verifies channels one
  at a time at high cost. The matrix tells an operator, today, which of the eighteen
  claims they can rely on — and tells the team where to spend the next effort.
