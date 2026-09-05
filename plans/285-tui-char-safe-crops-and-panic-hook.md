# Plan 285: Stop the TUI panicking on multibyte tool output, and restore the terminal if it does

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the row in `plans/280-production-readiness-handoff.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0dd4c03..HEAD -- src/tui/app.rs src/tui/commands/calls.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P0 — BLOCKER (ledger W0-6)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: nothing
- **Category**: bug
- **Planned at**: commit `0dd4c03`, 2026-09-04

## Why this matters

Two sites slice arbitrary text by **byte** index. One crops the first line of tool stdout,
the other crops model-authored argument strings. Any non-ASCII character straddling the cut
panics the render loop. An `ls` over a filename with an accented or CJK character is enough,
which makes this routine rather than exotic for this project's users.

Worse than the crash: there is no `panic::set_hook` anywhere in the TUI, and
`restore_terminal` runs only when the loop returns `Err`. A panic therefore leaves raw mode,
the alternate screen and mouse capture switched on — the user's shell is unusable until they
blindly type `reset`.

The repo has already fixed this exact class once, in `compact_args_for_log`, and kept
regression tests for it. These two sites were missed.

## Current state (verified at `0dd4c03`)

```rust
// src/tui/app.rs:2643-2656 — both arms slice by byte
} else if preview.len() > 60 {
    format!("{}…", &preview[..60])
...
} else if preview.len() > 60 {
    format!("error: {}…", &preview[..60])
```

```rust
// src/tui/commands/calls.rs:113-117
if s.len() > MAX_ARG_VALUE_LEN {
    format!("{}… ({} chars)", &s[..MAX_ARG_VALUE_LEN], s.len())
```

The safe pattern already in the tree, with tests at `src/tui/app.rs:7365` and `:7374`
(`does_not_panic_when_the_crop_lands_inside_a_multibyte_char`,
`crops_every_multibyte_width_safely`):

```rust
// src/tui/app.rs:7415
fn compact_args_for_log(args: &serde_json::Value) -> String { ... const MAX_LEN: usize = 50; ... }
```

`src/tui/render.rs` also carries a char-safe `truncate_preview`.

## Steps

1. **Reuse, do not re-invent.** Read `render::truncate_preview` and the crop inside
   `compact_args_for_log`. Pick the one that already does char-safe cropping with an
   ellipsis and call it from both sites above. If neither is reachable from
   `commands/calls.rs`, lift the smaller of the two into a shared helper — but only then
   (rule of three: this is the third caller, so extraction is justified).
   **Verify**: `rg -n '&\w+\[\.\.' src/tui/` returns no byte-slice of user or model text.

2. **Install a terminal-restoring panic hook.** Where the TUI enters raw mode, chain a
   `std::panic::set_hook` that calls the existing `restore_terminal` before delegating to
   the previous hook, so the panic message is still printed — into a usable terminal.
   **Verify**: read the entry point (`run_tui`, around `src/tui/app.rs:7976`) and confirm
   the hook is installed after terminal setup and that `restore_terminal` is idempotent.

3. **Regression tests, modelled on the existing pair.** One test per site, using the same
   fillers the existing tests use (`é`, `世`, `🦀`) across a padding range so the cut lands
   mid-codepoint. Assert the function returns; a panic fails the test by itself.
   **Verify**: `cargo test --lib tui` passes; each new test panics if you revert its site.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib tui` passes with the new tests.
- Reverting either site makes its test panic (proves the tests are not vacuous).

## STOP conditions

- `restore_terminal` turns out not to be safe to call twice → STOP; make it idempotent in
  its own commit first.
- The panic hook would swallow the panic message → STOP; the message must still reach the
  user.

## Test plan

Two tests beside the existing multibyte tests in `app.rs`, one for `calls.rs`. Follow the
existing naming (`<subject>_<expected_behavior>`).

## Maintenance note

Any new `&s[..n]` on text that can contain tool output, model output, or filenames is this
bug again. The three char-safe helpers are the only sanctioned crop.

## Rollback

One commit across two files plus tests; no state or contract change.
