# rantaiclaw — next-drops execution plan (handoff)

Concrete, agent-executable plan for v0.6.30 → v0.6.31. Self-contained:
all file paths, function names, expected behavior, tests, and ship
conventions are spelled out so an agent can work top-to-bottom without
needing the chat history.

**Repo root:** `/home/shiro/rantai/RantAI-Agents/packages/rantaiclaw`
**Branch:** `feat/services-searxng-auto-launch`
**Active version:** `0.6.29-alpha` → bump to `0.6.30-alpha` for first cluster.
**Companion gap tracker:** `docs/project/2026-05-09-skills-runtime-gap-tracker.md` — update row status as items land.

---

## Operating conventions (read first)

These match the v0.6.20 → v0.6.29 ship rhythm. Don't deviate without good reason.

### Build target

- **Always** build `--target x86_64-unknown-linux-musl --release`. Static musl binary is the canonical artifact; non-musl builds aren't shipped.

### Validation per change

- `cargo check --target x86_64-unknown-linux-musl` — must be clean (no errors)
- `cargo test --target x86_64-unknown-linux-musl --lib <module-path>` — narrow run, NOT full suite (full suite is slow and the user's machine is constrained)
- For TUI changes: `bash dev/tui-smoke.sh` to verify rendering end-to-end via tmux
- Live test against real MiniMax when the change touches the agent loop. The key is in `.env`; `set -a; source .env; set +a` to load.

### Ship sequence per drop

1. Bump `Cargo.toml` `version = "0.6.X-alpha"`
2. `cargo build --release --target x86_64-unknown-linux-musl`
3. Tarball:
   ```bash
   cd /tmp && rm -rf v0.6.X-alpha-rantaiclaw-x86_64-unknown-linux-musl
   mkdir v0.6.X-alpha-rantaiclaw-x86_64-unknown-linux-musl
   cp <repo>/target/x86_64-unknown-linux-musl/release/rantaiclaw \
      v0.6.X-alpha-rantaiclaw-x86_64-unknown-linux-musl/
   tar czf ~/rantaiclaw-alpha-build/v0.6.X-alpha-rantaiclaw-x86_64-unknown-linux-musl.tar.gz \
      v0.6.X-alpha-rantaiclaw-x86_64-unknown-linux-musl
   sha256sum ~/rantaiclaw-alpha-build/v0.6.X-alpha-rantaiclaw-x86_64-unknown-linux-musl.tar.gz
   ```
4. Update `docs/project/2026-05-09-skills-runtime-gap-tracker.md` — flip row to ✅ with date.
5. Commit message convention (one logical drop per commit):
   ```
   feat(<scope>): <one-line summary> — bump v0.6.X-alpha

   <2–4 line context: what was the gap, why now>

   ## What landed
   - <bullets>

   ## Verification
   - <how it was tested>

   ## Trade-offs / deferred
   - <any compromises>
   ```

### Anti-patterns to avoid (from prior session feedback)

- **Don't run the full `cargo test` suite** — only run the modules you touched. Full suite freezes the test machine.
- **Don't auto-add features** — if a "while you're there" temptation surfaces, write it in the gap tracker for a later drop.
- **Don't skip the tmux smoke** for TUI work — `dev/tui-smoke.sh` exists; use it.
- **Don't break security defaults** — never lower `autonomy=full` or remove approval gates. If a feature needs more permission, document it in the commit body.

---

## v0.6.30 — finish SK-02, add SK-03, gate CI on tui-smoke

Single drop covering 3 items. Estimated 1 day total. Order matters: SK-02 first (smallest, unblocks the rest), then SK-03, then GV-03.

### Task 1 — SK-02: render gated skills in `/skills` picker (1h)

**Problem:** `Ctrl+I`/`Tab` install-deps hotkey was wired in v0.6.29 but is unreachable on gated skills, because `/skills` picker reads `ctx.available_skills` which is `load_skills_with_config`-filtered (gated skills excluded). The hotkey only fires on already-active skills — exactly the ones that don't need install-deps.

**Fix:** the Skill picker should show *all* skills (gated and active), with gated rows visually marked. The CLI's `skills list` already does this via `load_skills_with_status`; mirror it in the TUI.

**Files:**
- `src/tui/commands/skills.rs::SkillsCommand::execute` (and `SkillCommand::execute` for parity)
- `src/tui/context.rs` — add `available_skills_with_status: Vec<(Skill, Vec<String>)>` field
- `src/tui/app.rs` — populate the new field at startup + on hot-reload (sites that currently set `ctx.available_skills`)

**Implementation outline:**

1. Add `available_skills_with_status: Vec<(crate::skills::Skill, Vec<String>)>` to `TuiContext` next to `available_skills`. Reasons vec is empty when active.

2. Wherever `ctx.available_skills = load_skills_with_config(...)` runs (3 sites in `app.rs`: `init`, hot-reload after install, hot-reload after install-deps), also call `load_skills_with_status` and store both. The active-only `available_skills` is still needed for the system prompt and `/<skill>` slash invoke.

3. In `commands/skills.rs::build_skill_items`, accept the with-status vec and render gated rows with a `✗ ` prefix in `primary` and `gated: <reason>` appended to `secondary`. Active rows stay unchanged.

4. Update `SkillsCommand::execute` and `SkillCommand::execute` to pass the with-status vec.

**Test:**
- Add a TUI test in `dev/tui-smoke.sh` that opens `/skills`, types `gog` (or any skill known to be gated by `requires.bins`), and asserts the picker shows the row (not "No matches for 'gog'").
- Manual tmux verification: `Ctrl+I` on the gated row should fire the install-deps spawn (look for the title flip to "Skills · running install-deps for gog…").

**Done when:** `dev/tui-smoke.sh` passes and tmux capture shows gated skills in `/skills`.

---

### Task 2 — SK-03: skill file watcher (½ day)

**Problem:** Edit a SKILL.md while rantaiclaw is running → no reload. `ctx.available_skills` only refreshes on `skills install <slug>`. Skill authors and users editing local skills must restart.

**Fix:** background `notify` crate watcher on the profile's skills dir. On any file event in `<profile>/skills/**` or `<workspace>/skills/**`, debounce 500ms then call `load_skills_with_config` + `load_skills_with_status` and update `ctx.available_skills*`.

**Files:**
- Add `notify = { version = "8", default-features = false }` to `Cargo.toml` (binary-size discipline — no extra features). Verify `cargo build --release --target x86_64-unknown-linux-musl` size delta is < 100KB.
- Create `src/skills/watcher.rs` with a `SkillsWatcher` that owns a `notify::RecommendedWatcher` and an mpsc channel for debounced reload events.
- Wire it in `src/tui/app.rs::run_tui` near the existing `available_skills` populate. Spawn a watcher task; have it poll the channel each `drain_events` tick and refresh skills if a reload event arrived.
- Mirror in `src/agent/loop_.rs::run` for the headless agent (single-message + interactive paths) — though for `agent -m` single-shot it's moot since we exit before any file change can happen. Just the TUI matters.

**Implementation outline:**

```rust
// src/skills/watcher.rs
pub struct SkillsWatcher {
    _watcher: notify::RecommendedWatcher,  // keep alive; raw events filtered into…
    pub reload_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl SkillsWatcher {
    pub fn watch(profile_skills: &Path, workspace_skills: &Path) -> anyhow::Result<Self> {
        let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
        let (debounced_tx, debounced_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res { let _ = raw_tx.send(ev); }
        })?;
        watcher.watch(profile_skills, RecursiveMode::Recursive)?;
        if workspace_skills != profile_skills && workspace_skills.exists() {
            watcher.watch(workspace_skills, RecursiveMode::Recursive)?;
        }
        // Debounce: collect raw events for 500ms, emit one reload signal.
        tokio::spawn(async move {
            loop {
                let Some(_first) = raw_rx.recv().await else { break };
                tokio::time::sleep(Duration::from_millis(500)).await;
                while raw_rx.try_recv().is_ok() {} // drain coalesce window
                let _ = debounced_tx.send(());
            }
        });
        Ok(Self { _watcher: watcher, reload_rx: debounced_rx })
    }
}
```

In `app.rs::drain_events`:

```rust
if let Some(w) = self.skills_watcher.as_mut() {
    while w.reload_rx.try_recv().is_ok() {
        self.context.available_skills = load_skills_with_config(...);
        self.context.available_skills_with_status = load_skills_with_status(...);
        // Optional: log "🔁 Skills reloaded" to scrollback so user sees it
    }
}
```

**Test:**
- `cargo test --target x86_64-unknown-linux-musl --lib skills::watcher` — unit test that creates a tempdir, instantiates the watcher, writes a SKILL.md into it, asserts the reload channel emits within ~1s.
- Manual TUI test: open `/skills`, in another terminal `touch <profile>/skills/<existing>/SKILL.md`, observe the picker count update or row description refresh on next render tick.

**Done when:** unit test passes; touching a SKILL.md while TUI is open visibly refreshes `/skills` without restart.

---

### Task 3 — GV-03: CI hook for `dev/tui-smoke.sh` (1h)

**Problem:** `dev/tui-smoke.sh` exists but isn't gated. Future TUI regressions can land without the smoke catching them.

**Fix:** add to `dev/ci.sh` (the canonical CI runner per CLAUDE.md). Mirror the existing pattern.

**Files:**
- `dev/ci.sh` — find existing test/check stages, add a `tui-smoke` stage. Should:
  - Skip gracefully if `tmux` isn't installed (warn but don't fail)
  - Build the debug binary first (the script auto-builds if missing, but explicit is better)
  - Run `bash dev/tui-smoke.sh` with `RANTAICLAW_BIN` pointing at the just-built binary

**Implementation:** likely a 5-line addition to `dev/ci.sh`. Read it first to match the existing style.

**Test:** run `./dev/ci.sh tui-smoke` (or whatever subcommand naming the file uses). Should pass on this machine.

**Done when:** `./dev/ci.sh all` includes the smoke and passes.

---

### v0.6.30 ship checklist

- [ ] Tasks 1-3 implemented + tests added
- [ ] `cargo check --target x86_64-unknown-linux-musl` clean
- [ ] `cargo test --target x86_64-unknown-linux-musl --lib skills::watcher` passes
- [ ] `bash dev/tui-smoke.sh` passes (extended with the gated-skill assertion from Task 1)
- [ ] tmux manual: `/skills` shows gog with `✗ gated: missing binary gog`
- [ ] tmux manual: `Ctrl+I` on the gated row fires install-deps (title flips)
- [ ] tmux manual: edit a SKILL.md externally → `/skills` reflects within ~1s
- [ ] `Cargo.toml` bumped to `0.6.30-alpha`
- [ ] Tarball + sha256 in `~/rantaiclaw-alpha-build/v0.6.30-alpha-rantaiclaw-x86_64-unknown-linux-musl.tar.gz`
- [ ] Gap tracker rows SK-02, SK-03, GV-03 flipped to ✅ with 2026-05-XX date
- [ ] Single commit landed with the convention above

---

## v0.6.31 — RT-02 SSE streaming on `/api/v1/agent/chat` (~1 day)

### Background

The agent already supports event streaming via `Agent::turn_streaming(message, events_tx, cancel)` — used by the TUI for the live word-by-word render. The gateway endpoint is currently sync: takes `{message}`, blocks 1.5–4s, returns `{text, model, provider, duration_ms}`. API consumers can't show partial responses.

### Goal

Convert `POST /api/v1/agent/chat` to an SSE (Server-Sent Events) endpoint that streams agent events as they arrive, then closes after `Done`. Keep the existing sync behavior available behind a query flag for backwards compatibility.

### Files

- `src/gateway/api_v1.rs::agent_chat` — split into:
  - `agent_chat_sync` (current behavior, no change)
  - `agent_chat_stream` (new, SSE)
  - dispatcher `agent_chat` that picks based on `Accept: text/event-stream` header OR `?stream=1` query param
- `src/agent/agent.rs` — `turn_streaming` exists; just thread an `mpsc::Sender<AgentEvent>` through

### Implementation outline

1. Add `axum::response::sse::{Sse, Event, KeepAlive}` to imports.

2. New handler:

```rust
async fn agent_chat_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChatRequestBody>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    if body.message.trim().is_empty() {
        return Err(err_400("message must not be empty"));
    }
    let mut config = state.config.lock().clone();
    if let Some(p) = body.provider { config.default_provider = Some(p); }
    if let Some(m) = body.model { config.default_model = Some(m); }
    if let Some(t) = body.temperature { config.default_temperature = t; }

    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(64);
    let cancel = CancellationToken::new();

    // Spawn the agent turn in the background; events stream to events_rx.
    tokio::spawn(async move {
        let mut agent = match Agent::from_config(&config) {
            Ok(a) => a,
            Err(_) => return,
        };
        let _ = agent.turn_streaming(&body.message, Some(events_tx.clone()), Some(cancel.clone())).await;
    });

    // Adapt the AgentEvent stream into SSE Events.
    let stream = async_stream::stream! {
        while let Some(ev) = events_rx.recv().await {
            let payload = match ev {
                AgentEvent::Chunk { text, .. } => json!({"type":"chunk","text":text}),
                AgentEvent::Usage { prompt, completion, total } => json!({"type":"usage","prompt":prompt,"completion":completion,"total":total}),
                AgentEvent::Error(e) => json!({"type":"error","message":e}),
                AgentEvent::Done { final_text, cancelled } => json!({"type":"done","text":final_text,"cancelled":cancelled}),
                _ => continue,
            };
            yield Ok::<_, std::convert::Infallible>(Event::default().data(payload.to_string()));
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
```

3. Dispatcher:

```rust
async fn agent_chat(...) -> impl IntoResponse {
    let wants_stream = headers.get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false)
        || query.get("stream") == Some(&"1");
    if wants_stream {
        agent_chat_stream(...).await.into_response()
    } else {
        agent_chat_sync(...).await.into_response()
    }
}
```

4. Persistence: still call `record_api_session` (RT-01 added it) — but now with the *streamed* final_text accumulated from `Chunk` events. Buffer chunks while streaming; on `Done`, write the session.

### Tests

- `cargo test --target x86_64-unknown-linux-musl --lib gateway::api_v1` — add a unit test that boots a gateway, hits `/api/v1/agent/chat` with `Accept: text/event-stream`, asserts at least one SSE event arrives (use a mock provider for determinism).
- Live test: with `MINIMAX_API_KEY` set:

  ```bash
  curl -N -X POST http://127.0.0.1:9091/api/v1/agent/chat \
    -H 'Authorization: Bearer <token>' \
    -H 'Accept: text/event-stream' \
    -H 'Content-Type: application/json' \
    -d '{"message":"Count to 5 slowly, one number per line."}'
  ```

  Should see `data: {"type":"chunk",...}` events arrive incrementally, ending with `data: {"type":"done",...}`.

- Verify the sync path still works (no `Accept: text/event-stream`, no `?stream=1`) — same response shape as v0.6.21.

### Trade-offs

- Reuses existing `AgentEvent` enum — if it gains variants, the SSE dispatcher's match needs updating
- `record_api_session` only persists on successful `Done`; cancelled streams don't write (acceptable — matches CLI/TUI behavior)
- No SSE retry logic — clients reconnecting are responsible for de-duping

### v0.6.31 ship checklist

- [ ] Sync path unchanged for clients without `Accept: text/event-stream`
- [ ] Stream path emits `chunk`/`usage`/`error`/`done` events
- [ ] Session persistence happens on stream completion
- [ ] Unit test for SSE plumbing
- [ ] Live curl test against MiniMax shows incremental output
- [ ] Doc update in `docs/commands-reference.md` or a new `docs/api-v1-streaming.md`
- [ ] `Cargo.toml` → `0.6.31-alpha`
- [ ] Tarball + sha256 in `~/rantaiclaw-alpha-build/`
- [ ] Tracker row RT-02 flipped to ✅
- [ ] Single commit, conventional message

---

## Deferred — do not work on without explicit user signal

These are tracked but **shouldn't be picked up** in the next two drops. They're either P3+ priority or explicitly deferred until a real user hits the friction.

| ID | Why deferred |
|---|---|
| **SK-04** `skills.install.preferBrew/nodeManager` | XS effort but no user has asked for non-default ordering yet. 1h whenever someone hits it. |
| **SK-06** multi-source skill location precedence | OpenClaw checks 6 locations; we check 3. Real users probably never hit it. |
| **SK-07** per-agent skill allowlists | Out of scope until multi-agent lands as a top-level feature. |
| **SK-08** `migrate codex` | Niche; defer until a Codex user shows up. |
| **HE-02** Phase 2 `skills config` editor | File-edit covers it. Revisit if friction surfaces. |
| **HE-01,03,04,05,06,07** Hermes-flavored items | Permanently skipped — different ecosystem. See gap tracker for reasoning. |

If asked to work on any of these, push back: "SK-XX is deferred per the plan. Unblock by giving me a real user who needs it, or shift it to P1 in the tracker."

---

## Cross-cutting reminders

- **Auto mode is on** — execute autonomously, don't ask for permission on routine choices. Course-correct on user feedback as it lands.
- **Sandbox guards** — the user's actions sandbox will block destructive moves (e.g. setting `autonomy=full`, copying secret keys). Don't fight them; document the limitation and move on.
- **MiniMax key** — in `.env` at repo root. Source it before any `agent -m` test: `set -a; source .env; set +a`.
- **Don't touch** the user's real `~/.rantaiclaw/active_workspace.toml` or `.secret_key` files. The smoke harness uses `mktemp -d` for a reason.
- **Update the gap tracker** at the start of each drop. Mark items in-progress, then ✅ on land. The tracker is load-bearing across sessions.

---

## Quick reference — ready to invoke

When you start v0.6.30:

```bash
cd /home/shiro/rantai/RantAI-Agents/packages/rantaiclaw
git status                                                    # confirm clean working tree
git log --oneline -5                                          # last commit should be 9b85d65
sed -i 's/version = "0.6.29-alpha"/version = "0.6.30-alpha"/' Cargo.toml
# … implement Tasks 1-3 …
cargo check --target x86_64-unknown-linux-musl
bash dev/tui-smoke.sh
cargo build --release --target x86_64-unknown-linux-musl
# … tarball + commit per the convention above …
```

Then update `docs/project/2026-05-09-skills-runtime-gap-tracker.md` and ship.
