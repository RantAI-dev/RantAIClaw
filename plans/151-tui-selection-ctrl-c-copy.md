# Plan 151: In-app text selection + Ctrl+C copy in the TUI chat pane

> **Executor instructions**: Follow step by step. Steps 1–3 are one PR. Run every
> verification command, including the tmux live drive in step 4. If anything in
> "STOP conditions" occurs, stop and report. When done, add this plan's row in
> `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 0f30035..HEAD -- src/tui/app.rs src/tui/render.rs src/tui/context.rs`
> Line numbers below are from `0f30035` (v0.20.1-alpha). Non-empty diff → re-verify
> each cited line before editing.

## Status

- **Priority**: P2 — a capability users had was silently removed; daily-use friction
- **Effort**: L
- **Risk**: MEDIUM (touches the chat render path and the Ctrl+C handler)
- **Depends on**: none
- **Category**: feature (restores a lost behavior)
- **Planned at**: commit `0f30035` (v0.20.1-alpha), 2026-08-16

## Why this matters

Through v0.9.0-alpha the TUI did not capture the mouse: in Windows Terminal an
operator could drag-select chat text and press Ctrl+C to copy — the platform's
native "copy like normal Windows" flow. Commit `1ad0109` (PR #271, shipped
v0.10.0-alpha) enabled `EnableMouseCapture` to fix a real bug — in alt-screen,
wheel events arrive as Up/Down arrows and the composer treated them as history
recall, silently rewriting the input (`src/tui/app.rs:7291`, pinned by
`mouse_wheel_scrolls_chat_not_composer:8818`). Side effect, never noted in the
changelog: drag-selection died, and with it Ctrl+C copy. An operator reported
exactly this regression.

The two goals are not in conflict — the terminal protocol is. Mouse capture is
all-or-nothing: there is no "capture wheel, release drag" mode. The industry
answer (surveyed 2026-08-16) is to keep capture and **implement selection inside
the app**:

- **Hermes**: mouse drag highlights text with a selection background; copy via
  OSC 52. Their issue tracker maps the mines: Ctrl+C copy-vs-interrupt conflict
  (NousResearch/hermes-agent#16181), "copied N chars" but empty clipboard
  (#16019), GNOME Terminal lacks OSC 52 (#18308).
- **zeroclaw** (v0.8.2, PR #8000): click-to-copy entries + shift+drag range
  select, OSC 52 on mouse-up.
- **codex** tried the cheap alternative — `/toggle-mouse-mode` — and its own
  issue (openai/codex#1247) records why it isn't good enough (scroll dies).
- **opencode**'s copy-on-select with un-disableable toasts is the anti-pattern
  (anomalyco/opencode#21542).

Decision (operator confirmed): build the full in-app selection, no interim
`/copy`-only phase. The Windows mental model is the spec: **drag highlights,
Ctrl+C with an active selection copies; Ctrl+C without one keeps today's
meaning.** Bonus over the pre-v0.10 native behavior: the copy source is the
message text, not screen pixels — no `│` border pollution, no wrap-injected
line breaks, code fences intact.

## Current state — architecture facts the design rests on

All verified at `0f30035`:

- **Fullscreen alt-screen TUI.** `setup_terminal` (`src/tui/app.rs:7278`) →
  `enter_fullscreen` (`:7290`) enters alt-screen + `EnableMouseCapture`.
  (Doc comments around `:4252` still describe an older inline-viewport design —
  stale text, not the running code; the render body at `:4299` is fullscreen.)
- **Chat data model**: `ctx.messages` — logical entries with `role`, `content`
  (raw text), optional `tool_calls`. This is the copy source of truth.
- **Render path** (`render_chat_pane`, `:5128`): every frame builds one flat
  `Vec<Line>` — per message: blank separator (from the second message on), then
  `render_message_lines(...)` → `extend_wrapped(&mut lines, &raw, inner_w)`;
  a streaming tail (spinner + partial) appends after. Window math at
  `:5204-5214`: `scroll_offset` counts display lines up from the newest
  (0 = bottom, clamped to `total - inner_h`), visible slice =
  `lines[start..end]`, drawn inside a `Borders::ALL` rounded block — so the
  inner text area is the pane rect shrunk by 1 on every side. **Borders never
  enter the line buffer.**
- **Layout**: chat pane is `chunks[0]` of the vertical layout (`:4307-4316`).
  `ctx.last_chat_rows` (`:5137`) already stashes a layout fact on the context —
  the precedent this plan extends.
- **Mouse events today**: only `MouseEventKind::ScrollUp/ScrollDown`
  (`:907-911`, ±3 lines on `ctx.scroll_offset`). Down/Drag/Up are ignored —
  free for this feature.
- **Modals** own the whole screen when active (`modal_active()`, `:4362`).
- Wheel regression test: `mouse_wheel_scrolls_chat_not_composer` (`:8818`) —
  must stay green untouched.

## Design

### Selection model (line-granularity, provenance-anchored)

Selection endpoints are **not** display-line indices (those shift every frame
while streaming). They are provenance pairs:

```rust
/// (message index in ctx.messages,
///  RENDERED pre-wrap line index within that message's render_message_lines output)
type LineAnchor = (usize, usize);
pub struct Selection { anchor: LineAnchor, head: LineAnchor }
```

**The anchored unit is the rendered line, NOT a `msg.content` line.** The
renderer prepends a role label line ("You:" / "Assistant:"), inserts tool-call
blocks, and converts inline markdown to styled spans (`render.rs:173-183`), so
rendered-line indices and content-line indices never align. Provenance and
extraction must both index into the SAME `render_message_lines` output or the
copy silently grabs the wrong lines.

`render_chat_pane` builds, alongside `lines`, a parallel
`provenance: Vec<Option<LineAnchor>>` (same length): `Some((msg_idx,
rendered_line_idx))` for every display line produced by wrapping rendered line
`rendered_line_idx` of message `msg_idx`; `None` for separators, the spinner,
the streaming tail, and splash lines. A rendered line that wraps into N display
lines yields N entries with the same anchor — selection is over rendered lines,
wrapping cannot corrupt it.

**Selection lifetime**: any mutation of `ctx.messages` invalidates anchors —
`/compress` (compaction replaces history), `/clear`, and any resume/history
restore. Clear the selection at every such site, and make `extract` clamp
indices defensively (out-of-range → empty result, never a panic) so a missed
site degrades instead of crashing.

Line granularity (whole lines, not columns) is deliberate: it is what zeroclaw
ships (coarser, per-entry, and users are happy), it makes the copy output exact
raw text, and it keeps the hit-test one subtraction. Column precision is a
possible later refinement, not part of this plan.

### Hit-testing

On mouse Down/Drag at screen `(col, row)`:

1. Reject if a modal is active, or the point is outside the chat pane's inner
   rect (pane rect shrunk by the 1-cell border). The chat pane rect must be
   stashed on the context at render time (new field, e.g.
   `ctx.last_chat_area: Rect` — same pattern as `last_chat_rows`).
2. `visible_idx = row - inner_top`; `global_idx = start + visible_idx` where
   `start` is recomputed from the same clamped window math as `:5204-5214`.
   Stash `start` (or recompute from stashed totals) at render time so the
   handler and the renderer can never disagree.
3. `provenance[global_idx]` → `Some(anchor)` sets/extends the selection;
   `None` (separator/spinner/splash) — for Down: clear selection; for Drag:
   clamp to the nearest anchored line inside the drag direction.

Down sets `anchor` and `head` to the hit line. Drag updates `head`. Up ends the
gesture (selection persists — copy is Ctrl+C, per the Windows model; **no**
copy-on-release, that is opencode's anti-pattern). Plain click (Down+Up, no
movement) on any line or dead space clears the selection.

### Rendering the highlight

In `render_chat_pane`, after building `lines` + `provenance`: if a selection is
active, normalize it (anchor/head may be in either order — order by
`(msg_idx, raw_line_idx)`), and for every display line whose provenance falls
inside the range, restyle the whole `Line` with a selection background
(`Style::bg` — pick from the existing theme palette in `src/tui/render.rs`,
e.g. a dim blue consistent with `Color::Rgb(40, 70, 140)` chrome). Uniform
whole-line background, like Hermes.

### Copy — Ctrl+C precedence

The Ctrl+C handler is `src/tui/app.rs:1001-1017` and has TWO arms today:
`AppState::Streaming` → set `cancelling` + send `TurnRequest::Cancel` (pinned
by the test at `:9096`); `Ready | Quitting` → quit. New precedence, **selection
first**:

1. Selection active → extract + copy + clear selection + status feedback.
   Never falls through to cancel/exit (Hermes #16181 is the cautionary tale —
   an operator with a highlight active must not kill their running turn).
2. No selection → both existing arms byte-identical (cancel while streaming,
   quit while ready) — pin BOTH with tests, not just the idle one.

Esc — the current consumer chain, in order: approval deny (`:986`), list
picker (`:1176`), info panel (`:1229`), autocomplete (`:1379`), overlay
(`:1383`), setup/wizard (`:1593`), streaming-cancel (`:1650`). **Decision**:
the selection-clear arm goes after every modal/overlay consumer and BEFORE the
streaming-cancel at `:1650` — first Esc clears the highlight, second Esc
cancels the turn. Pin this ordering with a test (selection active + streaming:
Esc clears selection, turn keeps running).

Extraction: re-render the normalized range's messages through the SAME
`render_message_lines` call the pane uses (factor the per-message
rendered-lines production into one function both callers share — they must not
be able to diverge), slice by the endpoint rendered-line indices, concatenate
each `Line`'s span contents, join with `\n`, messages with `\n\n`.

What this yields, stated honestly: WYSIWYG text. Multi-line code fences pass
through verbatim (the markdown parser is inline-only — `render.rs:387` — so
fenced blocks are untouched): the primary use case is exact. Inline styling is
already resolved to spans, so `**bold**` copies as `bold` and inline backticks
are dropped; a selection that includes the first rendered line of a message
carries its "You:"/"Assistant:" label. All acceptable — it copies what the
screen shows, minus borders and wrap.

### Clipboard transport — OSC 52, honestly

New pure helper (suggested new module `src/tui/selection.rs`, which also hosts
the hit-test + normalize + extract logic — keeps `app.rs` from growing another
subsystem):

```rust
/// \x1b]52;c;<base64(text)>\x07 — cap payload; None when text exceeds the cap.
pub fn osc52_sequence(text: &str) -> Option<Vec<u8>>
```

- Base64 standard alphabet; cap the **encoded** payload at 99 KiB (xterm's
  classic limit); over the cap → no escape, feedback says "selection too large
  to copy (N KiB)".
- Write the bytes straight to stdout inside the raw-mode session (flush).
- Base64: the `base64 = "0.22"` crate is already an unconditional dependency
  (`Cargo.toml:69`) — use it. No new dependency, no local encoder.
- **Honest feedback** (the #16019 lesson): we cannot detect whether the
  terminal honored OSC 52. The status message must not overclaim:
  `⧉ copied N lines (OSC 52 — if the clipboard is empty, your terminal may not
  support it; see /help)`. `/help` gains two lines: the Ctrl+C-copy flow, and
  the Shift+drag native-selection escape hatch (works everywhere, brings
  borders along).

### Explicitly not in scope

- Copy-on-select, toasts (opencode anti-pattern).
- `/toggle-mouse-mode` (codex; breaks wheel).
- Click-to-copy entries / code-block chrome (zeroclaw phase-2 — separate plan
  if wanted).
- Column-granular selection.
- Composer/status/overlay text selection — chat pane only.
- Streaming tail selection (provenance `None`; it is volatile mid-stream).

## Steps

### Step 1 — pure selection module

`src/tui/selection.rs`: `LineAnchor`, `Selection`, `normalize`, hit-test
(`global_line_at(row, area, start) -> usize`), `extract(messages, range) ->
String`, `osc52_sequence`. Everything here is pure and unit-tested — this is
the seam the repo prefers (`decide_gateway_action` pattern).

Tests: normalization order (both directions), extraction across one message /
across messages / endpoints mid-message (code fence must survive verbatim),
extraction with dangling anchors (msg_idx / line_idx past the end → empty, no
panic — the post-compaction case), OSC 52 base64 correctness against a known
vector, the size cap (just under / just over), empty-selection extract.

### Step 2 — render provenance + highlight + layout stash

In `render_chat_pane`: build `provenance` alongside `lines` (one branch per
`lines.push` site — separator, message wrap, spinner, streaming tail, splash);
stash `last_chat_area` + the window `start` on `ctx`; apply the highlight
style over the normalized range.

Tests: provenance length == lines length for a mixed transcript (multi-message,
wrapped long lines, streaming state); separator/spinner/splash rows are `None`;
a wrapped raw line yields identical consecutive anchors. (These run
`render_chat_pane` against a `TestBackend` frame or factor the line-building
into a testable function — prefer the factoring.)

### Step 3 — event wiring

- Mouse Down/Drag/Up arms in `handle_event` (beside `:907`), gated on
  `!modal_active()` and hit-test success.
- Ctrl+C precedence + Esc clear (verify current consumers first).
- Status-line feedback string; `/help` additions.
- Any interaction with the pending-approval pane: while an approval prompt is
  displayed, selection stays possible in the chat pane, but Ctrl+C precedence
  must be checked against the approval flow's key handling — trace it before
  wiring, and if approval consumes Ctrl+C today, selection-copy still wins only
  when a selection is active.

App-level tests (the `mouse_wheel_scrolls_chat_not_composer` pattern):
- drag over two lines → selection active; Ctrl+C → no `Quit`, selection
  cleared (and the copy path was invoked — assert via the feedback message or
  an injected writer seam).
- Ctrl+C with no selection, `Ready` state → `Quit` (today's arm, pinned).
- Ctrl+C with no selection, `Streaming` state → `TurnRequest::Cancel` sent,
  `Continue` (the existing test at `:9096` covers this — keep it green).
- Ctrl+C with selection active while `Streaming` → copy, NO cancel sent.
- Esc with selection active while `Streaming` → selection cleared, turn still
  running (the ordering decision, pinned).
- wheel still scrolls (existing test untouched and green).
- click clears selection.
- selection survives a streaming append (anchors are provenance, not display
  indices) — simulate by pushing a message mid-selection.
- compaction/`/clear` clears the selection (the D2 sites).

**Mutation proofs** (per repo practice, both directions):
- Remove the selection-first arm in Ctrl+C → the copy test must fail.
- Break provenance (off-by-one at the separator site) → the provenance-length
  or anchor test must fail.
- Revert each, confirm green.

### Step 4 — validation + live drive

```bash
cargo fmt --all -- --check
cargo clippy --lib --tests            # zero new diagnostics in touched files
cargo test --lib -- tui::
BASE_SHA=$(git merge-base origin/main HEAD) bash scripts/ci/rust_strict_delta_gate.sh
cargo check --all-targets             # examples/ escape — the PR #549 lesson
```

Repo-wide call-site sweep for any signature you change:
`grep -rn <symbol> --include=*.rs .` (src, tests, benches, **examples**).

Live drive (tmux, sandboxed HOME per the established technique):
1. Open the TUI with a scripted conversation (stub provider or a canned
   session); send mouse-down/drag/up escape sequences via
   `tmux send-keys -H` (SGR mouse encoding) or drive with a small expect
   script — verify the highlight renders (capture-pane shows the styled
   region), Ctrl+C prints the ⧉ feedback and does NOT exit, second Ctrl+C
   (no selection) exits as today.
2. OSC 52 bytes: run the TUI with stdout teed through `script`/a pty logger
   and assert the `\x1b]52;c;` sequence with the expected base64 lands in the
   output stream after the copy. (Real clipboard landing on Windows Terminal
   is verified by the operator post-ship — note it in the PR as not
   locally verifiable.)
3. Regression: wheel scroll still works; PgUp/PgDn unchanged; overlays still
   receive their keys with a selection active.

### Step 5 — docs + changelog + PR

- `CHANGELOG.md` (Unreleased → Fixed/Added): drag-select + Ctrl+C copy
  restored in-app; note the OSC 52 dependency and the Shift+drag fallback;
  name the regression window (v0.10.0-alpha → v0.20.1-alpha).
- `docs/start/troubleshooting.md`: "copied but clipboard empty" entry — OSC 52
  terminal support matrix in two sentences (GNOME Terminal/VTE = no;
  Windows Terminal, iTerm2, kitty, alacritty, wezterm, tmux ≥3.3 with
  `set-clipboard on` = yes), Shift+drag fallback.
- `docs/reference/commands.md` only if `/help` text is documented there —
  check.
- PR per template; risk `medium`, scope `tui`. No config keys → no schema
  bump.

## Risk and rollback

- **Risk**: the Ctrl+C handler is the highest-blast-radius edit (cancel/exit
  flows). The selection-first arm is a single guarded branch; every other path
  byte-identical. Render-path changes are additive (a parallel vec + a style
  pass).
- **Rollback**: revert the PR. No persisted state, no config, no schema.

## STOP conditions

- Drift check shows the render window math (`:5204-5214`) or mouse arm
  (`:907`) rewritten since `0f30035` — re-verify the design against the new
  shape before coding.
- Ctrl+C turns out to be consumed somewhere that cannot see the selection
  state (e.g. a lower-level interrupt handler outside `handle_key`) — report,
  don't force.
- The line-building factoring (step 2 tests) requires touching
  `render_message_lines`'s signature in a way that fans out beyond
  `src/tui/render.rs` + `app.rs`.
- Highlight styling cannot be applied without cloning the full line buffer
  every frame in a way that measurably lags streaming render — report with
  numbers.
- The shared rendered-lines factoring (extraction ↔ pane must use one
  function) cannot be achieved without changing `render_message_lines`'s
  public shape beyond `src/tui/render.rs` + `app.rs`.
