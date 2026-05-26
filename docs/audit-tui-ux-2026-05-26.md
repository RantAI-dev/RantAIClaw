# TUI UX Audit — 2026-05-26

Time-bounded read-only audit (~60-90 min) of the inline TUI (`src/tui/`) at HEAD = `320f3b9`. Scope was the user-facing surfaces that the recent fix(tui) cursor/last_error patches touched: input path, status bar, approval prompt, streaming feedback, key handler precedence, scrollback timing. **No fixes applied** — this is a finding list for triage.

## Severity legend

- **P0** — Functional break or data loss visible to users; ship a fix before the next release.
- **P1** — Visible UX bug or misleading affordance; should go into the next release if cheap.
- **P2** — Subtle/edge-case bug or stale claim; queue but don't gate the release.
- **P3** — Cosmetic, performance, or pre-existing tech debt; mention to maintainers but don't act.

## Quick summary

11 findings: **0 P0, 5 P1, 4 P2, 2 P3**. The biggest single UX cluster is the input/streaming feedback surface — three of the P1s (paste, working-indicator hint, queued_turns invisibility) all land there.

| # | Severity | Area | Title |
|---|----------|------|-------|
| 1 | P1 | input | Bracketed paste not enabled — multi-line paste auto-submits at newlines |
| 2 | P1 | status bar | `queued_turns` is incremented but never rendered — silent queueing |
| 3 | P1 | status bar | `context_window` is dead (always `None`) — `pct%` indicator permanently hidden |
| 4 | P1 | streaming | Working indicator says "esc to interrupt" but Esc does nothing during streaming — Ctrl+C is the actual key |
| 5 | P1 | approval | Approval prompt does not poll for security-policy timeout — stale "press Y/A/N" sticks after server-side auto-deny |
| 6 | P2 | input | Cursor positioning uses char count, not display width — wrong column for CJK/emoji/combining marks |
| 7 | P2 | approval | Overwritten (older) pending approvals auto-deny silently — no audit-trail message |
| 8 | P2 | scrollback | Mid-turn partial assistant output lost on terminal resize |
| 9 | P2 | scrollback | `body[committed..]` byte-slice at finalize_turn assumes `final_text == sum(stream chunks)` — fragile for providers that emit thinking-tokens-in-stream / trimmed-final |
| 10 | P3 | key handling | `KeyCode::BackTab` (Shift+Tab) and `Ctrl+G` (external editor) fire during setup_overlay / wizard — no guards |
| 11 | P3 | tech debt | Dead duplicate `render_status` / `render_input` methods (~150 LoC) never called |

---

## Findings

### 1. Bracketed paste not enabled — multi-line paste auto-submits at newlines · **P1 · input**

**Where:** `src/tui/app.rs:4868` (`setup_terminal`) and `handle_event` at 597-611.

**Symptom:** When a user pastes text containing newlines, the terminal sends each char as a separate `Event::Key`. The literal `\n` is delivered as `KeyCode::Enter`, and our Enter arm (line 933) calls `submit_input()`. So pasting "line 1\nline 2\nline 3" submits "line 1" immediately, then "line 2", then "line 3" — three separate turns, not one prompt.

**Root cause:** `enable_raw_mode()` is called at startup but `EnableBracketedPaste` is not. No `Event::Paste(_)` handler exists in `handle_event`.

**Fix scope:** ~10 LoC. Emit `EnableBracketedPaste` after `enable_raw_mode`, emit `DisableBracketedPaste` in `restore_terminal`, add an `Event::Paste(text)` arm that calls a new `paste_at_cursor(text)` helper on `TuiContext` (insert at cursor_pos, advance cursor_pos by char count). Newlines inside the pasted text become literal `\n` in the buffer, matching `Ctrl+J` semantics.

**Risk:** Low. Bracketed paste is widely supported (alacritty, kitty, wezterm, gnome-terminal, iTerm2, Windows Terminal, tmux ≥ 3.2). For terminals that ignore the escape, behavior degrades to today's behavior — no regression.

---

### 2. `queued_turns` invisible to user · **P1 · status bar**

**Where:** Set at `app.rs:1430` (submit_input while streaming) and decremented at `2127` (finalize_turn). Status bar render at `4243-4335` does NOT include it.

**Symptom:** User sends a message during streaming. submit_input increments `queued_turns`. The buffer clears (user thinks "sent"). But the only visible change is the buffer emptying — no toolbar segment shows "+1 queued" or similar. If the user queues 3 messages, they see no count, no confirmation, no way to cancel just the queued tail.

**Fix scope:** ~5 LoC. Add a `queued_turns > 0 → " · 2 queued"` span to `render_status_pane` between msgs and age. Optional second hook: confirmation message in scrollback ("queued · 2 turns ahead") on each post-streaming submit.

**Risk:** None. Pure additive.

---

### 3. `context_window` dead field — `pct%` permanently hidden · **P1 · status bar**

**Where:** `src/tui/context.rs:35` declares the field, `:152` initializes to `None`. Grep shows **no write site anywhere in the codebase**.

**Symptom:** Status bar `pct%` indicator (`app.rs:4295` and the duplicate at `:3584`) is gated on `window > 0` — always false, so the percentage never renders. The comment in context.rs:31 ("`None` when the provider didn't surface a window size") describes intended behavior, but nothing actually surfaces a window size. The code path is dead.

**Fix scope:** Two options.
- (a) Wire it up: at agent init / model switch, look up the model's context window from `crate::onboard::wizard::curated_models_for_provider` (or wherever model metadata lives) and stuff it into `ctx.context_window`. ~15-20 LoC across `reload_config`, the ListPickerKind::Model arm, and ModelCommand::execute.
- (b) Delete it: remove the field, the pct calculation, and the `/window` segment. ~10 LoC removal.

**Risk:** (a) low if model metadata exists. (b) zero. Choice is a product call: do we want users to see context-window pressure?

---

### 4. Working indicator says "esc to interrupt" but Esc does nothing during streaming · **P1 · streaming**

**Where:** `src/tui/widgets/working_indicator.rs:90, 106`. Both `Thinking` and `Tool` variants render `Span::styled("esc to interrupt", muted)`.

**Symptom:** During streaming, pressing Esc falls through every guarded Esc arm (overlay/picker/autocomplete/setup all require their respective state to be Some). No arm matches → Esc does nothing. Ctrl+C is the actual cancel key (app.rs:650-666).

**Fix scope:** Two options.
- (a) Change the hint to `"ctrl+c to interrupt"` — 1-LoC string change in working_indicator.rs.
- (b) Add an Esc arm during AppState::Streaming that mirrors Ctrl+C's cancel logic — ~6 LoC. Bonus: cancel UX matches the hint, and Esc-during-streaming becomes meaningful.

**Risk:** (a) zero. (b) low — only concern is users accidentally Esc'ing out of long turns, but that's already true for Ctrl+C.

---

### 5. Approval prompt does not poll for server-side timeout — stale "press Y/A/N" sticks · **P1 · approval**

**Where:** `src/tui/app.rs:1454-1490` (`drain_events` pending-approvals branch). The TUI's `pending_approval: Option<PendingRequest>` is only mutated in two places: `resolve_pending_approval` (user-driven, `:509`) and the drain that *adds* new approvals (`:1477`). Nothing checks whether the held request has already timed out in the underlying `security::pending()` registry.

**Symptom:** User leaves the approval prompt up for longer than the registry's timeout. The registry auto-denies (the comment at `:1429` confirms this is expected). The shell tool error returns to the LLM, which may finish the turn with a stuck approval-prompt overlay still drawn. User finally presses Y → `resolve_by_basename` returns no match → the audit line says `"⚠ ... was no longer pending"`. Acceptable feedback, but the prompt stuck on screen for many seconds with no indication that the decision window had closed.

**Fix scope:** ~10 LoC. In `drain_events`, after the pending-approval add loop, check `pending_approval.as_ref().map(|r| security.pending()?.contains(&r.id))`. If the request is no longer in the registry, take it (drop the prompt) and append a system message: `"⚠ approval window closed for ``X`` — turn already moved on"`.

**Risk:** Low. Requires the security registry to expose a `contains(id)` or "is still pending" method — verify the interface before commit. If it doesn't, a small extension to `security::pending()` is needed.

---

### 6. Cursor positioning uses char count, not display width — wrong column for wide/zero-width glyphs · **P2 · input**

**Where:** `src/tui/app.rs:4222-4239` (the wrap-model loop inside `render_input_pane` that I added with the recent cursor patch). Each char advances `col` by exactly 1.

**Symptom:** Type a CJK string like `こんにちは` and the cursor sits halfway through the visible glyphs. Same for emoji (most are 2 cells, some sequences more). For combining marks (e.g. `é` typed as `e + U+0301`), the cursor over-advances.

**Note in code:** I already documented the trade-off in the comment block at `:4218`. This is here to flag the issue formally, not to suggest the implementation was wrong-headed — ASCII-only was an intentional first step.

**Fix scope:** ~5 LoC + 1 dep. Pull `unicode-width` (already in lockfile transitively, line 7322 of Cargo.lock) into Cargo.toml as a direct dep, replace the `+= 1` with `+= ch.width().unwrap_or(0) as u16`. Newlines stay as-is.

**Risk:** Low. The wrap calculation will then match ratatui's own width handling. Edge case: zero-width chars with `cursor_pos` exactly between a base char and its combining mark — cursor lands on the base char's cell, which is what most terminals do anyway.

---

### 7. Overwritten pending approvals auto-deny silently · **P2 · approval**

**Where:** `src/tui/app.rs:1477`. The comment at `:1423-1429` says "Replacing an earlier unresolved request is fine — the inner registry tracks all of them by id, and the user only sees the newest in the prompt. Older ones still auto-deny on timeout."

**Symptom:** Two tool calls fire in quick succession during one turn. Request A surfaces the inline prompt. Before user decides, request B arrives → A is overwritten in `pending_approval`, only B is visible. A still exists in the security registry and will auto-deny on timeout. **No audit-trail line is committed for A's eventual auto-deny** — it just disappears.

**Fix scope:** ~5 LoC. Either:
- (a) When overwriting, append a system message: `"⚠ approval for ``A`` rolled over — earlier prompt auto-denies on timeout"`, OR
- (b) Queue multiple pending approvals (refactor `pending_approval: Option<_>` → `VecDeque<_>` + a "1 of 2" indicator on the prompt).

**(a) is cheap and informative; (b) is the right long-term fix but invasive.**

**Risk:** (a) zero. (b) needs careful key-routing — Y/A/N decide the FRONT of the queue, advance after each.

---

### 8. Mid-turn partial assistant output lost on terminal resize · **P2 · scrollback**

**Where:** `src/tui/app.rs:5519-5548` (the Resize event branch in `run_loop`). On resize, the code clears screen + scrollback (`\x1b[3J\x1b[2J`), recreates the inline terminal, and replays `app.context.messages`.

**Symptom:** User resizes the terminal while the assistant is mid-stream. `state.partial` holds the in-flight reply but it's not yet in `context.messages` (only persisted in `finalize_turn`). The screen-wipe + replay drops everything in the partial accumulator from the visible scrollback. When the turn finishes, only the FINAL response gets committed via `_continuation` — the user sees the assistant's reply appear all at once instead of having watched it stream.

**Note:** The data isn't actually lost — `state.partial` is still in memory and finalize_turn will commit it. It's only the visual streaming experience that's lost. Tester might describe it as "the spinner ran for 30s then the whole reply appeared at once."

**Fix scope:** ~15 LoC. After the screen wipe + terminal rebuild, if `AppState::Streaming { partial, .. }` is active, also commit a `partial`-snapshot to scrollback as a placeholder marked "_streaming_partial". Reset `stream_committed_chars` to `partial.len()` so subsequent chunk flushes pick up from the snapshot's end.

**Risk:** Medium — needs careful handling of the stream_committed_chars / stream_header_committed state. Test on tmux + alacritty resize.

---

### 9. `body[committed..]` byte-slice assumes `final_text == sum(stream chunks)` · **P2 · scrollback**

**Where:** `src/tui/app.rs:2112-2120` (finalize_turn).

**Symptom:** `stream_committed_chars` is a BYTE position into `state.partial` (the live streaming accumulator). At turn end, `body` is set from `final_text` (the agent's final reply). The code then slices `body[committed..]` to emit only the trailing tail. **If `final_text` differs from what was streamed** — e.g. providers that include thinking-tokens in the stream but exclude them from the final, or trim trailing whitespace — `committed` may land mid-UTF8-codepoint and panic at runtime, or just emit gibberish.

The defensive `.min(body.len())` at `:2112` covers the "final shorter than partial" case (committed clamps to body.len(), tail is empty — visually a no-op). But it doesn't cover "final has different content at the same byte offset".

**Fix scope:** ~10 LoC. Replace the byte slice with a chars-aware approach: count chars up to `committed`, slice on a char boundary, or just emit the full `body` and trust ratatui dedup. Simpler: when stream_header_committed AND the partial-vs-final byte length differs by more than a threshold, emit the full body fresh (acknowledging the duplicated scrollback as a one-time cost).

**Risk:** Low. The bug is latent — most providers emit identical stream/final. But the panic-on-mid-char case has zero recovery if it triggers.

---

### 10. Shift+Tab and Ctrl+G fire during setup_overlay / wizard · **P3 · key handling**

**Where:**
- `KeyCode::BackTab` at `app.rs:863` has no guard. Comment claims modal guards above already returned, but list_picker / info_panel are the only ones swallowed; setup_overlay and first_run_wizard are not gated until later arms. So Shift+Tab during the wizard cycles autonomy preset.
- `KeyCode::Char('g') + Ctrl` at `:879` has no guard either. Ctrl+G during wizard opens the external editor on whatever is in `input_buffer` (likely empty during wizard).

**Symptom:** User in the middle of provisioning a provider via `/setup provider` accidentally hits Shift+Tab → autonomy preset cycles silently. Or hits Ctrl+G → external editor launches, suspending the wizard mid-flow.

**Fix scope:** ~4 LoC. Add `&& self.setup_overlay.is_none() && self.first_run_wizard.is_none()` to both arm guards.

**Risk:** Zero — both keys become no-ops in the modal contexts where they shouldn't have fired anyway.

---

### 11. Dead duplicate `render_status` / `render_input` methods · **P3 · tech debt**

**Where:** `src/tui/app.rs:3498` (`render_input`) and `:3567` (`render_status`). Both are `&self` methods on `TuiApp`. Grep confirms zero callers. The actually-used renderers are the free fns `render_input_pane` (`:4099`) and `render_status_pane` (`:4243`).

**Symptom:** ~150 LoC of dead code accumulates clippy warnings (`unused`, `dead_code` if flagged) and confuses readers trying to follow the render path.

**Fix scope:** Delete both methods. ~150 LoC removal. Or apply `#[allow(dead_code)]` if they're aspirational for future use.

**Risk:** Zero — confirmed via grep. Should be a separate cleanup PR per CLAUDE.md "one concern per PR".

---

## Recommendations for v0.6.58-alpha bundle

If the goal is to ship a meaningful TUI UX polish release on top of the cursor/last_error fixes already on main, I'd prioritize this slice (all P1, ~80 LoC total):

1. **Finding 1** — bracketed paste (high user value, low risk)
2. **Finding 4(a)** — change "esc" hint to "ctrl+c" (1 line, no debate)
3. **Finding 2** — show queued_turns in status bar (additive, low risk)
4. **Finding 10** — guard Shift+Tab and Ctrl+G during modals (4-line patch, prevents accidental triggers)

That's a coherent "v0.6.58-alpha: TUI input + feedback polish" story without scope creep.

**Defer to v0.6.59 or later:**
- Finding 3 (`context_window`) — needs product call on whether to wire vs delete
- Findings 5, 7 — approval lifecycle deserves its own focused PR
- Findings 6, 8, 9 — Unicode width, resize-during-stream, body-slice — edge cases that benefit from dedicated test coverage
- Finding 11 — pure cleanup, separate PR

## Out of scope for this audit

- Agent-loop, gateway, providers, channels, MCP, RAG — not user-facing TUI surfaces
- Performance profiling (audit was read-only, no instrumentation)
- Test coverage gap analysis
- Cross-terminal compatibility matrix
