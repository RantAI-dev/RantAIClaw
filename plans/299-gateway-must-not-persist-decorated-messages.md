# Plan 299: Stop the gateway persisting client render-mode decoration into chat history

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat d5a1bba..HEAD -- src/gateway/api_v1.rs src/sessions/store.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P2 (follow-up to W1-7 / claw-ui plan 297)
- **Effort**: S–M
- **Risk**: LOW–MED (changes what is stored for a turn)
- **Category**: bug
- **Planned at**: commit `d5a1bba`, 2026-09-05
- **Origin**: found while executing claw-ui #109. The console side is fixed; the residue is
  server-side, so it could not be closed from that repo.

## Why this matters

claw-ui #109 stopped the console prepending conversation history to each message. One piece
of client-side decoration remains: the generative-UI render instruction, which the console
attaches to the message body when that mode is on.

The gateway persists `body.message` verbatim, so the instruction is stored as part of the
user's turn and replayed on every later turn — including after the user switches back to
markdown. The model is then told to render generative UI for a session that no longer wants
it, and exported transcripts carry an instruction the user never typed.

The general defect is broader than this one string: **the gateway stores whatever the client
decorated, as if the user had typed it.** Any future client-side prefix inherits the bug.

## Current state (verified at `d5a1bba`)

`src/gateway/api_v1.rs` persists the incoming `body.message` as the user turn, then replays
stored history on subsequent turns (that replay is what made removing the console's history
block safe in #109).

The console side, post-#109: the history block is gone; the render-mode instruction is still
applied to the outgoing message text.

`rg -n 'RENDER MODE' src/` in this repo returns nothing — the gateway has no knowledge of the
marker, which is why it cannot strip it today.

## Steps

1. **Decide where the boundary belongs, and say so in the PR body.** Two shapes:
   (a) the client sends render mode as a **structured field** alongside the message, and the
   gateway applies it to the outgoing prompt without persisting it; or
   (b) the gateway strips known decoration markers before persisting.
   (a) is correct — (b) is a denylist that the next decoration will slip past. Prefer (a) and
   treat (b) only as a migration for already-stored turns.
   **Verify**: the chosen shape is written down before coding.

2. **Add the structured field** to the chat request, defaulted so existing clients are
   unaffected. Persist the user's text; apply the mode to the prompt sent to the provider.
   **Verify**: `docs/reference/api-v1.md` gains the field — this repo's contract rule says a
   request field that exists must be documented.

3. **Coordinate the console change.** claw-ui must send the field instead of decorating the
   body. That is a claw-ui PR; note the dependency in both PR bodies so they land in the
   right order (gateway first, tolerant of the old shape).

4. **Decide what happens to turns already stored with the marker.** Either leave them (they
   are historical) or strip on read. Do not silently rewrite stored history.

5. **Tests.** (a) A chat request with the render mode set persists the user's text without the
   instruction; (b) the instruction still reaches the provider for that turn; (c) a later turn
   in the same session does not carry it.
   **Verify**: `cargo test --lib gateway` and `cargo test --test config_api` pass.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib gateway` passes with the three new tests.
- Stored user turns contain what the user typed, and nothing else.
- `docs/reference/api-v1.md` documents the new field.

## STOP conditions

- The replay path turns out to re-send stored turns to the provider verbatim in a way that
  makes (b) in step 5 impossible without changing replay semantics → STOP and report.
- The change would break an older console version still decorating the body → STOP; the
  gateway must tolerate both shapes for at least one release.

## Test plan

Three tests in the gateway module. Assert on what is persisted, not only on what is sent.

## Maintenance note

The contract this establishes: the message field is the user's words. Anything the client adds
for rendering or context travels in its own field. Worth stating in `api-v1.md` beside the
chat endpoint so the next client does not re-invent decoration.

## Rollback

One commit plus a documented request field. Reverting restores the decoration behaviour;
coordinate with the console PR so the two do not diverge.
