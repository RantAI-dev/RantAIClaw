# Plan 158: `/model` must switch the agent's model, not just the status-bar label

> **Executor instructions**: Follow this plan step by step. This is one PR.
> Run every verification command, including the live drive in Step 6. If
> anything in "STOP conditions" occurs, stop and report. When done, add/update
> this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 8735d9e..HEAD -- src/tui/app.rs src/tui/commands/model.rs src/tui/commands/mod.rs src/tui/async_bridge.rs`
> All line numbers below are from `8735d9e` (post-v0.22.0-alpha). If this diff
> is non-empty, re-verify each cited line before editing.

## Status

- **Priority**: P1 — the command exists, reports success ("Model set to: …"),
  and does nothing; every turn after a `/model` switch silently runs (and
  bills) on the old model
- **Effort**: M
- **Risk**: MEDIUM (touches the TUI→agent bridge; mitigated by reusing the
  existing wizard-close Reload path end to end)
- **Depends on**: none. Independent of plan 159.
- **Category**: bugfix
- **Planned at**: commit `d0089a4` (v0.21.0-alpha), 2026-08-17; line numbers
  re-anchored at `8735d9e` (post-v0.22.0-alpha), 2026-08-18

## Why this matters

Reported by an operator: after `/model`, the label changes but replies still
come from the old model. Verified in source — this is not a hot-reload
regression; `/model` has been cosmetic since it was written:

- Picker Enter (`src/tui/app.rs:3183-3192`, `ListPickerKind::Model` arm): sets
  `self.context.model = key`, clears `last_error`, prints "Model set to: {key}".
  Nothing else.
- `/model <arg>` (`src/tui/commands/model.rs:73-83`): sets `ctx.model`, returns
  `CommandResult::Message`. Nothing else.

`ctx.model` is a display string: the status bar, `/usage`, and the session
record read it. The agent never does — it is built once by
`Agent::from_config(&config)` from `config.default_provider`/`default_model`
and replaced only by `TurnRequest::Reload` (`src/tui/async_bridge.rs:85-116`).
`TurnRequest::Submit(String)` (`async_bridge.rs:12`) carries only the text.
`/model` neither writes the config nor sends a `Reload`, and the config
watcher (`app.rs:345`) cannot fire because `config.toml` never changed. The
reload machinery itself is healthy — the wizard and `/setup` use it every day.

So the user-visible contract ("Model set to: X") is a lie on three surfaces:
the next turn's provider/model, the config on disk, and the next launch.

## Design

On a user-driven model switch (either entry path):

1. keep the existing label/UX behaviour (`ctx.model`, `last_error`, message);
2. write `config.default_provider` + `config.default_model`;
3. persist via `Config::save()` (`src/config/schema.rs:4462`, `async fn`,
   takes `&self` — it encrypts credentials into the on-disk form without
   mutating the in-memory decrypted copy, per the invariant documented at
   `app.rs:258-261`);
4. push `TurnRequest::Reload` with the updated (still-decrypted) config —
   the same pattern the skills watcher uses at `app.rs:3781-3785` and the
   wizard save path uses.

Target strings are `provider:model` (`ModelEntry::target()`,
`src/tui/widgets/model_picker.rs:16` — every picker key has this shape).
Model ids may themselves contain `:` (ollama `llama3:8b`), so the split is
`split_once(':')`: first segment is the provider. A bare argument with no `:`
(typed by hand) switches only the model and keeps the current provider.

Known double-fire, accepted: the save triggers the config watcher, whose
debounced tick runs `reload_config` (`app.rs:2378`) and pushes a second
`Reload` with identical config. One redundant `Agent` rebuild (incl. MCP
re-discovery) per manual model switch — a human-frequency event. Suppressing
it would need another memo flag like `channels_restarted_for_save`
(`app.rs:262`); not worth the state until someone measures pain (YAGNI).
`reload_config` also re-derives `context.model` and `available_providers`
(`app.rs:2485-2494`) from the saved config, which converges to the same
values — no flicker.

## Step 1 — `split_model_target` (pure function + tests first)

In `src/tui/commands/model.rs`, above `ModelCommand`:

```rust
/// Split a `/model` target into `(provider, model)`.
///
/// Picker keys are always `provider:model` (`ModelEntry::target()`), but
/// model ids may themselves contain `:` (ollama `llama3:8b`), so only the
/// first segment is the provider. A bare string with no `:` (or an empty
/// first segment) names a model only — the caller keeps the current
/// provider.
pub(crate) fn split_model_target(target: &str) -> (Option<&str>, &str) {
    match target.split_once(':') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            (Some(provider), model)
        }
        _ => (None, target),
    }
}
```

Tests in the same file's existing `#[cfg(test)]` module:

```rust
    #[test]
    fn split_target_with_provider() {
        assert_eq!(
            split_model_target("openrouter:meta/llama-4-scout"),
            (Some("openrouter"), "meta/llama-4-scout")
        );
    }

    #[test]
    fn split_target_keeps_colons_inside_the_model_id() {
        assert_eq!(
            split_model_target("ollama:llama3:8b"),
            (Some("ollama"), "llama3:8b")
        );
    }

    #[test]
    fn split_target_bare_model_names_no_provider() {
        assert_eq!(split_model_target("gpt-5.3"), (None, "gpt-5.3"));
    }

    #[test]
    fn split_target_degenerate_colon_forms_are_model_only() {
        assert_eq!(split_model_target(":x"), (None, ":x"));
        assert_eq!(split_model_target("x:"), (None, "x:"));
    }
```

Run: `cargo test --lib tui::commands::model` — the four new tests fail to
compile until the function exists, then pass.

Export it for the app: in `src/tui/commands/mod.rs`, next to the existing
re-exports, add:

```rust
pub(crate) use model::split_model_target;
```

(Check the `mod model;` declaration's visibility there first; the item itself
stays `pub(crate)`.)

## Step 2 — `CommandResult::SetModel`

In `src/tui/commands/mod.rs`, add a variant to `CommandResult`
(`mod.rs:36-66`):

```rust
    /// Switch the active model: update the label, persist
    /// `default_provider`/`default_model`, and rebuild the agent via
    /// `TurnRequest::Reload`. Carried as a result rather than done in the
    /// handler because the config and the agent bridge live on `TuiApp`,
    /// not on `TuiContext`.
    SetModel(String),
```

## Step 3 — `TuiApp::apply_model_selection`

In `src/tui/app.rs`, next to `reload_config` (`app.rs:2386`):

```rust
    /// Apply a user-driven model switch from `/model` (arg or picker).
    ///
    /// Until v0.21.0-alpha this was label-only: `context.model` changed,
    /// the agent kept its launch-time model (`Agent::from_config`), and
    /// the config on disk never learned about the switch. Persist first,
    /// then reload — the same ordering the wizard save path uses, so a
    /// crash between the two leaves disk authoritative.
    fn apply_model_selection(&mut self, target: &str) {
        self.context.model = target.to_string();
        // The previous `last_error` (e.g. "model unavailable") may have
        // been caused by the model we are leaving.
        self.context.last_error = None;
        let msg = format!("Model set to: {target}");
        let _ = self.context.append_system_message(&msg);
        self.scrollback_queue.push(("system".to_string(), msg));

        let (provider, model) = super::commands::split_model_target(target);
        if let Some(provider) = provider {
            self.config.default_provider = Some(provider.to_string());
        }
        self.config.default_model = Some(model.to_string());

        let config = self.config.clone();
        let req_tx = self.context.req_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = config.save().await {
                // The in-session switch below still applies; only the
                // on-disk copy is stale. Next successful save (wizard,
                // /setup) heals it.
                tracing::warn!("failed to persist model change: {e:#}");
            }
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload(Box::new(config)))
                .await;
        });
    }
```

Note `self.config` is the decrypted in-memory config (the same one the
provisioner save path clones), so the `Reload` hands the agent usable
credentials — the invariant `reload_config`'s decrypt pass exists to protect
(`app.rs:2405-2411`).

## Step 4 — Both entry paths route through it

**Picker** — replace the `ListPickerKind::Model` arm
(`app.rs:3183-3192`) body with:

```rust
            ListPickerKind::Model => {
                self.apply_model_selection(&key);
            }
```

**Arg path** — in `src/tui/commands/model.rs::execute` (`model.rs:73-83`),
replace the early-return block with:

```rust
        let model = args.trim();
        if !model.is_empty() {
            return Ok(CommandResult::SetModel(model.to_string()));
        }
```

(The `ctx.model` / `last_error` mutations move into
`apply_model_selection` — do not leave them duplicated here.)

**Result handler** — in `TuiApp::handle_command`'s match
(`app.rs:3001-3063`), add:

```rust
            CmdResult::SetModel(target) => {
                self.apply_model_selection(&target);
            }
```

## Step 5 — Behaviour test (persist + reload observed, then mutation-proved)

The existing helper `make_app_from_store` (`app.rs:8857`) drops the request
receiver, so the bridge cannot be observed. Refactor it into a variant that
keeps the receiver, and delegate:

```rust
    /// Like `make_app_from_store`, but hands back the bridge's request
    /// receiver so a test can observe what the app sends to the agent.
    fn make_app_with_bridge_from_store(
        store: SessionStore,
        model: &str,
    ) -> (TuiApp, tokio::sync::mpsc::Receiver<TurnRequest>) {
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (_events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let ctx = TuiContext::new(store, model, None, req_tx, events_rx).expect("context");
        let app = TuiApp {
            // …identical field-by-field body of today's make_app_from_store…
        };
        (app, req_rx)
    }

    fn make_app_from_store(store: SessionStore, model: &str) -> TuiApp {
        make_app_with_bridge_from_store(store, model).0
    }
```

(Move the existing struct literal wholesale; `make_app_from_store` keeps its
signature so the ~30 existing call sites are untouched.)

The test:

```rust
    #[tokio::test]
    async fn model_selection_persists_config_and_reloads_the_agent() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "anthropic:old-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();

        app.apply_model_selection("openrouter:meta/llama-4-scout");

        // Label updated synchronously.
        assert_eq!(app.context.model, "openrouter:meta/llama-4-scout");

        // The reload carries the new pair. Save happens before the send in
        // the same task, so once this arrives the file must exist too.
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), req_rx.recv())
            .await
            .expect("reload sent within 5s")
            .expect("bridge channel open");
        let TurnRequest::Reload(config) = req else {
            panic!("expected TurnRequest::Reload");
        };
        assert_eq!(config.default_provider.as_deref(), Some("openrouter"));
        assert_eq!(config.default_model.as_deref(), Some("meta/llama-4-scout"));

        let raw = std::fs::read_to_string(tmp.path().join("config.toml"))
            .expect("config persisted to disk");
        assert!(raw.contains("meta/llama-4-scout"), "saved config names the new model");
    }
```

And the bare-model rule:

```rust
    #[tokio::test]
    async fn bare_model_switch_keeps_the_current_provider() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "anthropic:old-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("anthropic".to_string());

        app.apply_model_selection("claude-opus-5");

        let req = tokio::time::timeout(std::time::Duration::from_secs(5), req_rx.recv())
            .await
            .expect("reload sent within 5s")
            .expect("bridge channel open");
        let TurnRequest::Reload(config) = req else {
            panic!("expected TurnRequest::Reload");
        };
        assert_eq!(config.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(config.default_model.as_deref(), Some("claude-opus-5"));
    }
```

Run: `cargo test --lib tui::app::tests::model_selection` (and
`bare_model_switch`). Expected: PASS.

**Mutation proof** (vacuous-guard check — do not skip):

1. Comment out `self.config.default_model = Some(model.to_string());` —
   `model_selection_persists_config_and_reloads_the_agent` must FAIL on the
   `default_model` assert. Restore.
2. Comment out the `req_tx.send(...)` — the same test must FAIL on the 5s
   timeout. Restore.

If either mutation leaves the test green, the test is decorative; fix it
before proceeding.

## Step 6 — Live drive (the exit condition)

The oracle is behavioural, not the label (labels lied before this plan):

```bash
tmux new-session -d -s modelswitch
tmux send-keys -t modelswitch 'RUST_LOG=info ./target/debug/rantaiclaw 2>/tmp/model-switch.log' Enter
```

1. Send a turn; note it answers (baseline on the configured model).
2. `/model <same-provider>:definitely-not-a-real-model` (typed arg path).
   Send a turn. **Expected**: the turn FAILS with a provider error naming
   `definitely-not-a-real-model` — proof the agent, not the label, switched.
   Before this plan, this turn would succeed on the old model.
3. `grep "agent reloaded with new config" /tmp/model-switch.log` — the
   `async_bridge.rs:102` line must appear (twice per switch is acceptable —
   direct Reload + config-watcher pass; see Design). If the switch landed
   mid-turn, the deferred variant "agent reloaded with new config
   (post-turn)" (`async_bridge.rs:243`) counts equally.
4. `/model` (picker path), select a real model, send a turn. **Expected**:
   answers normally.
5. `grep default_model ~/.rantaiclaw/config.toml` (adjust for the active
   profile dir) — shows the last-picked model.
6. Quit, relaunch, ask "which model are you" or check the status bar —
   the switch survived the restart.

Use a sandbox `RANTAICLAW_CONFIG_DIR` if you don't want your real profile's
config touched (the technique from the 2026-08-16 live drives).

## Step 7 — Docs

`docs/reference/commands.md` — the `/model` entry: state that switching
persists `default_provider`/`default_model` and hot-reloads the running agent
(previously undocumented because it did neither). One or two sentences; this
file is a runtime-contract reference (CLAUDE.md §4.1).

## Non-goals

- No validation that the typed model exists — the arg path accepts free text
  today and the live-drive oracle depends on that; a wrong model fails the
  next turn with the provider's own error, which is the honest signal.
- No `model_routes` editing; `available_providers` for the picker still
  derives from `default_provider` + routes (`app.rs:2485-2494`).
- No channel-runtime restart: channels rebuild from config on their own
  restart path; the channels fingerprint is model-agnostic, so no listener
  flap from a model switch.
- `/resume` restoring a session's model into `ctx.model` (`app.rs:3208`)
  remains label-only. Making resume re-point the agent is a separate decision
  with its own UX questions (a resumed session silently rewriting your
  default model is arguably worse) — leave it, note it in the PR body.

## Risk and rollback

- Risk: MEDIUM — writes to `config.toml` from a new call site and rebuilds
  the agent mid-session. Both are existing, exercised paths (`Config::save`
  from provisioners; `Reload` from wizard close and the skills watcher); the
  new code only routes `/model` through them.
- No schema change: `default_provider`/`default_model` are existing keys; no
  defaults widened; schema version untouched.
- Rollback: revert the single commit — `/model` degrades back to label-only,
  which is the current (broken but non-destructive) behaviour.

## STOP conditions

- `Config::save()` turns out to mutate `self` or re-encrypt in place (the
  plan relies on `&self` + non-mutating encryption per `app.rs:258-261` and
  `schema.rs:4462`) — stop; the clone-then-save ordering needs rethinking.
- The behaviour test cannot construct `TuiApp` without a live tokio runtime
  beyond `#[tokio::test]`, or `Config::default().save()` demands state a
  TempDir cannot provide — stop and report what it needs; do not weaken the
  test to assertions on `app.config` in memory only (that half passes with
  the persistence line deleted).
- Either mutation in Step 5 leaves the suite green after a genuine fix
  attempt — stop; the test design is wrong, not the mutation.
- The live drive's step 2 turn SUCCEEDS on the fake model — the agent did not
  switch; do not rationalize it (e.g. "provider ignores unknown models") —
  find the actual reason before shipping.
