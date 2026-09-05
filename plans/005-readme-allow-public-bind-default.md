# Plan 005: Fix README example config that teaches disabling the public-bind guard

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- README.md`
> If `README.md` changed since this plan was written, re-locate the `[gateway]`
> example before editing; on a structural mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: docs
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The most-copied config snippet in the project — the quick-start `[gateway]`
block in the README — sets `allow_public_bind = true`. The schema default is
`false`, the config reference documents `false` as "block accidental public
exposure", and there is a test asserting the missing-`[gateway]` default is
`false`. The project's stated posture is deny-by-default at the exposure
boundary. So the headline example instructs new users to disable the
accidental-public-exposure guard on the gateway: copy-paste onboarding produces
a publicly-bindable gateway. A stale doc that is actively wrong is worse than a
missing one — this is a one-line fix with real security payoff.

## Current state

- `README.md:283-322` — the Configuration example. The offending line is 321:
  ```toml
  # Gateway
  [gateway]
  enabled = true
  port = 8080
  allow_public_bind = true      # <-- line 321: contradicts deny-by-default
  ```

- The correct default, for reference (do NOT edit these):
  - `src/config/schema.rs:1021` — `allow_public_bind: false` (default impl).
  - `src/config/schema.rs:5671-5672` — test: "Missing `[gateway]` must default
    to `allow_public_bind=false`".
  - `docs/reference/config.md:287` — documents `false` = block accidental public
    exposure.
  - The intentional-LAN-exposure workflow already exists at
    `docs/operations/network-deployment.md` (the auditor cited lines 65-115).
    Confirm the path exists: `ls docs/operations/network-deployment.md`.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Confirm the line | `grep -n "allow_public_bind" README.md` | shows the line to fix |
| Markdown lint (if configured) | check `dev/ci.sh`/repo for the markdown lint command; run it if present | exit 0 |

This is docs-only; no Rust build is required.

## Scope

**In scope**:
- `README.md` — the single `allow_public_bind` line in the config example (and
  an optional one-line clarifying comment).

**Out of scope** (do NOT touch):
- `src/config/schema.rs` — the default is already correct.
- `docs/reference/config.md`, `docs/operations/network-deployment.md` — already
  correct; do not restructure them.
- Any other part of the README.

## Git workflow

- Branch: `advisor/005-readme-allow-public-bind-default`
- One commit; message e.g.
  `docs(readme): default gateway example to allow_public_bind=false (deny-by-default)`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Flip the example to the safe default with a pointer comment

Change README line 321 from `allow_public_bind = true` to:
```toml
allow_public_bind = false   # localhost-only by default; see docs/operations/network-deployment.md to expose on a LAN
```
Keep the surrounding `[gateway]` block (`enabled`, `port`) unchanged.

**Verify**: `grep -n "allow_public_bind = false" README.md` → matches;
`grep -n "allow_public_bind = true" README.md` → no matches.

### Step 2: Confirm the referenced doc exists

**Verify**: `ls docs/operations/network-deployment.md` → the file exists. If it
does not, drop the path from the comment and instead reference
`docs/reference/config.md` (which is confirmed present) — do not point at a
non-existent file.

## Test plan

- No code tests. If the repo has a markdown link-check in CI
  (`.github/workflows/ci-run.yml` has a docs-quality job), ensure the comment
  does not introduce a broken relative link — verify the referenced path exists
  (Step 2).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `grep -n "allow_public_bind = true" README.md` returns no matches
- [ ] `grep -n "allow_public_bind = false" README.md` matches the gateway example
- [ ] The referenced doc path in the comment exists on disk (`ls` succeeds)
- [ ] Only `README.md` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The README no longer contains a `[gateway]` example block (drift).
- The schema default has meanwhile changed to `true` (check
  `grep -n "allow_public_bind" src/config/schema.rs`) — that would be a real
  policy change to investigate, not a doc fix; report it.

## Maintenance notes

- Any future doc/example that shows `[gateway]` must keep `allow_public_bind`
  at `false` unless the surrounding prose is explicitly about intentional LAN
  exposure (and then it should link the network-deployment doc).
- Reviewer should grep the whole docs tree for other `allow_public_bind = true`
  examples: `grep -rn "allow_public_bind = true" docs/ README.md` — fix any
  others found (in scope for this plan if trivial; otherwise note them).
