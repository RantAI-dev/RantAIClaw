# Plan 145: Decide `matrix-sdk`, `whatsapp-web`-in-default, and the duplicate transport stacks

> **Executor instructions**: This plan asks for **decisions and their consequences**,
> not a predetermined patch. Steps 1–3 produce a written recommendation for the
> maintainer; step 4 implements only what they approve. Do not implement any option
> before the decision is recorded. If anything in "STOP conditions" occurs, stop and
> report.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- Cargo.toml Cargo.lock deny.toml .github/workflows/`
>
> **Line numbers WILL have drifted** if earlier plans merged first. Relocate by symbol
> name and continue.

## Status

- **Priority**: P3
- **Effort**: M (decision) + variable (implementation)
- **Risk**: MED
- **Depends on**: none. **Requires a maintainer decision** before step 4.
- **Category**: migration
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Three dependency facts, none of which is a defect, all of which are costs nobody has
consciously accepted.

`matrix-sdk` is **421 of 898 packages** in the graph, 88 of them exclusive. It carries
two RUSTSEC suppressions, a permanently-disabled CI dimension, and 1,168 lines of
channel code that no job type-checks — and it ships in no release binary, while the
README implies a build flag is all that stands in the way. "Blocked upstream" has been
the answer for months; the status quo has a real cost and no expiry date, and choosing
it by *inaction* is the one outcome nobody would defend if asked.

`whatsapp-web` is in the default feature set, so every user links a full WhatsApp
protocol stack — including third-party pre-1.0 reimplementations of the Signal
protocol and the Noise handshake — plus a **second** blocking HTTP client. The two
other heavyweight platform channels are opt-in. On a product whose CI keeps a binary
size floor, that asymmetry is worth deciding rather than inheriting.

And the default binary links **three HTTP client stacks and three WebSocket
implementations** to do one job each.

## Current state

`Cargo.toml:42` pins `matrix-sdk 0.16` with `e2e-encryption`. Lockfile closure: 421 of
898 reachable, 88 exclusive (`imbl`, `html5ever`, the `vodozemac`/`ruma` stack,
`blake3`, `decancer`, `aquamarine`, …).

`.github/workflows/ci-run.yml:121-128` omits `--all-features` for this crate.
`deny.toml:14-19` and `:50-52` carry two RUSTSEC ignores whose sole entry path is
matrix-sdk. `.github/workflows/pub-release.yml:320` builds
`--profile release-fast --locked` with no `--features`, so releases do not ship Matrix.

`Cargo.toml:253` — `default = ["tui", "whatsapp-web", "remote-install", "kb"]`.
`Cargo.toml:288` — `whatsapp-web` pulls `wa-rs`, `wa-rs-core`, `wa-rs-binary`,
`wa-rs-proto`, `wa-rs-ureq-http`, `wa-rs-tokio-transport`, `serde-big-array`, `prost`.
`Cargo.toml:266-267` — `channel-matrix` and `channel-lark` are opt-in.

`Cargo.lock:6085` + `:6129` — `reqwest 0.12.28` (ours) and `reqwest 0.13.3` (via
`rig-core 0.37`, a default dep). `:7793` + `:7809` — `tokio-tungstenite` 0.23.1 and
0.28.0. `:8338` — `ureq 3.2.0`, entering via `wa-rs-ureq-http`; the same closure brings
`tokio-websockets`. Exclusive cost of the wa-rs group: 31 crates.

`deny.toml:96` already sets `multiple-versions = "warn"`, so this is known but
unmeasured. `scripts/ci/check_binary_size.sh:13` keeps a 5 MB aspirational target
against a ~31 MB reality; `:18-22` records the `whatsapp-web`-in-default decision as
already made once.

`deny.toml:100-102` sets `unknown-git = "deny"` with `allow-git = []`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Size, default | `cargo build --profile release-fast && ls -l target/release-fast/rantaiclaw` | a number |
| Size, minus a feature | same with `--no-default-features --features …` | a number |
| Crate weight | `cargo tree --edges normal --prefix none \| sort -u \| wc -l` | a number |

**These builds are expensive on this disk-constrained box** (~27 GB for a full test
run; release builds have exhausted the disk in past efforts). Check free space first,
build one configuration at a time, and clean between. If you cannot build safely, say
so — a measured decision is the point, and an unmeasured one is worse than a delayed
one.

## Scope

**In scope**: `Cargo.toml`, `deny.toml`, `.github/workflows/`, and the docs that state
what release binaries contain.

**Out of scope**: implementing any option before the maintainer decides. Removing the
Matrix channel's *code* — that is a separate change even if option 4 is chosen.
`src/channels/whatsapp_web.rs`'s own defects (plan 123).

## Git workflow

- Branch: `chore/dependency-decisions`
- Commit the written recommendation first, on its own, so the decision is reviewable
  before any manifest change.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Measure

Produce real numbers, not estimates:

- stripped binary size: default, minus `whatsapp-web`, minus `kb`
- crate count: default versus each of the above
- if `cargo bloat` is available, the top contributors

Record them in the PR. Every argument below is currently made from crate counts; a
size delta is what turns it into a decision.

**Verify**: the numbers are in the PR, with the exact commands used.

### Step 2: Write up the matrix-sdk options

Present all four with honest blast radius. Do not pre-select in the write-up itself —
the recommendation goes at the end, separately.

1. **Wait for upstream.** Zero code. Cost is indefinite: the suppressions accumulate as
   the ruma/imbl stack ages, and `matrix.rs` keeps drifting from a trait surface it is
   never compiled against — it has already missed the entire `render_target` rollout.
   Defensible **only** with a dated review trigger attached.
2. **`[patch.crates-io]` a fork raising the recursion limit.** One line in a fork, ~6
   in `Cargo.toml`, zero in `src/`. But it collides head-on with `deny.toml`'s
   `unknown-git = "deny"`, so it needs a documented source-policy exception — that is
   the real cost, not the patch. Requires re-forking on every matrix-sdk release.
   Restores `--all-features` CI **and** Matrix compilation in one move.
3. **Pin an older matrix-sdk.** Blast radius unknown until attempted; `matrix.rs` uses
   `RoomMessageEventContent`, `MessageType::{Text,Notice}` and the `Room`/sync API, all
   of which moved across 0.1x. Realistically a day of API churn plus new advisory
   exposure on an older ruma. Worth a two-hour timebox only if option 2's exception is
   refused.
4. **Drop the channel.** Removes 88 crates and roughly halves the all-features
   type-check surface. Mechanically low risk; strategically high — Matrix is the only
   E2EE self-hosted channel, it has a dedicated setup guide, and `channels.md:16` has
   an FAQ for it, which means users ask.

**Recommendation to put in the write-up**: option 2, with a written `deny.toml`
exception, falling back to option 1 **with an expiry date** if the exception is
refused. Option 4 should not happen by inaction — which is what is happening now.

**Verify**: the write-up is committed and contains all four options with the measured
numbers from step 1.

### Step 3: Write up the packaging questions

- **`whatsapp-web` in default.** `check_binary_size.sh:18-22` shows this was decided
  once; present the size delta from step 1 and ask whether it still holds now that the
  channel is known to carry a Signal-protocol reimplementation and no test module.
  Note that moving it out is a **user-visible packaging change** — anyone building from
  source silently loses the channel — so it needs a release note and a check of the
  packaging scripts.
- **The duplicate stacks.** The cheap half is replacing `wa-rs-ureq-http` with a
  `reqwest`-backed transport against the trait `wa-rs-core` already exposes, which
  removes `ureq` and `ureq-proto` outright and brings WhatsApp Web traffic under the
  `[proxy]` config it currently bypasses. The `rig-core` reqwest 0.13 duplicate is not
  worth forcing — leave it until rig-core's own cadence catches up.

**Verify**: the write-up covers both with numbers.

### Step 4: Implement only what was approved

After the maintainer decides, implement it. If option 2 was chosen, the `deny.toml`
exception must be written **with its rationale in the file**, not just in a PR body —
the next person to read `deny.toml` needs to know why the policy has a hole.

Whichever matrix option wins, one follow-up applies in every case: add a
`channel-matrix` entry to the CI features matrix so the module is at minimum
type-checked, and make the release configuration state explicitly whether Matrix ships
— today it silently does not while `README.md:141` implies otherwise.

**Verify**: the implemented change matches the recorded decision, and nothing else.

## Test plan

This plan changes packaging, not behaviour. What must hold:

1. The build succeeds in every configuration the repo claims to support — default,
   `--no-default-features`, and each feature the docs mention.
2. If a feature moved out of `default`, the release workflow and install docs were
   updated in the same PR.
3. If a `[patch.crates-io]` was added, `cargo deny check` passes with the documented
   exception and fails without it.
4. If a transport was replaced, the affected channel's tests still pass and its traffic
   now honours `[proxy]`.

**Verify**: the configurations above build; `cargo deny check` behaves as described.

## Done criteria

- [ ] Step 1's measurements are in the PR with the exact commands
- [ ] The four matrix-sdk options are written up with blast radius and a recommendation
- [ ] The packaging questions are written up with numbers
- [ ] **A maintainer decision is recorded** before any manifest change
- [ ] The implemented change matches the decision and nothing else
- [ ] Any `deny.toml` exception carries its rationale in the file
- [ ] The release configuration states whether Matrix ships
- [ ] `plans/README.md` status row for 145 updated with the decision taken

## STOP conditions

Stop and report back if:

- You cannot build safely — disk exhaustion has stopped release builds in this repo
  before. Report the constraint; do not present crate counts as if they were size
  measurements.
- The maintainer has not decided. **Do not implement a default.** This plan's output
  can legitimately be a written recommendation and nothing else.
- Option 2 is chosen and the fork cannot be made to build either. Report before
  spending time on option 3.
- Removing `whatsapp-web` from `default` breaks a packaging script you cannot update
  within this plan's scope.

## Maintenance notes

- **What interacts with this**: plan 143 adds the `channel-lark` CI job and
  deliberately leaves Matrix out because it cannot compile — whichever option wins
  here determines whether that changes. Plan 123 fixes WhatsApp Web's own defects and
  is unaffected by where the feature sits. Plan 144 documents what release binaries
  contain and should be updated in the same wave.
- **What a reviewer should scrutinise**: that the decision was actually recorded before
  the manifest changed. A PR that implements an option and describes it as obvious is
  the failure mode this plan exists to prevent.
- **Why this is P3 despite the numbers being large**: nothing here is broken. It is
  accumulated cost with no owner, and the valuable output is a decision — which is why
  the plan is structured to produce one even if no code changes.
