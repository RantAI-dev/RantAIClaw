# Plan 048: Make the approval prompt name the command that is actually blocked

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 3edb236..HEAD -- src/security/policy.rs src/tui/commands/allowlist.rs`
> If either changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, treat it as
> a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `3edb236`, 2026-07-27

## Why this matters

`SecurityPolicy` has a live override for the shell allowlist
(`allowed_commands_runtime`, read via `effective_allowed_commands()`), used so
a config hot-reload can *narrow* the allowlist and not only widen it.

`is_command_allowed` reads through that override correctly.
`first_unallowed_basename` — the function whose entire job is to tell the UI
*which* command to ask the operator about — does not. It reads the boot-time
`allowed_commands` field directly, while using `effective_autonomy()` two
dozen lines above. Half of the function was converted; half was not.

Concrete misbehaviour after a reload narrows the allowlist from
`["git", …]` to `["echo"]`, for the command `git status && brew install x`:

1. `is_command_allowed` rejects at `git` — correct, `git` is no longer allowed.
2. `first_unallowed_basename` **skips** `git` (still on the stale boot list)
   and returns `Some("brew")`.
3. The shell tool asks the operator to approve **`brew`**.
4. They approve. `add_runtime_command("brew")` succeeds — the allowlist is now
   wider for no reason.
5. The retry still fails at `git`. `first_unallowed_basename` now returns
   `None`, so the loop falls into the hard-block branch and the operator is
   left with no path forward.

So the operator is prompted about the wrong command, grants a permission they
never intended, and still gets denied. It fails closed on execution, but it
widens a security boundary as a side effect of a denial, and it is very
confusing to debug.

A second, latent instance of the same bypass exists in the TUI's `/allowlist`
display.

## Current state

Files involved:

- `src/security/policy.rs` — the accessor, the correct reader, and the
  incorrect one.
- `src/tools/shell.rs` — the consumer that turns the returned basename into an
  operator prompt.
- `src/tui/commands/allowlist.rs` — the second, display-only bypass.

The accessor that should be used — `src/security/policy.rs:651-656`:

```rust
    pub fn effective_allowed_commands(&self) -> Vec<String> {
        self.allowed_commands_runtime
            .read()
            .clone()
            .unwrap_or_else(|| self.allowed_commands.clone())
    }
```

The reader that does it right — `src/security/policy.rs:726-734` inside
`is_command_allowed`:

```rust
            // The config list is read through the override so a hot-reloaded
            // allowlist can drop entries, not only add them.
            let on_config_list = {
                let guard = self.allowed_commands_runtime.read();
                match guard.as_ref() {
                    Some(list) => list.iter().any(|a| a == base_cmd),
                    None => self.allowed_commands.iter().any(|a| a == base_cmd),
                }
            };
```

The reader that does it wrong — same file, `:1048-1049` uses the live autonomy
accessor:

```rust
    pub fn first_unallowed_basename(&self, command: &str) -> Option<String> {
        let autonomy = self.effective_autonomy();
```

…but `:1076` bypasses the allowlist override:

```rust
            let on_boot = self.allowed_commands.iter().any(|a| a == base_cmd);
            let on_runtime = !on_boot && {
                let set = self.runtime_allowlist.read();
                set.contains(base_cmd)
            };
            if !on_boot && !on_runtime {
                return Some(base_cmd.to_string());
            }
```

The consumer that turns this into an operator prompt —
`src/tools/shell.rs:325-346`:

```rust
                    let (Some(approvals), Some(basename)) = (
                        self.security.pending(),
                        self.security.first_unallowed_basename(command),
                    ) else {
```
```rust
                    let decision = approvals
                        .request_decision(basename.clone(), command.to_string(), "")
                        .await;
                    match decision {
                        Decision::Once | Decision::Session => {
                            if let Err(e) = self.security.add_runtime_command(&basename, false) {
```

The second bypass, display-only — `src/tui/commands/allowlist.rs:170`:

```rust
        let boot = &security.allowed_commands;
```

This is rendered to the operator as the "Boot allowlist" in `/allowlist`. It
is latent today because nothing on the TUI path writes the override — the only
*production* callers of `set_allowed_commands` repo-wide are in
`src/channels/mod.rs` (three more, at `src/security/policy.rs:1171`, `:1179`,
`:1190`, are tests). But
it is an operator-facing security-inspection surface showing a list that can
be wrong, and it is the same mistake.

Repo conventions to match:

- Accessors are named `effective_*`; the raw fields stay `pub` for
  construction and display.
- Tests live in-file under `#[cfg(test)] mod tests`. Model new ones on
  `set_allowed_commands_narrows_across_clones` in `src/security/policy.rs`.

### Ordering

**Land this plan BEFORE `plans/050-policy-refresh-carries-process-state.md`.**
050 deletes `effective_allowed_commands()` — the accessor this plan's Step 1
starts calling — and privatises `allowed_commands`, which this plan's Step 2
reads. If 050 lands first, this plan's "Current state" excerpts no longer
match the tree and its drift protocol will (correctly) STOP you.

If you are reading this *after* 050 has landed, do not improvise: report that
the ordering was inverted and stop.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no output |
| Lint (same as CI) | `cargo clippy --locked --all-targets -- -D clippy::correctness` | exit 0 |
| Unit tests | `cargo test --lib` | exit 0, all pass |
| Focused tests | `cargo test --lib security::policy` | all pass |

Note: CI also runs a **strict-delta** clippy gate
(`scripts/ci/rust_strict_delta_gate.sh`) at `-D warnings` — restricted to the
lines your diff touches, with pedantic lints on. The table's
`-D clippy::correctness` will not catch those. Before pushing, re-run clippy
at `-D warnings` and check that no warning points at a line you added.

Note: some `skills::tests::toml_*` tests are non-hermetic against `$HOME` on
some machines. If they fail, confirm they also fail on an unmodified checkout
before treating it as your regression.

## Scope

**In scope**:

- `src/security/policy.rs`
- `src/tui/commands/allowlist.rs` — including its `#[cfg(test)] mod tests`;
  Step 2 deliberately requires updating one assertion there.
- `plans/README.md` — append the status row for this plan (the table currently
  ends at row `045` today, and at `047` once plans 046-047 have run — append to
  whatever the last row is, do not assume `045`). Append exactly:

  ```
  | 048 | Make the approval prompt name the command that is actually blocked | P2 | S | LOW | — | bug | TODO |
  ```

**Out of scope** (do NOT touch):

- `src/tools/shell.rs` — the consumer is correct as written; it just receives
  a better answer once the policy is fixed.
- `src/channels/mod.rs` — the reload path is correct.
- Making `allowed_commands` a private field. Tempting (the compiler would then
  enforce accessor use) but it is `pub` and read at several construction and
  display sites; that is a separate refactor with a wider blast radius.
- The `runtime_allowlist` layer — `/allow --persist` grants are deliberately a
  separate layer and must keep working exactly as they do.

## Git workflow

- Branch: `fix/first-unallowed-basename-effective-allowlist`
- Conventional commit titles. Example from this repo's history:
  `fix(channels): let a config reload narrow the shell allowlist`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Read the config allowlist through the override

In `src/security/policy.rs`, in `first_unallowed_basename`, replace the direct
field read at `:1076` so it consults the override, matching what
`is_command_allowed` already does.

Hoist the effective list **once before the loop** rather than calling the
accessor per segment — `effective_allowed_commands()` clones a `Vec`, and this
function iterates over command segments:

```rust
        let config_allowed = self.effective_allowed_commands();
```

then inside the loop:

```rust
            let on_config_list = config_allowed.iter().any(|a| a == base_cmd);
            let on_runtime = !on_config_list && {
                let set = self.runtime_allowlist.read();
                set.contains(base_cmd)
            };
            if !on_config_list && !on_runtime {
                return Some(base_cmd.to_string());
            }
```

Rename the local from `on_boot` to `on_config_list` so the name stops claiming
it is the boot list. Keep the rest of the function unchanged.

**Verify**: `cargo check --all-targets` → exit 0. (`--all-targets` compiles the
test modules too; `build --lib` would report success while a test module is
broken.)

### Step 2: Make `/allowlist` show the list actually in force

In `src/tui/commands/allowlist.rs` at `:170`, replace the direct field read
with `security.effective_allowed_commands()`.

The returned value is an owned `Vec<String>`, not a reference, so drop the `&`:
`let config_allowed = security.effective_allowed_commands();`. Rename the local
from `boot` to `config_allowed` for the same reason Step 1 renames its own —
`boot` would now be a lie. The local is used at `:178`, `:179`, and `:182` (`:181` is the `} else {`);
update all of them.

Change the label literal at `src/tui/commands/allowlist.rs:177` from:

```rust
            "Boot allowlist ({}): {}\n",
```

to exactly:

```rust
            "Config allowlist ({}): {}\n",
```

**This breaks one existing test, and fixing it is part of this step.**
`allowlist_shows_boot_and_runtime` (fn at `src/tui/commands/allowlist.rs:270`)
asserts the old string at `:281`:

```rust
                assert!(m.contains("Boot allowlist"));
```

Update that assertion to `"Config allowlist"`. Do **not** treat this as a STOP
condition — it is a rename you were told to make, in a file already in scope.
Leave the other assertions in that test (`"Runtime allowlist"`, `"rg"`,
`"Pending approvals"`) untouched.

**Verify**: `cargo check --all-targets` → exit 0.

### Step 3: Add the regression tests

See "Test plan". Write them before running the full suite.

**Verify**: `cargo test --lib security::policy` → all pass.

### Step 4: Full verification

**Verify**, all three:

- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --locked --all-targets -- -D clippy::correctness` → exit 0
- `cargo test --lib` → exit 0

## Test plan

Add to the `#[cfg(test)] mod tests` block in `src/security/policy.rs`,
modelled on `set_allowed_commands_narrows_across_clones`:

**Building the test policy**: use the module's existing `default_policy()`
helper (`src/security/policy.rs:1093`) as-is. It is Supervised — required,
because `first_unallowed_basename` returns `None` early for both `ReadOnly`
and `Full` (`:1050-1052`), so a policy at either level would pass vacuously.
Its default allowlist already contains both `git` and `echo` (`:140-153`), so
there is no need to hand-build one.

1. `first_unallowed_basename_names_a_command_dropped_by_a_reload` — using
   `default_policy()` (allowlist already contains `git` and `echo`). Assert
   `first_unallowed_basename("git status")` is `None` (allowed). Then call
   `set_allowed_commands(vec!["echo".to_string()])` and assert
   `first_unallowed_basename("git status")` now returns `Some("git")`. On the
   pre-fix code this returns `None`, because `git` is still on the stale boot
   list.

2. `first_unallowed_basename_matches_is_command_allowed_after_narrowing` —
   the invariant that actually matters. For the chained command
   `git status && brew install x`, after narrowing to `["echo"]`, assert
   `is_command_allowed(cmd)` is `false` **and** `first_unallowed_basename(cmd)`
   is `Some("git")` — i.e. the prompt names the same command the gate
   rejected. On the pre-fix code this returns `Some("brew")`, the wrong one.

3. `first_unallowed_basename_still_honours_runtime_grants` — after narrowing
   the config list, `add_runtime_command("git", false)`, then assert
   `first_unallowed_basename("git status")` is `None`. This pins that
   `/allow`-style grants remain a separate, still-working layer.

**Verification**: `cargo test --lib` → all pass, including the 3 new tests.

**Mutation check (required before you call this done)**: temporarily restore
the direct `self.allowed_commands` read in `first_unallowed_basename` and
confirm tests 1 and 2 fail. Restore the fix. If they still pass, the tests are
not covering the change — STOP and report.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --locked --all-targets -- -D clippy::correctness` exits 0
- [ ] `cargo test --lib` exits 0
- [ ] The bypass is gone from `first_unallowed_basename` specifically — scope
      the check to that function rather than the whole file:
      `awk '/pub fn first_unallowed_basename/,/^    }$/' src/security/policy.rs | grep -c 'self\.allowed_commands'`
      returns `0`.
      (Do **not** grep the whole file: legitimate matches remain at `:655`
      inside `effective_allowed_commands` and at `:732` inside
      `is_command_allowed`. Both are correct and must stay.)
- [ ] `grep -c "security.allowed_commands" src/tui/commands/allowlist.rs`
      returns `0`
- [ ] `grep -c "Boot allowlist" src/tui/commands/allowlist.rs` returns `0`
      (label and its assertion both updated)
- [ ] The three new tests exist and pass; the mutation check was performed and
      tests 1 and 2 failed under mutation
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The excerpts in "Current state" do not match the live code (drift). Line
  numbers drifting by a line or two while the quoted text matches is **not** a
  STOP — only a content mismatch is.
- Changing the `/allowlist` display requires touching files beyond
  `src/tui/commands/allowlist.rs` — e.g. a shared formatting helper that other
  commands depend on. Report instead of widening the change.
- A test **other than** `allowlist_shows_boot_and_runtime` (`:270`) asserts on
  the boot list and fails. That one is expected and Step 2 tells you to update
  it; any *other* such test would mean something deliberately depends on
  seeing the pre-reload list — report rather than changing it.
- A verification command fails twice after a reasonable fix attempt.

## Maintenance notes

- This is the second bug of exactly this shape (a gate reading a raw field
  while a live override exists). If a third override slot is ever added,
  grep for every direct read of the field it shadows **before** merging — the
  compiler cannot catch it while the fields stay `pub`.
- The durable fix is to stop having per-field override slots at all; that is
  tracked as separate, larger work. Until then, treat every
  `effective_*` accessor as having an invariant: *no gate may read the field
  it shadows*.
- Reviewers should check the `/allowlist` label change — showing the effective
  list under a label that says "Boot" would be worse than the current state,
  because the operator would trust a wrong name.
