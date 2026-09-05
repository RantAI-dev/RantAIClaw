# Plan 261: Add CI gates for docs-vs-code and unread config keys

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done. Land these gates ADVISORY-only first (don't fail the build until the backlog is cleared by plans 257/260).
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- .github/workflows/ci-run.yml scripts/ci/ tests/schema_drift.rs src/config/schema.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P3
- **Effort**: M (a first useful slice; full coverage is L)
- **Risk**: LOW (new advisory CI checks)
- **Depends on**: plans 257 + 260 (clear the current backlog first, or land these advisory-only)
- **Category**: dx
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

- **K1** Nothing verifies a documented default/flag/path against code, and the docs-quality CI job runs only on `docs_changed` — so a CODE change that invalidates a doc claim triggers no gate. Every J-finding in plan 260 is that shape; without a gate the count regrows every release.
- **DIR-01** The unread-config-key gate exists (`check_channel_config_readers.sh`, fail-closed, CI-wired) but only covers 15 channel structs; `agent.parallel_tools` and the other dead keys sit in its blind spot. Generalizing it to the whole schema is the cheapest guarantee the config surface stays honest.

## Current state (confirm before editing)

- `tests/schema_drift.rs:31-44` — fingerprints `schema_for!(Config)` (shape, not doc claims).
- `scripts/ci/check_channel_config_readers.sh` — fail-closed (`:31-32`), CI-wired (`.github/workflows/ci-run.yml:86`), empty `KNOWN_UNREAD` (`:36-38`); `STRUCTS` (`:44-46`) lists 15 channel structs only; ambiguous-field-name handling at `:48-54`. Its header (`:9-21`) argues the general case.
- `scripts/ci/docs_quality_gate.sh:65-68,117` — markdownlint + changed-line link check; zero semantic verification. `.github/workflows/ci-run.yml:270-285` runs it only when `docs_changed=='true'`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Run the new doc-vs-help check | `bash scripts/ci/docs_command_coverage.sh` (new) | lists any command missing from commands.md |
| Run the generalized key gate | `bash scripts/ci/check_config_readers.sh` (new/renamed) | lists unread keys |
| YAML sanity | `yamllint .github/workflows/ci-run.yml` (if available) | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: `scripts/ci/` (a new/extended check), `.github/workflows/ci-run.yml` (trigger on `rust_changed`, not just `docs_changed`; wire the new checks advisory-first), optionally `tests/` for a Rust-side doc-table check.
**Out of scope**: fixing the backlog (plans 257/260); the schema-drift snapshot (plan 253).

## Git workflow

- Branch: `ci/docs-and-config-key-gates`
- Message e.g. `ci(config): gate docs command coverage and generalize the unread-config-key check`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (K1): a check that every CLI command appears in `commands.md`

Add `scripts/ci/docs_command_coverage.sh` (or a Rust test) that renders `rantaiclaw --help` plus each subcommand's help and asserts every command name appears in `docs/reference/commands.md`. Catches J5/J6/J8-class drift. Wire it into CI on `rust_changed` OR `docs_changed`. Advisory (non-blocking) until plan 260 clears the backlog, then flip to blocking.

**Verify**: run it locally → it lists the commands plan 260 will add (before 260) / passes (after 260).

### Step 2 (K1, optional deeper): a config.md default-table check

Add a check that parses the `| key | default | … |` tables in `config.md` and resolves each documented default against `schema_for!(Config)`'s `default` annotations. Catches J7 and the whole class. This is the L-effort half — implement if time allows, else leave a stub + note.

**Verify**: run it → flags J7 (before plan 260) / passes (after).

### Step 3 (DIR-01): generalize the unread-key gate

Replace the hardcoded `STRUCTS` list in `check_channel_config_readers.sh` (`:44-46`) with all `*Config` structs in `schema.rs` (rename the script to `check_config_readers.sh`). Run it once and publish the hit list as the backlog into `KNOWN_UNREAD` with a note referencing plan 257. Expect the ambiguous-field-name path (`:48-54`) to dominate at this scale — if a bare `.field` grep is too noisy, match on the declaring type; if that's a big change, note it as a follow-up and keep the check advisory.

**Verify**: run it → lists the dead keys plan 257 removes (`parallel_tools`, etc.).

### Step 4: wire triggers and keep advisory until backlog clears

In `.github/workflows/ci-run.yml`, add the new checks to the `rust_changed` trigger (not just `docs_changed`). Mark them advisory (continue-on-error / non-required) until plans 257 + 260 land; the PR notes must say when to flip them blocking.

**Verify**: `yamllint` (if available) passes; the workflow parses.

## Test plan

- The checks ARE the tests (shell/Rust). Prove each FLAGS the current backlog before 257/260 land and PASSES after (or would pass against a cleaned tree).
- Verification: run each new script locally against the current tree; confirm the expected hit lists.

## Done criteria

- [ ] the docs-command-coverage check exists and runs in CI (advisory)
- [ ] the unread-key check covers all `*Config` structs (advisory), with the backlog published to `KNOWN_UNREAD`
- [ ] CI triggers include `rust_changed` for the docs checks
- [ ] `yamllint` (if available) passes; workflow parses
- [ ] `git status` shows only CI/script files
- [ ] `plans/README.md` row updated

## STOP conditions

- The generalized key check produces overwhelming false positives from the ambiguous-name path — keep it advisory + publish the backlog; do NOT make it blocking until the matching is reliable (a syn-based AST pass may be the real fix — note it as a spike).
- CI wiring would make an unrelated job fail — keep the new checks non-required until the backlog is clear.

## Maintenance notes

- Reviewer: confirm the checks are ADVISORY on merge (so they don't block on the existing backlog) and that they actually flag the current drift.
- Flip to blocking only after plans 257 + 260 land; record that hand-off in the PR.
- The default-table check (Step 2) is the highest-value long-term guard — if deferred, file it as a follow-up.
