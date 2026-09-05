# Plan 121: Decompose `channels/mod.rs` into the ten mapped modules

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **This plan is a sequence of independent, revertible PRs, not one change.**
> Each row of the table in step 2 is its own branch, its own review and its own
> merge. Do not attempt the whole decomposition in one PR.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P3
- **Effort**: L
- **Risk**: MED
- **Depends on**: plans/120 (last in the serialized `src/channels/mod.rs` chain)
- **Category**: tech-debt
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

`mod.rs` is 7,480 lines — 3,765 production and 3,716 test — and it is the
highest-churn file in the repo. Every channel change routes through it, so it is a
standing merge-conflict generator, and it mixes orchestration, dispatch, config
reload, route overrides, history, owner policy and per-platform glue in one place,
which CLAUDE.md §3.4 names directly.

The important finding is not that it is big. It is that **it is a shallow star, not
a tangle**: external fan-in is only ten public symbols, every internal seam has 2–4
references, and nearly all of them are reached from one function through one context
value. That is what makes this MED risk rather than HIGH — and what makes a staged
split tractable where a big-bang rewrite would not be.

Two things make it non-trivial and must shape how you work: the in-file tests reach
**private** items via `use super::*`, so each extraction carries its tests or widens
visibility; and `ChannelRuntimeContext`'s 30 private fields are read by nearly every
group, so it stays put.

## Current state

Measured at `f189422`: 7,480 total lines. The `#[cfg(test)] mod tests` block starts
at `:3766` and runs to EOF.

External fan-in — the complete list of symbols used from outside `src/channels/`:

- `start_channels`, `start_channels_with_cancellation` (5 call sites)
- `build_system_prompt_with_mode` (4 from `src/agent/loop_.rs`), `build_system_prompt` (1)
- `channel_supports_announce_delivery` (1 from `src/cron/scheduler.rs`)
- `handle_command`, `channel_roster`, `announce_daemon_reload`, `reload_managed_daemon`

That is the entire surface that must keep working. Everything else is internal.

`ChannelRuntimeContext` (`:222-278`) has 30 fields mixing transport
(`channels_by_name`), model I/O (`provider`, `provider_cache`, `api_key`,
`reliability`), storage (`conversation_histories`, `history_store`) and policy
(`security`, `channel_approval`, `approval_owners`, `tool_approvals`, `guest_gate`).

`process_channel_message` (`:1650-2132`) is a single 483-line function and the hub:
it applies the config reload, dispatches slash commands, resolves the provider,
builds memory context, spawns typing, runs the tool loop, sanitizes, splits and sends.

`:3206-3400` — the agent-stack bootstrap (provider, observer, runtime adapter,
security policy, memory, tool registry, peripheral tools, skills, prompt) runs inside
`start_channels_with_cancellation` before a single channel is constructed.

31 `println!` calls sit alongside the daemon runtime, while `:1658-1663` documents
that `println!` on the *runtime* path corrupts the TUI — the file already holds two
incompatible output contracts.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |
| Feature-gated lint | `cargo clippy --features channel-lark --all-targets -- -D warnings` | exit 0 |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

**Test count is your invariant.** Before you start, record the number of tests
`cargo test --lib channels::` reports. It must be identical after every PR in the
sequence. A test that vanishes during a move is the failure mode of this plan.

## Scope

**In scope**: `src/channels/mod.rs` and the new sibling modules it is split into.

**Out of scope**:
- **Any behaviour change whatsoever.** This is a pure move. If you find a bug while
  moving code, write it down and leave it; a behaviour change hidden inside a
  1,000-line diff is unreviewable, and CLAUDE.md §10 names it explicitly.
- **Splitting `ChannelRuntimeContext`.** Its fields are read by nearly every group.
  It stays in `mod.rs` and its fields become `pub(crate)`. Restructuring it is a
  separate, genuinely high-risk change — do not attempt it here.
- **Moving the agent-stack bootstrap into `src/agent/`.** That is a §6.4 argument with
  its own blast radius and it deserves its own plan.
- Any file outside `src/channels/`.

## Git workflow

- One branch **per row** of the step-2 table: `refactor/decompose-channels-mod`, e.g.
  `refactor/decompose-channels-mod`.
- Conventional commits, e.g. `refactor(channels): extract reply sanitization into sanitize.rs`
- Merge each before starting the next. Do NOT push or open a PR unless the operator
  instructed it.

## Steps

### Step 1: Record the baseline

Record the test count and paste it into every PR in this sequence:

```
cargo test --lib channels:: 2>&1 | tail -3
```

Also record `wc -l src/channels/mod.rs`.

### Step 2: Extract in this order, one PR each

Rows 1–4 are near-mechanical and land first to prove the pattern. Row 5 is the
largest single move and deserves its own careful review. Row 9 is a pure CLI/daemon
separation.

| # | Target module | Source ranges in `mod.rs` | ~Prod LOC | External fan-in |
|---|---|---|---|---|
| 1 | *(already deleted by plan 119)* | — | — | — |
| 2 | `sanitize.rs` | 1267–1452 | 186 | 0 |
| 3 | `prompt.rs` | 314–441, 1144–1266, 2281–2366 | 337 | **5** |
| 4 | `history.rs` | 409–441, 831–935 | 138 | 0 |
| 5 | `routing.rs` (config reload, route overrides, provider cache) | 140–220, 486–1010 | 605 | 0 |
| 6 | `commands.rs` (`/model`, `/provider`, `/models`, `/providers`) | 442–485, 1011–1143 | 176 | 0 |
| 7 | `supervisor.rs` (flock, listener supervision, typing, in-flight accounting) | 282–313, 1453–1648 | 227 | 0 |
| 8 | `factory.rs` (the single construction table from plan 120) | 3400–3583 | ~185 | 0 |
| 9 | `admin.rs` (bind/unbind, pair, daemon reload, roster, doctor, `handle_command`) | 2367–3000 | 634 | **4** |
| 10 | `mod.rs` remainder — re-exports, `ChannelRuntimeContext`, `process_channel_message`, `run_message_dispatch_loop`, `start_channels*` | — | ~950 | **5** |

Line ranges are as of `f189422` and **will shift** as you go. Locate each group by
its function names, not by line number, and re-read the range before each move.

For each row:

1. Move the functions and their `#[cfg(test)]` tests together. Tests that reach
   private items must move with the code they test, or the item becomes
   `pub(crate)` — prefer moving the tests.
2. Widen visibility to `pub(crate)`, never `pub`, unless the symbol is in the
   external fan-in list above.
3. Run the full verification set. **The test count must be unchanged.**
4. Merge before starting the next row.

Row 10's `process_channel_message` is **not** carved up directly — it shrinks as
rows 2–7 pull their code out from under it. Do not try to split the function itself.

### Step 3: Separate the two output contracts

`mod.rs` currently mixes CLI presentation (`println!`) with the daemon runtime, and
the file's own comment explains why that is unsafe on the runtime path. After row 9,
`admin.rs` owns the CLI surface and may keep its `println!`s; the runtime modules
must contain none.

**Verify**: `grep -c 'println!' src/channels/mod.rs` and the runtime modules → 0.

## Test plan

This plan writes **no new behavioural tests** — it is a move, and new tests would
obscure whether the move was faithful.

What it does require:

- The test count from step 1 is unchanged after every row.
- After the final row, one new structural test: assert that the public surface of
  `src/channels` is exactly the ten symbols in the external fan-in list, so a future
  extraction cannot silently widen the module's API.
- If any row tempts you to change a test to make it pass, **stop**. A test that needs
  editing to survive a move means the move was not faithful.

**Verify**: `cargo test --lib channels::` → same count, all pass, after each row.

## Done criteria

ALL must hold, after the final row:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0, and again with `--features channel-lark`
- [ ] `cargo test --lib channels::` reports the **same test count** as the step-1 baseline
- [ ] `wc -l src/channels/mod.rs` is under 1,200
- [ ] No symbol outside the ten-item external fan-in list is `pub`
- [ ] `git log --oneline` shows one commit per table row, each independently revertible
- [ ] No file outside `src/channels/` is modified (`git status`)
- [ ] `plans/README.md` status row for 121 updated

## STOP conditions

Stop and report back if:

- Plan 120 has not landed — this is last in the chain, and 120 creates `factory.rs`'s
  contents.
- A row's move requires changing a test's assertions. That means the move changed
  behaviour; back it out and report what you found.
- The test count drops at any row, even by one.
- A group turns out to have far more internal fan-in than the table says — the map
  was built from a static read, and if reality disagrees, the map is wrong and should
  be corrected before you continue rather than worked around.
- You find yourself needing to make `ChannelRuntimeContext`'s fields `pub` (not
  `pub(crate)`), or needing to split the struct. Both are out of scope and both are
  signals that the row's boundary is wrong.
- Any row exceeds roughly 700 moved lines. Split it further rather than shipping an
  unreviewable diff.

## Maintenance notes

- **What interacts with this**: every other plan in group A (115–120) touches this
  file, which is why this one is last. Plans in groups C–K do not touch it, so they
  can proceed in parallel with this sequence — but each row is still a wide diff, so
  coordinate timing with anyone holding a long-lived branch.
- **What a reviewer should scrutinise**: that each PR is a *move* — `git diff
  --stat` should show near-equal additions and deletions, and the moved hunks should
  be textually identical. Any row where the diff shows net new logic needs a second
  look.
- **Why the order matters**: rows 2–4 are small and prove the tests-move-with-code
  pattern before row 5 attempts 605 lines. Doing row 5 first is the most likely way
  to end up with an unreviewable diff and an abandoned branch.
- **Deliberately deferred**: moving the agent-stack bootstrap out of
  `start_channels_with_cancellation` into `src/agent/`. It is the strongest §6.4
  argument in the file — orchestration living in the transport layer — and it is a
  separate plan with a separate blast radius.
