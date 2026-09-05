# Plan 214: Decide and implement whether the shell tool honors `forbidden_paths` for read commands

> **Executor instructions**: This plan begins with a DECISION. Read "The
> decision" first, choose an option, record it, and then implement only that
> option. Run every verification command. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tools/shell.rs src/security/policy.rs`

## Status

- **Priority**: P1 (security — `forbidden_paths` is illusory for the shell surface)
- **Effort**: M
- **Risk**: MED
- **Depends on**: 198 (the `forbidden_paths` floor should exist first)
- **Category**: security / decision
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`forbidden_paths` enumerates credential/system dirs (`~/.ssh`, `~/.aws`,
`~/.gnupg`, `/etc`, …) to keep tools out of them. But the **shell tool never
consults it** — `shell.rs` calls only `validate_command_execution` +
`record_action`; it has zero `is_path_allowed`/`forbidden` references. The path
guards live only in the file-oriented tools.

The default allowlist includes `cat`, `grep`, `head`, `tail`, `find`, `wc`. So
in default Supervised mode, `cat ~/.aws/credentials`, `grep -r . ~/.ssh`,
`cat /etc/shadow` all pass: the base is allowlisted, args are unrestricted, and
the risk class is `Low` → no approval. The protection `forbidden_paths`
advertises is illusory for the shell surface — the single most capable tool.

This is genuinely a product decision (how far to scope shell reads), not a
mechanical bug, so this plan is framed as a decision with two implementations.

## Current state

### Shell has no path guard — `src/tools/shell.rs`

Grep for `is_path_allowed`/`is_resolved_path_allowed`/`forbidden` in `shell.rs`
returns **0**. The gate is `validate_command_execution(command, approved)` +
`record_action()` only.

### The path guard exists but is file-tool-only — `src/security/policy.rs:881-936`

`is_path_allowed` (lexical, `policy.rs:881-936`) and `is_resolved_path_allowed`
(post-canonical, `policy.rs:940+`) are called from
`file_read`/`file_write`/`pdf_read`/`image_info`, not from shell.

## The decision

### Option A (preferred) — scan allowlisted read commands for forbidden path args

For a bounded set of well-known **read** commands (`cat`, `grep`, `head`,
`tail`, `wc`, `less`, `more`, `od`, `xxd`, `strings`, `find`), extract the
path-like arguments and reject the command if any resolves under
`forbidden_paths` (which, per plan 198, includes the hardcoded floor). This
closes the credential-exfil path without trying to parse every possible command.

- Extract path args heuristically: non-flag tokens that look like paths
  (contain `/` or start with `~`/`.`), after de-quoting (reuse plan 196's
  `shell_argv`). Resolve `~`/relative against the workspace as the file tools do.
- If a forbidden path is found, block with a clear error (do **not** silently
  strip it).
- Accept that this is best-effort for the shell (a determined command can still
  read via a non-listed reader or indirection) — the goal is to stop the obvious,
  high-frequency `cat ~/.aws/credentials` class, and to stop advertising
  protection the shell entirely lacks.

### Option B (honest, smaller) — document the boundary and drop the false framing

If Option A's heuristic path-extraction is judged too risky/imprecise:

- Explicitly document that `forbidden_paths` is a **file-tool** control and does
  NOT constrain the shell tool, in the config schema doc, `docs/security/*`, and
  anywhere the framing implies otherwise.
- Recommend operators who need shell path confinement use a lower autonomy level
  or (once it exists) the real OS sandbox (plan 215).
- This removes the *lie* without adding shell path-scanning.

Prefer A (it actually closes the exfil path). Fall back to B only with an
explicit note that shell reads remain unconfined by `forbidden_paths`.

## Files

- **In scope (A)**: `src/tools/shell.rs` (the path check for read commands),
  possibly a helper in `src/security/policy.rs`.
- **In scope (B)**: `src/config/schema.rs` doc, `docs/security/*`, any code
  comment implying shell honors forbidden_paths.
- **Out of scope**: the file tools (already correct), the OS sandbox (plan 215),
  the API floor (plan 198 — prerequisite).

## STOP conditions

- DECIDE A vs B and record it before writing code.
- Option A: if the path-extraction heuristic starts blocking legitimate commands
  (e.g. `grep pattern file` where `file` is fine), tune conservatively — err
  toward blocking only clearly-forbidden targets, and STOP to reconsider scope
  if the false-positive rate is high. Do not degrade into a full shell parser.
- If plan 198's floor is not yet in place, land it first (Option A relies on the
  floor being enforced in `is_path_allowed`).

## Done criteria

**Option A:**
1. `cargo fmt`/`clippy`/`cargo test -p rantaiclaw --lib tools::shell security::policy` clean.
2. Tests: `cat <HOME>/.aws/credentials`, `grep -r . <HOME>/.ssh`, `cat /etc/shadow`
   are blocked by the shell tool (not merely the allowlist); a legitimate
   in-workspace `cat ./README.md` still runs.

**Option B:**
1. The schema/doc changes land; a test or CI doc-check asserts no doc claims the
   shell honors `forbidden_paths`. The PR states shell reads are unconfined by
   `forbidden_paths` and points to autonomy level / the sandbox as the control.

## Test plan

Option A: mirror the shell tool tests (temp workspace); add the forbidden-read
cases + the legitimate-read no-regression case. Option B: assert the corrected
wording; no behavioral test.

## Risk & rollback

- **Risk**: MED (A) — heuristic path scanning can over- or under-block; keep it
  conservative and read-command-scoped. LOW (B) — docs only, but leaves the
  shell unconfined (honestly stated).
- **Rollback**: revert `shell.rs` (A) or the docs (B).

## Maintenance note

The real fix for shell confinement is the OS sandbox (plan 215); this plan is
the interim, in-process guard (A) or the honest disclaimer (B). If the sandbox
lands and is wired to shell, revisit whether A's heuristic is still needed.
