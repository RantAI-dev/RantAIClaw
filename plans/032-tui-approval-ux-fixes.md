# Plan 032: TUI approval-UX honesty + consistency fixes (chip label, `/deny` turn-cancel, stale no-timeout comment, inert last-reason)

> **Context**: A deepscan (2026-07-23) of the TUI tool-approval UX — the amber
> "Approval needed" pane, the Y/A/N/Esc keys, the `/allow` `/deny` `/allowlist`
> `/autonomy` slash commands, and the shell-tool cascade that drives them —
> found the flow **functionally correct** (deny fails closed, session/persist
> allowlist works, cascade walks the chain, structural rejections skip the
> prompt). What it surfaced are four **honesty / consistency** gaps where a
> label, a comment, or a second entry-point disagrees with the real behavior.
> None is a security hole; all are UX-truth or dead-nuance cleanups.
>
> The cascade engine itself is now covered by two new tests added alongside
> this deepscan (`shell_cascading_approval_prompts_for_each_distinct_blocked_basename`,
> `shell_cascading_approval_deny_midway_returns_error_and_keeps_prior_grant`
> in `src/tools/shell.rs`) — this plan does NOT re-cover that; it fixes the
> label/comment/entry-point gaps.
>
> **Executor note**: Self-contained. Repo verification baseline —
> `cargo fmt --all -- --check` · `cargo clippy --all-targets -- -D warnings` ·
> `cargo test`. Disk-constrained box: prefer a shared warm `CARGO_TARGET_DIR`
> + `cargo test --lib` + `touch`-ing changed files (see the repo disk memo).
> strict-clippy-delta (PEDANTIC on changed lines) + setup_e2e run POST-merge —
> reproduce scoped `clippy --all-targets` locally before merging. The TUI code
> is behind the `tui` feature; build/test with it enabled.
>
> **Branch**: `fix/tui-approval-ux` (non-`main`). One commit per task.
> **Risk**: C1/C3/C4 LOW (label + comment + dead-code). C2 MEDIUM (a real
> behavior change to `/deny`). No exposure-boundary change; NO config-schema
> change → no schema-version bump.

## Baseline evidence (confirmed against main, 2026-07-23)

- **C1 — "yes once" chip is a misnomer; there is no `Once` semantics.**
  `render_approval_pane` renders `chip("Y", "yes once", …)`
  (`src/tools/../tui/app.rs:4903`), and the Y key maps to `Decision::Session`
  (`app.rs:773-776`). `resolve_pending_approval` then calls
  `add_runtime_command(&basename, false)` (`app.rs:569-581`) which adds the
  basename to the session runtime allowlist — so the command **never re-prompts
  for the rest of the session**, not "once". Worse, the shell-tool cascade
  treats `Decision::Once` and `Decision::Session` **identically** — both do
  `add_runtime_command(basename, false)` (`src/tools/shell.rs:346-352`). No code
  path anywhere grants a single-call-only approval. The label promises a
  semantics the runtime does not implement.

- **C2 — `/deny <base>` and the `N`/`Esc` key diverge on turn handling.**
  Pressing `N`/`Esc` on the pane routes to `resolve_pending_approval(Deny)`,
  which resolves the request AND fires `TurnRequest::Cancel` to stop the whole
  turn (`app.rs:591-603`) — the deliberate "user said no → don't let the LLM
  explore alternative commands" UX. The `/deny` slash command
  (`src/tui/commands/allowlist.rs:112-140`) only calls
  `resolve_by_basename(Deny)`; it does **not** cancel the turn, so the shell
  tool returns the "not allowed" error to the LLM which typically retries with a
  variant command (each re-hitting the gate). Two "deny" surfaces, two
  behaviors; the design intent (documented in the `resolve_pending_approval`
  doc-comment, `app.rs:534-546`) is the cancel-the-turn one.

- **C3 — "Older ones still auto-deny on timeout" comment is false for the TUI.**
  `app.rs:1780-1785` keeps only the newest pending request in
  `self.pending_approval` and comments that older ones "still auto-deny on
  timeout". But the agent/TUI registry is `PendingApprovals::default()`
  (`src/agent/agent.rs:390`), whose `Default` is `new(None)` = **no timeout**
  (`src/security/pending.rs:233-245`). So when two tool calls block with
  distinct basenames, only the newest shows in the amber box; the older ones
  wait **indefinitely**, visible only via `/allowlist`, with no auto-deny. The
  comment misdescribes the behavior. (Concurrent blockers are reachable when a
  second request arrives while one is already pending — the "replace newest"
  path at `app.rs:1785`.)

- **C4 — `last_reason` in the cascade is inert (dead nuance + misleading comment).**
  The cascade's deny arm returns `last_reason.unwrap_or(reason)`
  (`src/tools/shell.rs:366-376`) with a comment claiming it "keep[s] the LAST
  error … the blocker the user just rejected — not the very first one". But the
  allowlist-rejection error is `"Command not allowed by security policy:
  {command}"` embedding the **full command string** (`src/security/policy.rs:577`),
  which is identical on every cascade iteration (same `command`). So
  `last_reason` (set to the prior iteration's `reason`) and the fallback
  `reason` are byte-identical — the branch can never produce a different message.
  The machinery + comment imply a per-blocker distinction that cannot occur.

## Tasks

### Task 1 (C1) — Make the Y chip tell the truth

Pick ONE (executor decides with maintainer; Option A is the low-risk default):

- **Option A (relabel — recommended, LOW):** change the chip so it names the
  real scope. In `render_approval_pane` (`app.rs:~4903`):
  `chip("Y", "yes (session)", …)`. Keep `[A] always (persist)` as-is. Optionally
  align the doc-comment mock (`app.rs:4853-4857`). No behavior change.
- **Option B (implement real Once — MEDIUM, YAGNI-check first):** make Y map to
  `Decision::Once` and give `Once` true single-call semantics (resolve without
  `add_runtime_command`) in both `resolve_pending_approval` (`app.rs:569`) and
  the shell cascade (`shell.rs:346`). Only do this if a "just this one call,
  ask again next time" mode is actually wanted — per §3.2 YAGNI, do not add it
  speculatively. If chosen, the cascade's `Once` arm must still `continue`
  (re-validate) but NOT persist to the allowlist, which means the SAME basename
  later in the same chain would re-prompt — confirm that is the desired UX.

**Verify**: `cargo test --lib --features tui` (label change is render-only; add
a `composer`/render assertion only if a cheap one fits — otherwise visual).

### Task 2 (C2) — Make `/deny` cancel the turn like the `N` key (or document the split)

- **Preferred (align behavior):** route `/deny` through the same cancel path.
  The clean seam: have `DenyCommand` return a `CommandResult` variant that the
  app translates into `TurnRequest::Cancel` (mirror the existing
  `CmdResult::CancelTurn` arm in `handle_command`, `app.rs:~2660`), in addition
  to resolving the pending request as `Deny`. Do the `resolve_by_basename(Deny)`
  first (so the blocked shell call unblocks with a deny), THEN cancel.
- **Alternative (document only, if the split is intentional):** leave behavior
  but update the `/deny` help/description and the `resolve_pending_approval`
  doc-comment to state that `/deny` fails the single call while `N`/`Esc`
  cancels the turn.

**Repro test (behavior option):** an app-level test that, with a pending
approval open, dispatching `/deny <base>` both resolves the request AND requests
turn cancellation (assert a `TurnRequest::Cancel` is sent). If an app-level
harness is too heavy, add a `CommandResult`-level assertion that `/deny` yields
the cancel-signalling variant.

### Task 3 (C3) — Correct the stale comment; decide whether to surface the queue

- **Minimum (LOW):** fix `app.rs:1780-1785` — replace "still auto-deny on
  timeout" with the truth: the TUI registry has no timeout, so older pending
  requests wait until explicitly resolved and are listed by `/allowlist`.
- **Optional (MEDIUM, only if wanted):** show a "+N more pending" hint on the
  approval pane / status bar when `pending().list().len() > 1`, so a stranded
  older request is discoverable without typing `/allowlist`. YAGNI-gate this;
  the comment fix alone closes the correctness gap.

**Verify**: comment-only → `cargo fmt` + scoped `clippy`. If the hint is added,
a render assertion on the multi-pending case.

### Task 4 (C4) — Remove the inert `last_reason` machinery

Simplify the cascade deny arm (`shell.rs:310-376`): drop the `last_reason`
local and return the current `reason` directly on `Decision::Deny`. Behavior is
unchanged (the strings are identical), but the code stops implying a
distinction that can't happen, and the comment is deleted or corrected. KISS.

**Repro/guard**: the two cascade tests added in the deepscan already assert the
deny path returns a "not allowed" error; keep them green. Add nothing new.

## Non-goals

- No change to the deny-fails-closed security posture, the allowlist gate order,
  or any exposure boundary.
- No new config keys / schema version bump.
- Not re-testing the cascade engine (already covered by the two new shell tests).

## Risk & rollback

- C1/C3/C4: label + comment + dead-local removal — trivially revertible, one
  commit each.
- C2: the only behavior change. Rollback = revert the single `/deny` commit;
  the `N`/`Esc` path is untouched, so the safe deny surface always remains.
