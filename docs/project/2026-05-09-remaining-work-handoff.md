# rantaiclaw — remaining-work handoff plan

**Date:** 2026-05-09 (supersedes the partial plan in `2026-05-09-next-drops-plan.md`).
**Status anchor:** branch `feat/services-searxng-auto-launch` at commit `1e0bddc`, version `0.6.30-alpha`, pushed to `origin`.
**Goal:** every remaining work item — v0.6.30 PR, v0.6.31 cluster, P3/P4 backlog, anti-patterns — captured in one self-contained plan an agent can execute top-to-bottom without chat context.

Companion docs (read alongside):
- `docs/project/2026-05-09-skills-runtime-gap-tracker.md` — the row-by-row status board. Update it as items land.
- `docs/project/2026-05-09-next-drops-plan.md` — the earlier plan covering v0.6.30→v0.6.31 conventions. Operating conventions (build target, validation, ship sequence, anti-patterns) still apply; this plan doesn't restate them — go read that one's "Operating conventions" section first.

---

## TL;DR — what's left

| Tier | Cluster | Effort | Status |
|---|---|---|---|
| **T0** | Open the v0.6.30 PR | 5 min | not started |
| **T1.1** | Watcher reload overwrites install-deps error title (nit) | XS (1h) | not started |
| **T1.2** | RT-02 — SSE streaming on `/api/v1/agent/chat` | M (1 day) | not started |
| **T2** | P3 backlog (do **only** when signaled) | varies | parked |
| **T3** | P4 / permanently-skipped (do **not** start) | n/a | parked |

T1.1 and T1.2 ship together as **v0.6.31-alpha** in one drop.
T2 items wait for explicit user signal.
T3 items are never picked up unless the product direction shifts.

---

## T0 — Open the v0.6.30 PR

The v0.6.30 cluster is committed on the branch. PR isn't open yet.

```bash
cd /home/shiro/rantai/RantAI-Agents/packages/rantaiclaw
gh pr create \
  --base main \
  --head feat/services-searxng-auto-launch \
  --title "feat(skills): OpenClaw skill-system parity — v0.6.21 → v0.6.30" \
  --body "$(cat <<'BODY'
## Summary
- Closes the OpenClaw + ClawHub skill-system feature gap (metadata, config storage, execution-layer wiring, install-recipe runner, CLI/API/TUI surfaces).
- Lands the control-plane HTTP API (`/api/v1/*`).
- Persona / sessions / install-deps now work end-to-end across CLI, TUI, and API surfaces.
- 8 of 8 actionable bugs from the v0.6.21 deep test report closed.

See `docs/project/2026-05-09-skills-runtime-gap-tracker.md` for the row-by-row scorecard.

## Notable additions
- 18 new \`/api/v1/*\` endpoints (bearer-authed via existing \`PairingGuard\`)
- \`metadata.clawdbot.{requires,install}\` parsing and gating
- \`[skills.entries.<n>]\` per-skill TOML config block (OpenClaw shape)
- Per-skill env / api_key / config injected into shell exec
- \`skills install-deps\` recipe runner (brew/uv/npm/go/download)
- \`/<skill-name>\` direct invoke + autocomplete
- Hot-reload via notify-based file watcher
- TUI install spinner + auto-jump to /skills on success
- tmux-based smoke harness in \`dev/tui-smoke.sh\` + \`dev/ci.sh tui-smoke\`
- 7 new unit tests (env composition, openclaw.json port, install_deps selector, skill watcher)

## Test plan
- [ ] Reviewers confirm CI green
- [ ] \`cargo test --target x86_64-unknown-linux-musl --lib skills\` passes locally
- [ ] \`bash dev/tui-smoke.sh\` passes locally
- [ ] Manual TUI smoke: \`/skills\` shows gated rows · \`Ctrl+I\` fires install-deps
- [ ] Live: \`agent -m\` writes to sessions.db · \`personality set\` affects agent voice

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

**If `gh` isn't installed** or the user prefers GitHub UI: visit
`https://github.com/RantAI-dev/RantAIClaw/pull/new/feat/services-searxng-auto-launch`
and paste the same body.

**Done when:** PR URL is recorded; CI green or yellow with known-OK reasons documented.

---

## T1 — v0.6.31 cluster

Two changes shipped together. Bump to `0.6.31-alpha`. ~1 day.

### T1.1 — Fix watcher-reload overwriting install-deps error title (XS)

**Problem (caught during v0.6.30 review):**
After `Ctrl+I`/`Tab` triggers install-deps in `/skills`, the picker title flips through:

```
T+0.00s:  Skills · reloaded
T+0.05s:  ⠙  Installing deps for gog…
T+0.30s:  Skills · install-deps failed: <error>   ← user can't see this
T+0.35s:  Skills · reloaded                       ← watcher debounce overwrites
```

The `notify`-based skill watcher fires on *any* file change in the skills dir, including the partial writes that happen during a failed install-deps recipe. Its reload handler unconditionally sets `picker.title = "Skills · reloaded"`, clobbering the install-deps error message ~50ms after it appears.

**Fix:**
Guard the watcher's title-flip with a check against `skill_deps_install_in_progress`. While an install-deps job is either in flight or has just completed (within a short cooldown — say 3 seconds), the watcher reload silently refreshes `ctx.available_skills*` *without* touching `picker.title`. The install-deps tick handler retains exclusive control of the title.

**Files:**
- `src/tui/app.rs` — `drain_skill_reload_events` (or whatever the watcher-tick handler is named)

**Implementation outline:**

1. Add a `skill_deps_install_finished_at: Option<std::time::Instant>` field on `TuiApp`. Set it inside `tick_skill_deps_install` whenever a completion (Ok or Err) lands.

2. In the watcher reload handler:

   ```rust
   fn drain_skill_reload_events(&mut self) {
       let mut should_reload = false;
       if let Some(watcher) = self.skills_watcher.as_mut() {
           while watcher.reload_rx.try_recv().is_ok() {
               should_reload = true;
           }
       }
       if !should_reload { return; }

       self.refresh_available_skills();

       // Suppress the "Skills · reloaded" title flip if an install-deps
       // job is in flight or just finished — its title carries the
       // outcome message users need to see. The cooldown handles the
       // ~50ms race between completion and the trailing watcher event.
       let suppress_title = self.skill_deps_install_in_progress.is_some()
           || self.skill_deps_install_finished_at
               .map(|t| t.elapsed() < std::time::Duration::from_secs(3))
               .unwrap_or(false);
       if suppress_title {
           // Still refresh the picker items so the new state is reflected.
           if let Some(p) = self.list_picker.as_mut() {
               if p.kind == crate::tui::widgets::ListPickerKind::Skill {
                   p.set_items(self.skill_picker_items());
               }
           }
           return;
       }

       if let Some(p) = self.list_picker.as_mut() {
           if p.kind == crate::tui::widgets::ListPickerKind::Skill {
               p.set_items(self.skill_picker_items());
               p.title = "Skills · reloaded".to_string();
           }
       }
   }
   ```

3. In `tick_skill_deps_install`, set `self.skill_deps_install_finished_at = Some(Instant::now())` in both the Ok and Err completion branches.

**Verification (tmux harness):**

```bash
RANTAICLAW_BIN=$(pwd)/target/x86_64-unknown-linux-musl/release/rantaiclaw \
  bash dev/tui-smoke.sh
```

Manual:
```
$ /tmp/tui_drive.sh rctui start
$ /tmp/tui_drive.sh rctui send "/skills" Enter
$ /tmp/tui_drive.sh rctui send "gog"          # filter to gated row
$ /tmp/tui_drive.sh rctui send "Tab"          # fire install-deps
# Wait 1s
$ /tmp/tui_drive.sh rctui capture | head -2
# Should see "Skills · install-deps failed: …" or "Skills · still missing …"
# NOT "Skills · reloaded"
```

**Done when:** the install-deps error/success title persists for at least 3s after completion.

---

### T1.2 — RT-02 SSE streaming on `/api/v1/agent/chat` (M, ~1 day)

The previous plan (`2026-05-09-next-drops-plan.md` § "v0.6.31") has the full implementation outline for this. Highlights:

- Split `agent_chat` into `agent_chat_sync` (current behavior, no change) + `agent_chat_stream` (new SSE path)
- Dispatcher picks based on `Accept: text/event-stream` header **or** `?stream=1` query param
- Use `axum::response::sse::{Sse, Event, KeepAlive}` and `async-stream` (already a dependency)
- Spawn the agent turn with `mpsc::Sender<AgentEvent>`; adapt events into SSE `Event::default().data(json.to_string())`
- Event types: `chunk` / `usage` / `error` / `done`
- Buffer chunks in memory; on `Done`, call `record_api_chat_session` with the accumulated final_text

**One additional spec point not in the previous plan:**
**Cancellation propagation.** SSE clients can disconnect mid-stream (browser tab close, network drop). The handler should hold a `CancellationToken`, register a drop guard on the `Sse` stream that cancels the token. The agent honours `cancel` already (`turn_streaming` second param); session persistence should NOT fire on cancelled streams (matches CLI/TUI behavior). Mirror via:

```rust
let cancel = CancellationToken::new();
let cancel_for_drop = cancel.clone();
let stream = async_stream::stream! {
    // … emit events …
};
// Wrap stream so dropping it cancels the agent.
let stream = stream.chain(futures::stream::once(async move {
    cancel_for_drop.cancel();
    Ok(Event::default().comment("end"))
}));
```

**Tests:**

1. **Unit test** `gateway::api_v1::tests::sse_chat_emits_chunk_then_done`:
   - Mock `Provider` that returns predictable chunks
   - Boot gateway in test mode, call `/api/v1/agent/chat` with `Accept: text/event-stream`
   - Parse SSE response, assert: at least one `chunk` event arrives, ending with one `done` event whose `text` matches the assembled chunks
   - Verify a row landed in the test sessions.db with `source = "api"`

2. **Live curl** (manual, in commit body):
   ```bash
   curl -N -X POST http://127.0.0.1:9091/api/v1/agent/chat \
     -H 'Authorization: Bearer <token>' \
     -H 'Accept: text/event-stream' \
     -H 'Content-Type: application/json' \
     -d '{"message":"Count to 5 slowly, one number per line."}'
   # → data: {"type":"chunk","text":"1\n"}
   # → data: {"type":"chunk","text":"2\n"}
   # … 
   # → data: {"type":"done","text":"1\n2\n3\n4\n5\n","cancelled":false}
   ```

3. **Backwards-compat check**: same endpoint with no `Accept` header returns the old sync JSON shape — no client breakage.

**Doc update:** add `docs/api-v1-streaming.md` (new) covering the SSE event schema. Update `docs/commands-reference.md` if it mentions `/api/v1/agent/chat` to note the new mode.

**Done when:**
- Unit test passes (`cargo test --target x86_64-unknown-linux-musl --lib gateway::api_v1::tests::sse_chat_emits_chunk_then_done`)
- Live curl shows incremental events
- Sync path unchanged for non-SSE clients
- `record_api_chat_session` only fires on `Done { cancelled: false }`
- Tracker row RT-02 flipped to ✅

---

### v0.6.31 ship checklist

- [ ] T1.1 + T1.2 implemented
- [ ] `cargo check --target x86_64-unknown-linux-musl` clean
- [ ] All new unit tests pass via narrow `cargo test --lib <module>`
- [ ] `bash dev/tui-smoke.sh` passes
- [ ] tmux manual: install-deps error title persists ≥3s
- [ ] Live curl: SSE stream emits chunks → done; sync path unchanged
- [ ] `Cargo.toml` → `0.6.31-alpha`
- [ ] Tarball + sha256 in `~/rantaiclaw-alpha-build/v0.6.31-alpha-rantaiclaw-x86_64-unknown-linux-musl.tar.gz`
- [ ] Tracker rows for the watcher nit + RT-02 flipped to ✅
- [ ] Single commit per the convention in `2026-05-09-next-drops-plan.md` § "Ship sequence"
- [ ] Push to `origin`; if PR is already open, the new commit lands on it automatically

---

## T2 — Deferred backlog (do **only** when signaled)

These are tracked in the gap tracker as P3/P4. **Do not pick them up speculatively.** Each row below has the **trigger condition** that should make the next agent reach for it.

| ID | Effort | Trigger condition (real user signal) |
|---|---|---|
| **SK-04** `skills.install.preferBrew` / `nodeManager` config keys | XS (1h) | A user reports "rantaiclaw chose npm but I want pnpm" or vice versa. Until then, the hardcoded preference order (brew → uv → npm → go → download) is fine. |
| **SK-06** Multi-source skill location precedence (workspace → project-agent → personal-agent → managed → bundled → extraDirs) | M (1 day) | A multi-machine or multi-workspace user reports skills shadowing the wrong way. Until then, our 3 lookup spots cover everyone running a single profile. |
| **SK-07** Per-agent skill allowlists (`agents.*.skills = [...]`) | L (>1 day) | Multi-agent (`agents.*` config) ships as a top-level concept. SK-07 is meaningless without it. |
| **SK-08** `migrate codex` — Codex CLI skills import | M (½–1 day) | A Codex CLI user explicitly asks to migrate their skill folders. Niche. |
| **HE-02** Phase 2 — `skills config <slug> --set k=v` interactive editor + `/skills config` TUI | L (1.5 days) | A user complains about hand-editing `[skills.entries.<n>]` TOML blocks. File editing covers it today; only ship if the friction surfaces. |

**Push-back script** if the user-handing-off agent gets nerd-sniped into one of these:
> "<ID> is parked per the handoff plan with the trigger condition: <quote condition>. I haven't seen that signal — should I pick it up anyway, or stay on the v0.6.31 cluster?"

If the user confirms a signal exists, then:
1. Move the row out of T2 into a T1.x slot in this plan
2. Update the gap tracker with effort + new priority
3. Bump to a fresh `0.6.X-alpha` for that drop
4. Ship per the standard convention

---

## T3 — Permanently skipped (do **not** start)

These have explicit `⊘ Skip` verdicts in the gap tracker. Listed here so the agent has a single place to reference when pushing back.

| ID | Why we don't do this |
|---|---|
| **HE-01** Declarative `config_settings` typed schema in skill manifest | Different ecosystem. ClawHub skills don't ship this block; we'd be parsing a field nothing produces. |
| **HE-03** Skill Workshop / auto-create skills from agent work | Requires deep agent-loop introspection hooks. Hermes' core differentiator and a different product direction from rantaiclaw. |
| **HE-04** Self-improvement learning loop | Same as HE-03. |
| **HE-05** `skills snapshot export` / share | `cp -r ~/.rantaiclaw/profiles/<name>/skills` and `git init` cover it. A bespoke JSON export is a parallel format we'd have to keep in sync. |
| **HE-06** `skills publish` / push to a tap | `git push` covers it. Wrapping it adds an auth/credentials layer rantaiclaw doesn't have today. |
| **HE-07** Multi-registry `tap add owner/repo` | ClawHub is the registry. A second one doesn't exist worth supporting. |
| **Local malware scanner** | ClawHub runs LLM + static + VirusTotal scans server-side; results surface via `skills inspect`. Local scanning means binary bloat for redundant work. |

If asked to work on any of these:
> "<ID> is permanently out of scope per the handoff plan because <quote reason>. If the product direction has shifted, please update the gap tracker first; otherwise I'll stay on the active queue."

---

## Maintenance — keeping this plan and the tracker honest

The tracker (`2026-05-09-skills-runtime-gap-tracker.md`) is the **single source of truth** for what's done. This plan describes **how to do what's left**. When status changes:

1. **Implementing a row:** update tracker row to ✅ with date in the same commit that lands the code. Do NOT update both files in separate commits — they stay synced.
2. **Discovering new gaps:** add a row to the tracker with a fresh ID (next number in the relevant prefix: SK-09, RT-04, GV-04, …). If it needs implementation guidance, append a section to *this* plan in the appropriate tier.
3. **Promoting a P3/P4 to active:** move the T2 row up to a T1.x slot here, set new effort estimate, bump tracker priority.
4. **Demoting / killing a row:** move it to T3 with the `⊘ Skip` reason, OR delete from the tracker entirely if it's truly N/A.

The plan is editable in place — same date is fine for same-day updates. If the situation materially changes (e.g. v0.6.32 ships and a new cluster of work surfaces), date-stamp a new file and link the old one as `superseded-by`.

---

## Quick reference — ready-to-paste sequences

### Start v0.6.31

```bash
cd /home/shiro/rantai/RantAI-Agents/packages/rantaiclaw
git pull --ff-only origin feat/services-searxng-auto-launch
sed -i 's/version = "0.6.30-alpha"/version = "0.6.31-alpha"/' Cargo.toml
# … implement T1.1 + T1.2 …
cargo check --target x86_64-unknown-linux-musl
cargo test --target x86_64-unknown-linux-musl --lib gateway::api_v1
cargo test --target x86_64-unknown-linux-musl --lib skills::watcher
bash dev/tui-smoke.sh
cargo build --release --target x86_64-unknown-linux-musl
# tarball + commit per 2026-05-09-next-drops-plan.md "Ship sequence"
git push origin feat/services-searxng-auto-launch
```

### Verify a TUI behavior end-to-end

The harness lives in `dev/tui-smoke.sh`. For ad-hoc tmux drives during development, the `tui_drive.sh` pattern from earlier sessions still works:

```bash
cat > /tmp/tui_drive.sh << 'EOF'
#!/bin/bash
SESSION="$1"; shift
case "$1" in
  start)
    tmux kill-session -t $SESSION 2>/dev/null
    tmux new-session -d -s $SESSION -x 200 -y 60
    sleep 0.3
    tmux send-keys -t $SESSION "set -a; source /home/shiro/rantai/RantAI-Agents/packages/rantaiclaw/.env; set +a" Enter
    tmux send-keys -t $SESSION "export RANTAICLAW_CONFIG_DIR=/home/shiro/.rantaiclaw/profiles/minimax-test" Enter
    tmux send-keys -t $SESSION "/home/shiro/rantai/RantAI-Agents/packages/rantaiclaw/target/x86_64-unknown-linux-musl/release/rantaiclaw" Enter
    sleep 4 ;;
  send) shift; tmux send-keys -t $SESSION "$@" ;;
  capture) tmux capture-pane -t $SESSION -p ;;
  stop) tmux send-keys -t $SESSION 'C-c'; sleep 0.5; tmux kill-session -t $SESSION 2>/dev/null ;;
esac
EOF
chmod +x /tmp/tui_drive.sh
```

### Live LLM round-trip via API (after v0.6.31 SSE)

```bash
# Sync path (backwards compat):
curl -s -X POST http://127.0.0.1:9091/api/v1/agent/chat \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"message":"hello"}'

# Stream path:
curl -N -X POST http://127.0.0.1:9091/api/v1/agent/chat \
  -H "Authorization: Bearer $TOKEN" \
  -H "Accept: text/event-stream" \
  -H "Content-Type: application/json" \
  -d '{"message":"hello"}'
```

---

## Hand-off contract

When the next agent picks this up, they should:

1. **Read this plan top-to-bottom** before touching any code.
2. **Read the tracker** to confirm current state.
3. **Read `docs/project/2026-05-09-next-drops-plan.md` § "Operating conventions"** for the build/test/ship rhythm.
4. **Execute T0 (open PR) immediately**, regardless of which feature work they're queued up for.
5. **Then start T1.1 + T1.2** as v0.6.31 cluster.
6. **Decline T2/T3 work** unless the user explicitly waves the flag.
7. **Update the tracker** as the single act that marks a row done — not "done in chat" or "done in commit message" alone.

That's it. The loop closes when v0.6.31 ships and the tracker has zero P1 rows open.
