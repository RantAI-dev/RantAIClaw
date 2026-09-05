# Plan 051: Stop using tool registration as an enforcement mechanism

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **This plan removes code.** If you find yourself adding a mechanism rather
> than deleting one, re-read "Why this matters" — you have probably
> misunderstood the intent.
>
> **Drift check (run first)**: `git diff --stat 3edb236..HEAD -- src/tools/mod.rs src/agent/prompt.rs src/channels/mod.rs src/gateway/mod.rs src/agent/agent.rs src/agent/loop_.rs`
> Compare the "Current state" excerpts against the live code. Line numbers
> drifting by a line or two while the quoted text matches is **not** a STOP —
> only a content mismatch is.

## Status

- **Priority**: P2
- **Effort**: L
- **Risk**: HIGH (`src/tools/**`, `src/channels/**`, `src/gateway/**`)
- **Depends on**: none. **This plan stands alone** — see "Ordering" below. It
  pairs naturally with `plans/050-policy-refresh-carries-process-state.md` but
  does not require it.
- **Category**: tech-debt
- **Planned at**: commit `3edb236`, 2026-07-27

## Why this matters

Three separate concerns are currently implemented with one mechanism:

1. **Capability inventory** — which tools exist. Changes with config, skills,
   peripherals. Naturally stable.
2. **Enforcement** — what is permitted right now. Must be live.
3. **Prompt honesty** — what the model is told it can do. Derived, per turn.

`apply_preset_tool_filter` uses (1) to implement (2) and (3): under the Strict
preset it *removes the `shell` tool from the registry*.

Removing a tool from the inventory is destructive and not reversible without
rebuilding the whole registry, which is why the behaviour is broken in both
directions on every build-once surface:

- **Loosening** (Strict → Smart/Off) never restores `shell`. The prompt says
  commands will run; there is no tool to run them. Restart required.
- **Tightening** (Smart → Strict) leaves `shell` registered. On the gateway
  the level still refuses each command, so it is noise rather than a hole —
  but on channels the *system prompt is also boot-pinned*, so the model is
  never even told Strict is in force.

It is also the reason for the hedge added to the Strict prompt block, telling
the model the tool list it was handed may be stale. That hedge is a symptom:
it exists because the registry lies.

The fix is to stop overloading (1). Keep `shell` registered always; let the
gate refuse (it already does — `is_command_allowed` returns `false` immediately
under `ReadOnly`); and derive the tool list *shown to the model* per turn from
the live policy. Then the loosening direction works, the tightening direction
works, and the hedge can be deleted.

**The test of success is deletion**: `apply_preset_tool_filter` and the hedge
both go away. If this change ends with more code than it started, it is the
wrong change.

## Current state

The filter — `src/tools/mod.rs:234-251`:

```rust
pub fn apply_preset_tool_filter(tools: &mut Vec<Box<dyn crate::tools::traits::Tool>>) {
    let strict = matches!(
        crate::profile::ProfileManager::active()
            .ok()
            .and_then(|p| crate::approval::policy_writer::read_active_preset(&p.policy_dir())),
        Some(crate::approval::policy_writer::PolicyPreset::Strict)
    );
    if strict {
        let before = tools.len();
        tools.retain(|t| t.name() != "shell");
```

Note it reads the preset marker from **disk** on every call, independent of the
policy object — a second source of truth for "which preset is active".

Its five call sites, and their cadence:

| Call site | Cadence |
|---|---|
| `src/agent/agent.rs:431` | per `Agent` construction (per request on the console chat path) |
| `src/gateway/mod.rs:518` | per turn (inside the tools factory) |
| `src/channels/mod.rs:3290` | **once at daemon start** |
| `src/agent/loop_.rs:2074` | **once per CLI process** |
| `src/agent/loop_.rs:2587` | in `process_message`, which has **no callers** |

The hedge this plan deletes — `src/agent/prompt.rs`, inside the
`Some(PolicyPreset::Strict)` arm of `SafetySection`:

```rust
                     - The `shell` tool is normally removed from your tool list \
                     in this preset. If it is still listed — the policy can \
                     change mid-session, and the list you were handed may \
                     predate that — do not call it; every command is denied.\n\
```

The prompt already receives the tool list, so it can derive the shown set
itself — `src/agent/prompt.rs:66`:

```rust
    pub tools: &'a [Box<dyn Tool>],
```

and the channel path wraps real registry names into that field —
`src/channels/mod.rs:2323-2327`:

```rust
    let stub_tools: Vec<Box<dyn Tool>> = tools
        .iter()
        .map(|(name, desc)| Box::new(DescriptorTool::new(*name, *desc)) as Box<dyn Tool>)
        .collect();
```

The gate that already does the enforcing — `src/security/policy.rs:669-671`
(**not** `:638`, which is the body of `effective_autonomy`):

```rust
    pub fn is_command_allowed(&self, command: &str) -> bool {
        if self.effective_autonomy() == AutonomyLevel::ReadOnly {
            return false;
```

(After plan 050 this reads `self.fields().autonomy` rather than an override
accessor; either way the behaviour is the same.)

### Ordering

An earlier draft claimed this plan depends on plan 050 "because doing this
first would leave the gate stale". **That was wrong**, and the correction
matters:

- **Channels** — the surface this plan most helps — already hot-reloads the
  gate today. `set_autonomy` is called per inbound message
  (`src/channels/mod.rs:676`, reached from `:1699`), and under `ReadOnly`
  `is_command_allowed` refuses immediately. That reaches the boot-built tool
  registry because the override lives *inside* the shared `Arc`.
- **Gateway** rebuilds its policy per turn; **Agent/TUI** and **CLI** rebuild
  per Agent / per process.

So removing the filter opens no hole on any surface, with or without 050.
Either order is safe. Landing this first is arguably better: it is smaller,
and it removes a reader of the preset marker that 050 would otherwise have to
reason about.

Repo conventions to match:

- Prompt sections are built from `PromptContext` and return a `String`; see
  the other arms of `SafetySection` in `src/agent/prompt.rs`.
- Tests live in-file under `#[cfg(test)] mod tests`. Prompt tests to model on:
  `safety_section_strict_states_the_full_read_only_refusal` and
  `safety_section_agent_smart_keeps_yna_prompt_text` in `src/agent/prompt.rs`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no output |
| Lint (same as CI) | `cargo clippy --locked --all-targets -- -D clippy::correctness` | exit 0 |
| Compile incl. tests | `cargo check --all-targets` | exit 0 |
| Unit tests | `cargo test --lib` | exit 0, all pass |
| Focused | `cargo test --lib agent::prompt` / `--lib tools` | all pass |

Note: CI also runs a **strict-delta** clippy gate
(`scripts/ci/rust_strict_delta_gate.sh`) at `-D warnings` — restricted to the
lines your diff touches, with pedantic lints on. The table's
`-D clippy::correctness` will not catch those. Before pushing, re-run clippy
at `-D warnings` and check that no warning points at a line you added.

## Scope

**In scope**:

- `src/tools/mod.rs` — delete `apply_preset_tool_filter`
- `src/agent/prompt.rs` — derive the shown tool list; delete the hedge
- `src/agent/agent.rs`, `src/gateway/mod.rs`, `src/channels/mod.rs`,
  `src/agent/loop_.rs` — remove the filter call sites
- `plans/README.md` — append (the table ends at row `045` on today's tree; in the execution order 046-051 each earlier plan appends its own row, so expect it to end at the row before `051`. Append rather than assuming a fixed last row):

  ```
  | 051 | Stop using tool registration as an enforcement mechanism | P2 | L | HIGH | — | tech-debt | TODO |
  ```

**Out of scope** (do NOT touch):

- `src/security/policy.rs` — the gate is correct and is what this plan relies
  on. Do not add enforcement here.
- The channels boot-pinned **system prompt** (`src/channels/mod.rs:3694`). It
  needs to become per-turn for this plan's benefit to be fully realised on
  channels, but that is a separate change with its own blast radius. Note it in
  your report; do not do it here.
- `src/approval/policy_writer.rs` and the preset marker file. This plan removes
  one *reader* of the marker, not the marker itself.
- Deleting `process_message` in `src/agent/loop_.rs` — it has no callers and
  should go, but as its own cleanup commit, not smuggled in here.

## Git workflow

- Branch: `refactor/registration-is-not-enforcement`
- One commit per step; the codebase must compile between steps.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Characterization tests first

Before changing behaviour, pin what Strict currently guarantees, so the
refactor cannot quietly weaken it. Write these in **`src/tools/mod.rs`**'s
test module — **not** in `src/security/policy.rs`, which is out of scope for
this plan and whose modification the Done criteria forbid. Assert that under
`ReadOnly`:

- `is_command_allowed` returns `false` for a command that *is* on the allowlist
- `can_act()` returns `false`

These must pass before and after every later step. If they ever fail, the
enforcement moved — which is the one thing this plan must not do.

Name them `strict_refuses_an_allowlisted_command` and
`strict_cannot_act`, so the Done criterion below can check for them by name.

Build the policy with `SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly)`
if that builder exists (plan 050 adds it), otherwise
`SecurityPolicy { autonomy: AutonomyLevel::ReadOnly, ..SecurityPolicy::default() }`.
Check which form compiles before writing both tests — 050 privatises that
field, so the struct-literal form stops working once it lands.

**Verify**: `cargo test --lib tools::tests` → all pass, including the two new
tests. Do **not** use `cargo test --lib security::policy`: these tests live in
`src/tools/mod.rs`, so that filter matches nothing and passes green whether or
not you wrote them.

### Step 2: Make the prompt derive the shown tool set

In `src/agent/prompt.rs`, in the `Strict` arm of `SafetySection`, replace the
hedge with text derived from `ctx.tools`. **Both branches need new wording** —
there is no existing "not registered" sentence to fall back on (`grep -n
"registered" src/agent/prompt.rs` returns only a code comment at `:348`).
Use these two literals so the tests below can assert on them:

- **`ctx.tools` contains `shell`** — emit a line containing the exact phrase
  `shell is listed but every command is refused`.
- **`ctx.tools` does not contain `shell`** — emit a line containing the exact
  phrase `the shell tool is not available in this session`.

Keep the rest of the Strict block (the broader refusal list) as-is.

**This breaks one existing test, and fixing it is part of this step.**
`safety_section_strict_states_the_full_read_only_refusal`
(`src/agent/prompt.rs:800`) builds a Strict context with an **empty** tool list
and asserts the hedge is present:

```rust
        assert!(
            out.contains("still listed"),
            "must cover the stale-registration case: {out}"
```

With empty tools the new code takes the shell-absent branch, so `still listed`
is gone. Update that assertion to the shell-absent phrase above, and drop the
`!out.contains("it is not in your tool list")` assertion at `:831` — it was
guarding against wording this step removes. Keep the rest of that test
(`Writing files`, `refused by policy`) unchanged. **Do not treat this as a
STOP** — it is a rename you were told to make, in a file already in scope.

**Verify**: `cargo test --lib agent::prompt` → all pass, including a new test
per the Test plan.

### Step 3: Remove the filter call sites

Delete the `apply_preset_tool_filter(...)` call at each of the five sites
listed in "Current state". After this, `shell` is present in every registry
and the gate is the only thing that refuses it.

Take them one file at a time and run `cargo check --all-targets` after each,
so a mistake is attributable.

**Verify**: `cargo check --all-targets` → exit 0 after each file. Then
`grep -rn "apply_preset_tool_filter" src/ | wc -l` returns `2` — the
definition at `src/tools/mod.rs:234` plus one prose mention in a comment at
`src/agent/agent.rs:429`. (Before this step: `7`.)

Use `grep -rn … | wc -l`, never `grep -c … src/` — `grep -c` on a *directory*
prints `0` and exits 2, so it "passes" no matter what the code says.

### Step 4: Delete the filter

Remove `apply_preset_tool_filter` from `src/tools/mod.rs`, plus any test that
exercises it directly, plus the now-stale comment at `src/agent/agent.rs:429`
that names it.

**Verify**: `grep -rn "apply_preset_tool_filter" src/ | wc -l` returns `0`
(before this plan: `7`); `cargo check --all-targets` → exit 0.

### Step 5: Full verification

- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --locked --all-targets -- -D clippy::correctness` → exit 0
- `cargo test --lib` → exit 0
- The Step 1 characterization tests still pass.

## Test plan

1. `strict_prompt_acknowledges_a_registered_shell` (in `src/agent/prompt.rs`) —
   build a `PromptContext` at `PolicyPreset::Strict` whose `tools` include a
   `DescriptorTool` named `shell`; assert the rendered text contains the exact
   phrase `shell is listed but every command is refused`.
2. `strict_prompt_reports_shell_absent_when_it_is` — same but with no `shell`
   in `tools`; assert the exact phrase
   `the shell tool is not available in this session`.
3. Keep and re-run the Step 1 characterization tests as the safety net.

**Deliberately NOT written**: a "shell is registered under Strict" test. The
filter is applied at the *call sites*, never inside the registry builders, so
such a test passes today as well — `default_tools_names` in
`src/tools/mod.rs:773` already asserts `shell` is present and is green right
now regardless of the preset. A test that is green before and after proves
nothing. The behaviour change is covered instead by the call sites being gone
(Done criteria) and by tests 1–2 on the prompt.

**Mutation check (required)**: for test 1, revert Step 2 so the Strict arm
emits the old hedge unconditionally; test 1 must fail.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --locked --all-targets -- -D clippy::correctness` exits 0
- [ ] `cargo test --lib` exits 0
Each command below was **run against the current tree** and returns the
"before" value shown, so each is genuinely falsifiable.

- [ ] `grep -rn "apply_preset_tool_filter" src/ | wc -l` returns `0` (before: `7`)
- [ ] `grep -rn "normally removed from your tool list" src/agent/prompt.rs | wc -l`
      returns `0` (before: `1`) — the hedge is gone.
      Do **not** grep for `"may predate that"`: the source is a Rust
      line-continuation string, so that phrase never appears on one line and
      the check would pass today.
- [ ] `grep -rn "fn strict_refuses_an_allowlisted_command\|fn strict_cannot_act" src/tools/mod.rs | wc -l` returns `2` (before: `0`), and `cargo test --lib tools::tests` passes
- [ ] Tests 1–2 exist and pass; the mutation check was performed
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- A prompt test **other than**
  `safety_section_strict_states_the_full_read_only_refusal` fails after Step 2.
  That one is expected and Step 2 tells you how to update it.
- A Step 1 characterization test fails at any point. Enforcement has moved;
  that is the one outcome this plan forbids.
- Removing the filter makes a tool reachable that the gate does **not** refuse
  under `ReadOnly`. That would mean the gate is not the complete backstop this
  plan assumes — report immediately, do not proceed.
- You discover a sixth call site of `apply_preset_tool_filter` not listed
  above.
- The channels system prompt turns out to need changing for the Strict text to
  be correct there. It is out of scope; report and stop rather than widening.

## Maintenance notes

- The rule this establishes: **the registry is an inventory, the policy is the
  gate, the prompt is derived.** A future change that removes a tool to enforce
  something is reintroducing this bug — challenge it in review.
- Channels will still show a stale Strict/Smart *narration* until its system
  prompt is built per turn. That is the last remaining piece of this family and
  should be its own plan.
- `apply_preset_tool_filter` was the only reader of the preset marker on the
  registry path. After this, the marker is read by the prompt and by
  `rantaiclaw autonomy` for display. If the marker ever becomes purely a
  display artifact, the two-sources-of-truth problem noted during the audit
  disappears with it.
