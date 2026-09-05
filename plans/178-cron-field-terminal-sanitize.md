# Plan 178: Sanitize cron job fields before rendering to a terminal/TUI and reject control chars at the API

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/mod.rs src/cli_style.rs src/tui/commands/cron.rs src/gateway/cron_api.rs src/memory/sanitize.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

Cron job fields (name, command, prompt) are rendered to a terminal and to the
TUI **without stripping ANSI/control escapes**. The CLI `cron list` prints the
raw payload via `crate::cli_style::dim(&payload)` (`src/cron/mod.rs:46-52`), and
`dim` (`src/cli_style.rs:51-53`) only wraps text in ANSI — it does not strip
inner escapes. The CLI post-update output prints the raw `job.command`
(`src/cron/mod.rs:166`), and the TUI writes raw name/command/prompt into its
body (`src/tui/commands/cron.rs:99-108`). Meanwhile the HTTP API accepts
`name`/`prompt`/`command` with **no control-char validation**
(`src/gateway/cron_api.rs` create 148-167, update 269-289). So a field written
over HTTP (or through an injected agent turn) is later rendered by a
more-trusted surface: embedded escape sequences can rewrite already-printed
lines, hide themselves, or spoof what a `cron list` audit shows — a
cross-surface terminal-injection / audit-spoofing vector. The widest vector is
the agent-job `prompt`. An `is_control` predicate already exists at
`src/memory/sanitize.rs:90`. This plan sanitizes at every cron render site and
rejects control chars in `name` at the API boundary so the bad value is never
stored.

## Current state

Files and roles:

- `src/cron/mod.rs:46-52` — CLI list render:
  ```rust
  let payload = if job.command.is_empty() {
      job.prompt.clone().unwrap_or_default()
  } else {
      job.command.clone()
  };
  if !payload.is_empty() {
      println!("       {}", crate::cli_style::dim(&payload));  // raw payload
  }
  ```
- `src/cron/mod.rs:166` — post-update: `println!("  Cmd : {}", job.command);`
  (raw command).
- `src/cli_style.rs:51-53` — `dim` just wraps: `style(s).dim().to_string()` (no
  stripping).
- `src/tui/commands/cron.rs:99-108` — writes raw `name`, `expression`, and
  `what` (command or prompt) into the TUI body via `write!`.
- `src/gateway/cron_api.rs` — `CreateCronBody` (148-167) and `UpdateCronBody`
  (269-289) deserialize `name`/`prompt`/`command` with `#[serde(default)]`, no
  control-char check.
- `src/memory/sanitize.rs:80-101` — `strip_invisible(raw)` already does exactly
  the right kind of filtering: it keeps `\n`/`\t`/`\r`, drops invisible format
  chars and `c.is_control()` (the predicate at **line 90**). This is the
  reference implementation to mirror or reuse.

`src/memory/sanitize.rs:84-98` (the pattern to reuse):
```rust
for ch in raw.chars() {
    let keep = match ch {
        '\n' | '\t' | '\r' => true,
        c if is_invisible_format(c) => false,
        c if c.is_control() => false,      // <-- line 90
        _ => true,
    };
    if keep { out.push(ch); } else { removed += 1; }
}
```

Repo conventions:
- Rule-of-three + DRY: multiple render sites justify one shared
  `sanitize_for_terminal` helper. Preserve `\n` and `\t` (job fields are usually
  single-line, but do not mangle legitimate whitespace); strip ESC/CSI and C0
  control bytes.
- Prefer reusing/extending `src/memory/sanitize.rs` logic over a new bespoke
  filter (its `strip_invisible` already encodes the exact keep/drop decision).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests   | `cargo test --lib cron` | all pass |
| Drift   | `git diff --stat 2aefb9f..HEAD -- src/cron src/tui/commands/cron.rs src/gateway/cron_api.rs` | only your changes |

Do **not** run a bare `cargo test` (disk-constrained). Scope with `--lib`.

## Scope

**In scope**:
- One shared `sanitize_for_terminal` helper (see Step 1 for location)
- `src/cron/mod.rs` — apply at the list payload (46-52), the list-header
  `job.schedule` (41-45), and the post-update `job.command` (166)
- `src/tui/commands/cron.rs` — apply at the body render (99-108)
- `src/gateway/cron_api.rs` — reject control chars in `name` at create/update via
  a pure `reject_control_chars` helper (see Step 4)

**Expression/schedule sanitization policy (chosen)**: the `expression`/`schedule`
field is API-validated (must parse as a cron schedule), but this plan sanitizes
it at render on **both** surfaces — the CLI list header (`src/cron/mod.rs:44`)
and the TUI body (`src/tui/commands/cron.rs`) — for consistency and
defense-in-depth against any write path that could bypass validation (direct DB,
migration, a future surface). Do not sanitize it on one surface and not the
other.

**Out of scope**:
- Escaping/altering the web-console (browser) rendering — HTML surfaces escape
  differently; this plan is terminal/TUI + the storage boundary for `name`.
- Sanitizing run-history *output* — that is plan 175's redaction concern
  (different helper, different threat).
- Changing `dim`'s contract (`src/cli_style.rs`) — it stays a pure ANSI wrapper;
  sanitize the input *before* passing it to `dim`.

## Git workflow

- Branch: `advisor/178-cron-field-terminal-sanitize`
- Conventional commits (e.g. `fix(cron): sanitize job fields before terminal render`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a shared `sanitize_for_terminal` helper

Add a `pub(crate)` helper that strips ESC (`\x1b`) and other C0 control bytes
while **preserving `\n` and `\t`**. Reuse the `strip_invisible` logic from
`src/memory/sanitize.rs:80-101` — either call an exported version of it, or add
a sibling `sanitize_for_terminal` next to it that returns just the cleaned
`String`. Preferred: export a thin wrapper from `src/memory/sanitize.rs` so
there is a single control-char policy in the repo:

```rust
/// Strip ESC/CSI and C0 control characters for safe terminal/TUI rendering,
/// preserving `\n` and `\t`. Reuses the same keep/drop policy as
/// `strip_invisible`.
pub(crate) fn sanitize_for_terminal(raw: &str) -> String {
    // same match as strip_invisible: keep \n \t \r, drop invisible-format and
    // other control chars.
    raw.chars()
        .filter(|c| matches!(c, '\n' | '\t' | '\r') || (!is_invisible_format(*c) && !c.is_control()))
        .collect()
}
```
Confirm the `is_invisible_format` visibility allows this (it is in the same
module). If a `use` is needed to reach it from `src/cron`/`src/tui`, add the
minimal `pub(crate)` export.

**Verify**: `cargo build` compiles; `cargo clippy --all-targets -- -D warnings`
→ exit 0.

### Step 2: Apply at the CLI render sites

1. `src/cron/mod.rs:46-52`: wrap the payload —
   `crate::cli_style::dim(&crate::memory::sanitize::sanitize_for_terminal(&payload))`
   (adapt the path to where the helper lives).
2. `src/cron/mod.rs:166`: sanitize `job.command` before printing —
   `println!("  Cmd : {}", sanitize_for_terminal(&job.command));`
3. Also sanitize `job.schedule` in the **list header** (`src/cron/mod.rs:41-45`).
   That header renders only `dot(job.enabled)`, the short **hex** id
   (`&job.id[..min(8)]`), and `job.schedule` — it does **not** render `name`
   (there is no `name` to sanitize here), and the hex id cannot carry a control
   char, so only `job.schedule` needs wrapping:
   `sanitize_for_terminal(&job.schedule)`. This is the CLI half of the
   expression-sanitization policy (see Scope); the payload at 46-52 is already
   covered by item 1.

**Verify**: `cargo test --lib cron` → pass. Manually: a job whose command
contains an ESC sequence renders with the escape stripped (no cursor movement).

### Step 3: Apply at the TUI render site

`src/tui/commands/cron.rs:99-108`: sanitize `name`, `expression`, and `what`
before writing them into `out`. Wrap each interpolated field with
`sanitize_for_terminal(...)`.

**Verify**: `cargo test --lib` for the TUI cron module (if tests exist) or
`cargo build` — the TUI is a binary crate; a compile + clippy pass is the gate
here. `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Reject control chars in `name` at the API boundary

Extract a **pure** helper in `src/gateway/cron_api.rs` and put the rejection
logic there (do **not** inline the check in the async handlers only — a
handler-level 400 test needs `AppState` + `HeaderMap` auth + `spawn_blocking`,
and the `#[cfg(test)] mod tests` at line 392 holds only pure `resolve_job_kind`
tests, so there is no exemplar; a weak executor would write a vacuous handler
test):

```rust
/// Reject a schedule name containing control characters. A schedule name has no
/// legitimate newline/tab need, so any `char::is_control` char is refused.
fn reject_control_chars(name: &str) -> Result<(), String> {
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters".to_string());
    }
    Ok(())
}
```

Call it from both `create_cron` (before persisting) and `update_cron`, mapping
`Err(reason)` to `err_400(reason)`. This ensures the bad value is never
**stored**, so even a surface that forgets to sanitize on render is safe. Do
**not** reject control chars in `prompt`/`command` at the API (they can
legitimately contain newlines); those are handled by render-time sanitization
(Steps 2-3).

**Verify**: add a test in the `#[cfg(test)] mod tests` (line 392) that targets
the **pure** `reject_control_chars` helper directly — `Err(...)` for a name
containing a control char, `Ok(())` for a clean name — not a handler-level 400
assertion. `cargo test --lib` for the gateway cron tests → pass.

## Test plan

- Helper test (next to `strip_invisible` tests in `src/memory/sanitize.rs`):
  `sanitize_for_terminal` removes an ESC/CSI sequence and other C0 controls,
  preserves `\n`/`\t`, and leaves ordinary text unchanged.
- `src/gateway/cron_api.rs` test: target the **pure** `reject_control_chars`
  helper directly — `Err(...)` for a `name` with a control char, `Ok(())` for a
  clean `name` — not a handler-level 400 assertion (no AppState/auth exemplar
  exists in that test module).
- Verification: `cargo test --lib cron` (and the sanitize module tests) → all
  pass, including new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` and the sanitize-module tests pass; new tests exist
- [ ] A single `sanitize_for_terminal` helper is applied at every cron terminal/
      TUI render site (`src/cron/mod.rs` list payload + list-header schedule +
      post-update, `src/tui/commands/cron.rs`); `expression`/`schedule` is
      sanitized on both the CLI and TUI surfaces (policy in Scope)
- [ ] The HTTP API rejects control chars in `name` at create and update via a
      pure `reject_control_chars` helper that is tested directly (not a
      handler-level 400 assertion)
- [ ] `dim` (`src/cli_style.rs`) is unchanged (input is sanitized before it)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report if:

- The "Current state" excerpts do not match live code (drift since 2aefb9f).
- `is_invisible_format`/`strip_invisible` cannot be reached with a `pub(crate)`
  export without a wider refactor — report and fall back to a small local
  helper duplicated once (rule-of-three not yet met is acceptable here, but
  prefer reuse).
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- A reviewer should scrutinize: that sanitization happens on the value *before*
  `dim`/`write!` (not after), that `\n`/`\t` are preserved, and that the API
  `name` rejection does not also block legitimate names (unicode letters,
  spaces, punctuation are fine — only control chars are rejected).
- Deferred: browser/web-console rendering of these fields (different escaping
  model) — track separately if the console is found to render raw field values.
- If a new cron render surface is added later, it must call
  `sanitize_for_terminal`.
