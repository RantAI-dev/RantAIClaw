# Plan 206: Stop the preset from collapsing command globs to bare basenames — enforce the glob allowlist (or relabel it honestly)

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/approval/policy_writer.rs src/security/policy.rs src/agent/agent.rs`

## Status

- **Priority**: P1 (security — allowlist is coarser than it advertises)
- **Effort**: L
- **Risk**: MED (tightening will make some currently-auto-run commands prompt)
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The Smart preset bundle ships **read-only** command globs — `git status`,
`git log`, `git diff`, `curl --head *`, `curl -I *`, `ps *`, `find *`, `env`
(`src/approval/presets/policy_smart.toml`). But `apply_preset_to_config`
reduces each glob to its bare **basename** before storing it in
`config.autonomy.allowed_commands`, and the runtime shell gate matches
allowlist entries by basename. So the enforced allowlist is `git`, `curl`,
`ps`, `find`, `env` — the **whole binaries**, not the read-only forms.

Two concrete consequences:

1. The enforced allowlist is broader than the bundle the operator wrote and the
   model is shown. `git <anything>`, `curl <anything>`, `env <anything>` match.
   (Whether an individual command then auto-runs also depends on its risk class
   and `require_approval_for_medium_risk` — e.g. `git push` is Medium and still
   prompts by default. The defect is that the *allowlist layer* no longer
   distinguishes read-only from mutating forms; it leans entirely on the risk
   classifier, which has its own gaps — see plan 200.)
2. The on-disk `command_allowlist.toml` (globs) has **zero enforcement effect** —
   it is read only to render the system prompt (`src/agent/agent.rs:886-894`).
   An operator who hand-edits it (its own header invites this) changes what the
   model is *told*, not what is *enforced*. That is a config-that-lies.

## Current state

### Glob → basename collapse — `src/approval/policy_writer.rs:229-251`

```rust
    if let Ok(bundle) = toml::from_str::<PolicyBundle>(preset.bundle()) {
        let mut basenames: Vec<String> = bundle.command_allowlist.patterns.iter()
            .filter_map(|pat| {
                let first = pat.split_whitespace().next()?;   // "git status" -> "git"
                let base = first.rsplit('/').next()?;          // "/usr/bin/git" -> "git"
                if base.is_empty() { None } else { Some(base.to_string()) }
            }).collect();
        basenames.sort(); basenames.dedup();
        config.autonomy.allowed_commands = basenames;          // basenames, not globs
    }
```

### Runtime gate matches by basename — `src/security/policy.rs:790-800`

```rust
        let on_config_list = self.fields().allowed_commands.iter().any(|a| a == base_cmd);
```

### Globs are prompt-only — `src/agent/agent.rs:886-894`

The on-disk `command_allowlist.toml` globs are read to build the system prompt,
never for enforcement.

## The fix — pick a direction (this is a real decision; state it in the PR)

### Option A (preferred, higher-integrity) — enforce the glob allowlist

Make the runtime gate match a command against the **full glob patterns**, not
just the basename:

1. Store the globs (not basenames) that the preset/bundle defines. Add a
   representation the gate can match — e.g. keep `allowed_commands` as globs and
   match `"<base> <args...>"` against each pattern with a small, well-tested glob
   matcher (support only `*` — no regex).
2. Point the runtime gate at the same `command_allowlist.toml` the operator
   edits and the model is shown, so all three (file / prompt / enforcement)
   agree.
3. Keep the plain-basename entries working: a bare `git` pattern still means
   "any git" for operators who want that; the read-only bundle uses the narrower
   `git status`-style globs.

This is the L-effort path: it touches the allowlist storage shape, the matcher,
and the preset application. Do it behind thorough tests (Step: Done criteria).

### Option B (smaller, honest) — keep basename matching but stop the lie

If Option A's blast radius is judged too large for one PR:

1. Keep basename enforcement, but make `apply_preset_to_config` and the docs
   **honest** that the enforced allowlist is basename-granularity: the Smart
   bundle's read-only globs are advisory (shown to the model), and the enforced
   list is the set of allowed *binaries*.
2. Correct the `command_allowlist.toml` header and any doc that implies the
   globs are enforced, so an operator editing it is not misled.
3. Lean on the risk classifier (plan 200) as the real gate for mutating
   subcommands, and note the dependency.

Option B is a labeling/expectations fix, not an enforcement change. Prefer A;
fall back to B only with an explicit note that enforcement stays basename-level.

## Files

- **In scope (A)**: `src/approval/policy_writer.rs`, `src/security/policy.rs`
  (the matcher + allowlist shape), possibly `src/config/schema.rs` if the
  allowlist storage type changes, `src/agent/agent.rs` (unify the source).
- **In scope (B)**: `src/approval/policy_writer.rs` (comment), `command_allowlist.toml`
  header + docs.
- **Out of scope**: the risk classifier (plan 200), `is_args_safe` (plan 196),
  the CLI/API allowlist writers (plan 208).

## STOP conditions

- Before writing code, DECIDE Option A vs B and record it. If A's change to
  `allowed_commands`'s type ripples into config migration (schema version bump),
  STOP and confirm the migration path is acceptable before proceeding — the
  autonomy allowlist is a public config contract.
- If the runtime gate already matches globs (drift), this may be partly done —
  reconcile and report.

## Done criteria

**Option A:**
1. `cargo fmt`/`clippy`/`cargo test -p rantaiclaw --lib` clean.
2. Tests: with the Smart bundle applied, `git status` is allowed but a
   non-read-only `git <mutating>` that is NOT in the globs is NOT matched by the
   allowlist layer (it falls to the risk/approval path); `curl --head x` allowed
   but `curl -X POST x` not allowlist-matched. The on-disk globs and the enforced
   set are proven identical by a test that loads `command_allowlist.toml` and
   asserts the gate honors it.

**Option B:**
1. `cargo fmt`/`clippy`/`cargo test` clean.
2. A test/asserted doc note that `command_allowlist.toml` is advisory; the PR
   description states enforcement is basename-level and cross-references plan 200
   as the mutating-subcommand gate.

## Test plan

Option A needs a focused glob-matcher test suite (only `*` semantics; anchor to
the full command; no partial matches) plus an end-to-end "file globs == enforced"
test. Option B needs the labeling assertions. Either way, add a test proving
`apply_preset_to_config` no longer silently widens `git status` to all-`git`
enforcement (A) or documents that it does (B).

## Risk & rollback

- **Risk**: MED — Option A tightens enforcement; commands that auto-ran via the
  basename widening will now require the risk/approval path. That is the intent.
  Option A may need a schema-version bump (allowlist shape) — treat the config
  key as a public contract with a migration.
- **Rollback**: revert; if a schema bump was involved, ensure the migration is
  reversible or gated.

## Maintenance note

The three representations of the allowlist — the on-disk globs, the system-prompt
text, and the enforced list — must not diverge. Whichever option is chosen, add
the test that keeps them in agreement; the divergence here is exactly what let
the "read-only" bundle enforce whole binaries.
