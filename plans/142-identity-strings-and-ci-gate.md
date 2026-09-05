# Plan 142: Remove identity strings and add a CI gate for §9.1

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/onboard/wizard.rs src/channels/mod.rs .github/workflows/`
>
> **Line numbers WILL have drifted** — plan 133 merges before this one. Relocate by
> symbol name and continue. STOP only if the *code itself* no longer matches the
> "Current state" excerpt semantically.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: plans/133 (serialized over `src/onboard/wizard.rs`)
- **Category**: chore
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

CLAUDE.md §9.1 makes personal identity data in code, docs, tests, fixtures or commits
a **merge gate**, and names the substitutes to use. The repo violates its own gate in
the most visible place available: a contributor's real first name is in the onboarding
prompt every operator reads.

There is a second instance in a code comment, where a quoted bug report carries a real
handle.

Neither is dangerous. Both are the kind of thing that is trivial to fix once and
impossible to keep fixed without a check — which is why the substantive half of this
plan is the CI gate, not the removal.

## Current state

`src/onboard/wizard.rs:3609` — in the Telegram allowlist prompt, read by every
operator who runs setup:

```rust
                    "Use your @username without '@' (example: <name>), or your numeric Telegram user ID.",
```

`src/onboard/wizard.rs:6152`, `:6176`, `:6188` — the same first name as a fixture value
and in two assertions.

`src/channels/mod.rs:1662` — a comment quoting a v0.6.6 tester report that includes a
real handle. Lower stakes (it is a code comment, and the handle is the repo owner's),
but it is the same class.

CLAUDE.md §9.1 names the palette: `rantaiclaw_user`, `user_a`, `test_user`,
`RantaiClawAgent`, `RantaiClawOperator`, `rantaiclaw_bot`, `example.com`.

Neighbouring channel modules already comply — `src/channels/nextcloud_talk.rs:391`
and `:404` use `user_a`.

There is no CI check for any of this today.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Onboard tests | `cargo test --lib onboard::` | all pass |
| The new gate | `bash scripts/ci/check_identity_strings.sh` (name it as you like) | exit 0 |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/onboard/wizard.rs` (the four sites), `src/channels/mod.rs` (the
comment), a new CI check script, and the workflow entry that runs it.

**Out of scope**: rewriting the §9.1 policy itself; auditing the whole repo for
identity data beyond the pattern list you build — the gate will surface the rest, and
fixing an unbounded set inside this plan makes it unreviewable. Anything else in
`wizard.rs` (plans 132/133 own it and are merged).

## Git workflow

- Branch: `chore/identity-strings-and-ci-gate`
- Conventional commits, e.g. `chore(onboard): replace identity strings with project-scoped placeholders`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Replace the four sites

- `wizard.rs:3609` — use `rantaiclaw_user` in the prompt example.
- `wizard.rs:6152`, `:6176`, `:6188` — use `user_a` or `RantaiClawOperator`, matching
  what the assertions need.
- `src/channels/mod.rs:1662` — paraphrase the tester report without the handle. The
  comment's value is the *symptom* it describes, not who reported it; keep the
  symptom.

**Verify**: `cargo test --lib onboard::` → all pass.

### Step 2: Write the gate

A grep-based script over tracked files, failing on a small, explicit pattern list.

**Build the list from what §9.1 actually prohibits**, not from a general PII regex:
the names currently in the tree, obvious real-email shapes that are not `example.com`
or `.test`, and phone numbers outside the reserved ranges the repo already uses.

Two properties matter more than coverage:

- **It must not fire on the approved palette.** A gate that flags `user_a` or
  `rantaiclaw_user` will be disabled within a week.
- **It must be greppable and short.** Someone will need to read it when it fires on
  their PR at 2am; a clever regex that nobody can debug gets deleted, not fixed.

Scope it to tracked files only, and exclude `plans/`, `docs/project/archive/` and any
vendored path — historical snapshots are immutable by policy and re-editing them is
out of scope.

**Verify**: the script exits 0 on the current tree after step 1, and non-zero when you
temporarily reintroduce one of the removed names.

### Step 3: Wire it into CI

Add it as a fast job in the existing workflow, alongside the other cheap checks. It
should run on every PR — a gate that only runs post-merge cannot block the thing it
exists to block.

Note that this repo has push-only jobs, so verify **where** you added it actually runs
on PRs rather than assuming.

**Verify**: read the workflow file and confirm the job's trigger includes pull
requests.

### Step 4: Prove the gate works

Reintroduce one removed name in a scratch commit, confirm the check fails, and revert.
Record the output in the PR.

A gate nobody has seen fail is not known to work.

## Test plan

This plan's deliverable is the gate, so the test is the gate itself:

1. The script exits 0 on the cleaned tree.
2. The script exits non-zero for each pattern class it claims to cover — demonstrate at
   least one per class in the PR.
3. The script exits 0 on a file containing every value from the §9.1 approved palette.
   **This is the test that keeps the gate alive**; a gate with false positives gets
   removed.

No Rust tests are needed. Say so in the PR rather than adding a decorative one.

**Verify**: `cargo test --lib onboard::` → all pass, and the script behaves as above.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib onboard::` passes
- [ ] `grep -rin '<the removed first name>' src/ docs/` returns nothing
- [ ] The check script exits 0 on the tree and non-zero on a reintroduced name
- [ ] The script exits 0 against a file containing the full approved palette
- [ ] The CI job runs on pull requests, not only on push
- [ ] Step 4's demonstration output is in the PR body
- [ ] `plans/README.md` status row for 142 updated

## STOP conditions

Stop and report back if:

- Plan 133 has not merged — this is serialized over `src/onboard/wizard.rs`.
- The pattern list produces false positives you cannot eliminate without making it
  useless. Ship the four replacements, report the gate as not-yet-viable, and say what
  blocked it. A noisy gate is worse than none.
- The gate finds identity data in files beyond the four known sites. **Report the list
  first.** Some may be in immutable historical snapshots, where the policy is to leave
  them; that is a judgment call for the maintainer, not a fix to make in passing.
- You cannot find a workflow job that runs on pull requests.

## Maintenance notes

- **What interacts with this**: plan 143 adds other CI checks; if both land, they
  should sit in the same fast-checks job rather than each adding their own.
- **What a reviewer should scrutinise**: the false-positive test in step 3. Every gate
  of this kind that gets deleted is deleted because it cried wolf, not because it was
  wrong about the thing it caught.
- **Deliberately deferred**: a full-repo identity audit. The gate makes the rest
  discoverable incrementally, which is the maintainable order.
