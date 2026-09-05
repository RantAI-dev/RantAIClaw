# Plan 204: Close the `git checkout` leading-dash option-injection gap

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tools/git_operations.rs`

## Status

- **Priority**: P2 (security — option injection into git checkout)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`git_operations` `checkout` passes the branch spec directly as
`["checkout", branch_name]` with **no `--` separator**, and `sanitize_git_args`
does not reject a bare leading-dash token. So a `branch` value that starts with
`-` is interpreted by git as an **option**, not a ref:

- `{operation:"checkout",branch:"-f"}` runs `git checkout -f`, discarding
  uncommitted working-tree changes with no pathspec.
- Other single-token flags (`--orphan`, `-b`, …) reach `git checkout` as options.

The sibling subcommands already close this: `git_add` and `git_diff` use `--`
(`git_operations.rs:336,148`). `checkout` is the outlier.

## Current state

### `git_checkout` — `src/tools/git_operations.rs:352-372`

```rust
    // passes the single sanitized token as ["checkout", branch_name] — no `--`
    // sanitize_git_args (git_operations.rs:23-50) blocks --exec=/--upload-pack=/-c
    // etc., but NOT a bare leading-dash token like `-f` or `--orphan`.
    // git_checkout additionally bans only @ ^ ~ (git_operations.rs:368).
```

### The pattern the other subcommands use — `src/tools/git_operations.rs:336,148`

`git_add`/`git_diff` insert `--` before user-controlled paths, so a
leading-dash value is treated as a pathspec/ref, not an option.

## The fix

In `git_checkout`, insert a `--` separator before the branch token:

```rust
    // Before: args = vec!["checkout", branch_name]
    // After:
    let args = vec!["checkout", "--", &branch_name];
```

`git checkout -- <ref>` still checks out the ref; the `--` guarantees the token
is never parsed as an option. Additionally (belt-and-braces), reject a
`branch_name` that starts with `-` in `git_checkout`'s validation, so the intent
is explicit even if a future edit drops the `--`.

Verify that `git checkout -- <branch>` works for the tool's real use (switching
branches): if the tool's `checkout` is only ever used to switch to an existing
branch/commit, `--` is correct. If it also creates branches (`-b`), that path
must pass `-b` as a **separate, validated** argument, not as part of
`branch_name` — check the tool's schema and preserve any legitimate `-b`
support explicitly rather than via a leading-dash branch value.

## Files

- **In scope**: `src/tools/git_operations.rs` — `git_checkout` only.
- **Out of scope**: the other git subcommands (already correct), the
  `sanitize_git_args` blocklist (plan 196 covers the quote/abbrev class for the
  policy layer; this is the tool layer), the shell tool.

## STOP conditions

- If `git_checkout` legitimately supports branch creation via a leading-dash
  value (e.g. the schema documents `branch:"-b newbranch"`), STOP and redesign:
  expose `-b`/create as a separate boolean/param, keep `--` for the ref. Do not
  silently break branch creation.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib tools::git_operations` passes with new tests.
4. New tests:
   - `{operation:"checkout",branch:"-f"}` does not run `git checkout -f` — it is
     rejected or treated as a (non-existent) ref, not an option. (Assert on the
     built argv containing `--` before the token, or on a rejection.)
   - A normal `{operation:"checkout",branch:"main"}` still builds
     `["checkout","--","main"]` and works.

## Test plan

The git tests likely run against a temp repo. If the tool exposes a way to
inspect the built argv without executing (or a dry-run), assert the `--` is
present. Otherwise run against a temp git repo with an uncommitted change and
assert `checkout -f` did NOT discard it. Mirror the existing `git_operations`
test setup.

## Risk & rollback

- **Risk**: LOW — `--` is standard git usage; the only way it regresses is if
  the tool relied on leading-dash branch values for option behavior, which the
  STOP condition covers.
- **Rollback**: single-file revert; no schema/config/migration change.

## Maintenance note

Every git subcommand that forwards a user-controlled token as a positional
should use `--`. A quick audit of `git_operations.rs` for subcommands still
missing it (beyond `checkout`) is worth a follow-up if any remain.
