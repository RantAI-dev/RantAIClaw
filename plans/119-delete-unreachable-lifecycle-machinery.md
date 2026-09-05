# Plan 119: Delete the unreachable channel-lifecycle machinery

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs src/channels/registry.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: plans/118 (serialized chain over `src/channels/mod.rs`)
- **Category**: tech-debt
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

336 lines of channel-lifecycle machinery have never been reachable. That would be
ordinary debt except for two things: its doc comment tells readers the gateway uses
it, and it is a loaded trap. `add_channel` does not register a channel — it spawns a
**real listener** without taking the single-runner flock that exists to stop two
runners of the same channel. Anyone who wires it up as the doc instructs gets
duplicate Telegram long-polls (409 flapping), duplicate replies on WhatsApp, and a
status API reporting everything healthy — because `remove_channel` logs "aborting"
while actually *detaching* the task, and `ChannelStatus::Error` is never constructed
anywhere.

It is also the third divergent copy of the channel construction table, which is why
`channels doctor` silently lost Mattermost (plan 120 fixes that; this plan removes
the copy so 120 has two tables to unify, not three).

Separately, `channel add` accepts a `--config` argument it discards and reports
guidance as a non-zero exit.

## Current state

`src/channels/registry.rs:1-159` — 159 lines, **zero tests**, no `#[cfg(test)]` block.

`src/channels/mod.rs:3002-3008` — the doc comment that is not true:

```rust
/// This is used by the gateway to seed the registry at startup so the Config API
/// can report which channels are active and manage their lifecycle.
```

`src/gateway/` never references either symbol. A repo-wide grep for
`register_configured_channels` and `ChannelRegistry::new` returns only the
definition at `src/channels/mod.rs:3009`, the `pub use` at `src/channels/mod.rs:66`,
and the doc reference at `:3178`.

`src/channels/registry.rs:59-69` — `add_channel` spawns a listener:

```rust
        let handle = tokio::spawn(async move { channel.listen(tx, cancel_clone).await });
```

with no call to `acquire_channel_lock` (`src/channels/mod.rs:1467-1489`), the
advisory flock `spawn_supervised_listener` takes at `:1529`.

`src/channels/registry.rs:94-105` — the misleading abort:

```rust
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, handle.task).await {
            …
            Err(_) => { tracing::warn!("… did not stop within {}s, aborting", …); }
```

The timeout drops the `JoinHandle`, which **detaches** the task in Tokio. `.abort()`
is never called anywhere in the file.

`src/channels/registry.rs:77` and `:133-138` — `status` is written `Running` once at
insert and never mutated; `ChannelStatus::Error(String)` (`:20`) is never
constructed, so `list_channels` reports Running for a dead listener.

`src/channels/mod.rs:2701-2711` — the CLI arms:

```rust
        ChannelCommands::Add { channel_type, config: _ } => {
            anyhow::bail!("Channel type '{channel_type}' — use `rantaiclaw onboard` to configure channels")
        }
```

and `:2652-2660` — `Start`/`Run`/`Doctor` bail with an internal routing message
("must be handled in main.rs (requires async runtime)").

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |
| CLI surface | `cargo test --lib` (scoped to whatever covers `handle_command`) | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/channels/registry.rs` — delete
- `src/channels/mod.rs` — remove `register_configured_channels`, the `pub mod` and
  `pub use` at `:65-66`, and fix the `channel add` / dispatch-only CLI arms

**Out of scope**:
- **`spawn_relay` in `src/channels/approval_relay.rs`.** It is also dead, and it is
  **plan 122's** decision — that plan owns `approval_relay.rs`. Do not touch it here
  even though it is the same class of finding. (This was a real overlap in the first
  draft of the plan index; it is corrected there.)
- Unifying the two remaining construction tables — plan 120.
- `src/gateway/` — nothing there references the registry, so nothing there changes.

## Git workflow

- Branch: `refactor/delete-unreachable-lifecycle-machinery`
- Conventional commits, e.g. `chore(channels): delete the unreachable channel registry`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Confirm it is genuinely unreachable, and ask before deleting

Before removing anything, re-run the reachability check yourself and paste the
output into the PR body:

```
grep -rn 'register_configured_channels\|ChannelRegistry' src/ tests/ benches/ examples/ crates/
```

Expected: only the definition, the `pub use`, and the doc reference.

Then **ask the maintainer** whether the gateway live-config path the doc describes is
imminent work. Deleting in-flight scaffolding is worse than leaving it. If they say
it is coming, stop and convert this plan into "correct the doc and take the flock"
instead — the trap in `add_channel` must not survive either way.

**Verify**: the grep output matches the expectation.

### Step 2: Delete

Remove `src/channels/registry.rs`, `register_configured_channels`, and the two
re-exports. Remove any now-unused imports your deletion orphans — and **only**
those; do not tidy unrelated imports.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Make the CLI arms honest

- Drop the discarded `config` field from `ChannelCommands::Add`, or implement it.
  A documented no-op flag is worse than no flag.
- Print the guidance for `Add` / `Remove` to stdout and return `Ok(())`. These are
  informational outcomes; a non-zero exit makes scripts wrapping
  `rantaiclaw channel add` see a failure.
- For `Start` / `Run` / `Doctor`, which are dispatched in `main.rs` and cannot reach
  here: either split them into a separate enum so `handle_command` only handles what
  it owns, or replace the user-facing bail with an `unreachable!` carrying the
  invariant. Do not leave an internal routing detail as a user-visible error string.

**Verify**: `cargo test --lib channels::` → all pass.

## Test plan

This plan mostly deletes, so the test work is small and specific.

1. `channel_add_reports_guidance_without_failing` — assert the command returns
   `Ok(())` and that the guidance text is produced.
2. `channel_remove_reports_guidance_without_failing` — same.
3. If you kept the `Start`/`Run`/`Doctor` arms as `unreachable!`, no test is
   possible or wanted; say so in the PR. If you split the enum, assert the split
   compiles by construction (the type system is the test).

Do **not** write tests for the deleted registry.

**Mutation check**: not applicable to a deletion. For tests 1 and 2, confirm they
fail if you restore the `bail!` — that is the equivalent check here.

**Verify**: `cargo test --lib channels::` → all pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` passes, including the two new tests
- [ ] `src/channels/registry.rs` no longer exists
- [ ] `grep -rn 'register_configured_channels\|ChannelRegistry' src/ tests/` returns nothing
- [ ] `grep -rn 'spawn_relay' src/` still shows it present and untouched — it is plan
      122's, and touching it here is a scope violation
- [ ] The step-1 grep output and the maintainer's answer are recorded in the PR body
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 119 updated

## STOP conditions

Stop and report back if:

- Plan 118 has not landed — this chain is serialized over `mod.rs`.
- The maintainer says the gateway registry path is imminent. Convert to the
  fix-in-place variant described in step 1 rather than deleting.
- The grep in step 1 finds a caller you did not expect — especially in `crates/` or
  a feature-gated module you cannot compile. A caller behind `channel-matrix` would
  not show up in a default build's compile errors.
- Deleting orphans an import in a way that changes behaviour rather than just
  compiling — that would mean the module had a side effect, which is worth reporting.

## Maintenance notes

- **What interacts with this**: plan 120 unifies channel construction. Doing this
  deletion first means 120 has two divergent tables to reconcile instead of three.
- **What a reviewer should scrutinise**: that the deletion did not take anything live
  with it — the `pub use` at `:66` is the risky line, since re-exports can have
  consumers a grep for the type name misses if they alias it.
- **Why this is P2 and not P3**: the dead code itself is harmless; the trap is not.
  `add_channel` spawning an unflocked listener is a loaded gun aimed at whoever
  believes the doc comment, and the doc comment is the thing that makes them pull the
  trigger.
