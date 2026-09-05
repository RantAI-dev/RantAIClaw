# Plan 150: An empty API key must not lock the operator out of the binary

> **Executor instructions**: Follow this plan step by step. Steps 1 and 2 are one
> PR (step 1 is the safety net, step 2 the prevention — land them together so a
> half-shipped state still unbricks). Run every verification command, including
> the live drive in step 3. If anything in "STOP conditions" occurs, stop and
> report. When done, add/update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 1744d3f..HEAD -- src/onboard/provision/provider.rs src/tui/app.rs src/tui/async_bridge.rs src/providers/rig_native.rs src/main.rs`
> All line numbers below are from `1744d3f` (v0.20.0-alpha). If this diff is
> non-empty, re-verify each cited line before editing.

## Status

- **Priority**: P1 — a first-run flow bricks the binary on Windows and Linux alike
- **Effort**: M
- **Risk**: MEDIUM (touches TUI boot; mitigated by reusing the existing Reload path)
- **Depends on**: none
- **Category**: bugfix
- **Planned at**: commit `1744d3f` (v0.20.0-alpha), 2026-08-16

## Why this matters

Reproduced by an operator on Windows, then verified in source at v0.20.0-alpha:

1. Run setup, pick `openai` as provider, leave the API key **empty** (the prompt
   allows it), finish setup, exit.
2. Every subsequent launch dies before the UI exists:

   ```
   PS C:\Users\sulth> rantaiclaw
   Error: openai: OPENAI_API_KEY required
   ```

3. All three interactive entry points route through the same `run_tui` and die
   the same way: bare `rantaiclaw` (`src/main.rs:1448`), `rantaiclaw setup`
   (`src/main.rs:1676`), `rantaiclaw chat` (`src/main.rs:2218`). **The official
   repair tool — `setup` — is itself dead.** The only ways out are hand-editing
   `config.toml` or setting `OPENAI_API_KEY` in the environment, and the error
   message mentions neither.

This is a self-inflicted, unrecoverable state reachable from the happy path of
the first-run wizard. §3.5 (fail fast) is satisfied by the bail; §3.6 (usable by
default) is not — failing fast into a locked room is not an error path, it is a
trap.

## Current state — the full chain

**Producer** — `src/onboard/provision/provider.rs:314`:

```rust
if api_key.trim().is_empty() {
    // Empty key: don't validate, just move on. Some flows
    // (gemini with CLI auth, dev mode) expect this.
    break;
}
```

The escape hatch is legitimate for providers that can build without a key
(ollama, gemini CLI-auth). It is applied **to every provider unconditionally** —
nothing asks whether the chosen provider can actually start keyless. Then
`provider.rs:498-503` persists the poison pair:

```rust
config.default_provider = Some(provider_name.to_string());
config.api_key = if api_key.trim().is_empty() { None } else { Some(api_key) };
```

Contrast with the **rejected-key** path right above (`provider.rs:368-403`):
a 401/403 from the probe gets a `Severity::Warn` message plus a three-option
`Choose` (re-enter / continue anyway / abort). The empty-key path gets nothing.
Commit `7052a5f` (#485) closed the rejected-key persistence hole but never
touched the empty-key branch — it breaks out of the loop before the probe runs.

The custom-URL branch (`provider.rs:210-213`) also accepts an empty key, and
that one is **correct**: `create_provider("custom:…", None)` builds keyless,
pinned by `factory_custom_no_key` (`src/providers/mod.rs`). Keyless-ness is a
per-provider capability the factory already knows. Do not touch this branch.

**Consumer** — `src/tui/app.rs:7692`:

```rust
let mut agent = Agent::from_config(&app_config).await?;
```

Hard `?` before any UI exists. The chain: `Agent::from_config` →
`create_routed_provider` (`src/agent/agent.rs` ~:502) →
`src/providers/rig_native.rs:134`
`api_key.context("openai: OPENAI_API_KEY required")?`. (Same shape for
`anthropic` at :125 and `gemini` at :143 — gemini only reaches the rig branch
when no CLI-auth credential resolves.)

**Dead recovery** — `src/tui/app.rs:7819`:

```rust
} else if app_config.api_key.is_none() && app_config.default_provider.is_none() {
    app.first_run_wizard = Some(...);
}
```

Doubly dead: it sits 127 lines *after* the bail, and its condition is already
false — the producer wrote `default_provider = Some("openai")`.

**The healing mechanism already exists** — `src/tui/async_bridge.rs:74-96`:
`TurnRequest::Reload(Box<Config>)` rebuilds the actor's `Agent` from a fresh
config; `reload_config` (`src/tui/app.rs:2319`) reads + decrypts + sends it on
wizard/overlay close. A running TUI can already recover from a bad provider
config. The only gap is that boot refuses to reach the running state.

**Env fallback exists and must keep working** — `src/providers/mod.rs:887-919`:
when no config credential resolves, the factory consults per-provider env vars
(`OPENAI_API_KEY` at :890). This is why `$env:OPENAI_API_KEY="sk-..."` unbricks
today, and why the capability probe in step 2 must be the factory itself.

## Step 1 — Boot degrades instead of bailing (recovery)

**`src/tui/async_bridge.rs`**: change `TuiAgentActor.agent` from `Agent` to
`Option<Agent>`.

- `new(agent: Option<Agent>, …)`.
- `Submit` while `self.agent.is_none()`: emit the same event the UI already
  renders for turn failures (`AgentEvent::Error` or equivalent — find the
  variant the error arm of a turn already sends) with text like:
  `no working provider — fix it via /setup provider`. Do not crash, do not
  queue the message.
- `Reload`: on success set `self.agent = Some(new_agent)` — this is the heal.
  On failure keep current state (today's behavior).
- `Compact`/turn paths: guard on `Some` the same way as `Submit`.

**`src/tui/app.rs:7692`**: replace the `?` with a `match`:

- `Ok(agent)` → today's path, unchanged.
- `Err(e)` → proceed with `None` agent:
  - `security_handle` → `None` (`Agent::security()` already returns
    `Option<Arc<SecurityPolicy>>`, `src/agent/agent.rs:592`; `app.context.security`
    is already an Option).
  - `memory_handle` → `app.context.memory = None` (it is already an
    `Option`; `/memory` degrades gracefully).
  - `mcp_tools_by_server` → empty map.
  - Skip the `/resume` history-restore block (it needs an agent).
  - After `app` is built: append a system banner —
    `⚠ provider failed to start: {e:#}. Opening setup — enter a key or switch
    provider; the session heals in place.` — and call
    `app.open_setup_overlay("provider".into())` (topic verified:
    `src/onboard/provision/registry.rs:9`). If a `setup_provisioner` topic was
    passed (the `rantaiclaw setup <topic>` path at :7688), let that topic win;
    the banner still prints.

**`src/tui/app.rs:7819`**: leave the condition as-is. Its remaining job —
fresh install, nothing configured — still works, and the lockout case is now
owned by the error arm above. Widening it would re-trigger the wizard for
operators who deliberately run keyless providers.

Trace **every** use of `agent` between :7692 and the actor spawn (:7721-ish)
and give each a `None`-safe default — the three accessors and the resume block
are the known ones at `1744d3f`; re-grep after the drift check.

**Verify:**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib tui
```

Mutation proof (per repo practice): with the fix in place, temporarily restore
the `?` at :7692 — the new boot-degrade test must fail; revert.

Add a test at the decision seam if one is extractable (the repo prefers pure
seams — see `decide_gateway_action` in `src/webui.rs:175` for the pattern). At
minimum: an async_bridge test that `Submit` with `agent: None` emits the error
event and does not panic, and that `Reload` with a valid config transitions
`None → Some`.

## Step 2 — Setup refuses to silently save a provider that cannot start (prevention)

In `src/onboard/provision/provider.rs`, at the empty-key branch (:314): before
`break`, ask the factory whether this provider builds keyless:

```rust
if api_key.trim().is_empty() {
    match crate::providers::create_provider(provider_name, None) {
        Ok(_) => break, // keyless is a real capability for this provider (or env supplies the key)
        Err(_) => { /* warn + choose, below */ }
    }
}
```

The factory is the **same oracle boot uses** — including the env-var fallback
(`providers/mod.rs:887`), so an operator with `OPENAI_API_KEY` exported sails
through with an empty config key, exactly matching what boot will do. No
hand-written exception list to drift.

On `Err`: reuse the existing rejected-key UI pattern (`provider.rs:368-403`) —
`Severity::Warn` message ("openai cannot start without an API key") plus a
`Choose` with **two** options:

1. `Re-enter the API key` → `continue` (re-prompt).
2. `Abort setup (nothing will be saved)` →
   `anyhow::bail!(…)` — matching the abort arm at :399, which the
   `ProvisionOutcome` machinery from #485 already maps to nothing-persisted.

Deliberately **no** "continue anyway": for a provider that literally cannot
construct, that option is the lockout with extra steps. (Step 1 would catch it,
but recovery is the net, not the license.)

Notes:

- `create_provider` is a cheap constructor — the `factory_*` test suite calls
  it keyless dozens of times with no I/O. Confirm no network happens for the
  providers you touch (STOP condition otherwise).
- Do not touch the custom-URL branch (:210-213) — correct today.
- The probe's blocking cost is nil, but the provisioner is async event-driven;
  `create_provider` is sync — call it inline, no spawn needed.

**Verify:**

```bash
cargo test --lib onboard::provision
cargo test --lib providers   # factory contract unchanged
```

Mutation proof: revert the gate (restore unconditional `break`) — the new
provisioner test must fail; revert.

Follow the existing provisioner test harness in `src/onboard/provision/`
(event-script style used by the #484/#485 tests) to script: empty key for
`openai` → expect the Warn + Choose; select abort → assert
`ProvisionOutcome::Aborted` and config unchanged. And: empty key for `ollama`
→ sails through as today (pin the capability split).

## Step 3 — Live drive (the exit condition)

Per repo practice the exit is a driven binary, not a green suite. Use the
sandbox technique from prior efforts: point `RANTAICLAW_CONFIG_DIR` at a
scratch profile so the real one is untouched.

1. **Cold repro first** (evidence the bug exists before the patch): on
   `1744d3f`, craft `config.toml` with `default_provider = "openai"`,
   `default_model` set, no `api_key`, ensure `OPENAI_API_KEY` is **unset** —
   run `rantaiclaw` in tmux → confirm `Error: openai: OPENAI_API_KEY required`
   and immediate exit. Confirm `rantaiclaw setup` dies identically.
2. **Patched binary, same config**: TUI must open, show the banner, and land in
   the provider setup overlay. Enter a valid key (or switch to a keyless
   provider) → close overlay → send a chat message → a reply arrives **without
   restarting the process** (this proves the Reload heal).
3. **Patched setup flow**: run the provider provisioner, pick `openai`, submit
   an empty key → the Warn + Choose must appear; abort → relaunch → no lockout,
   config unchanged. Then repeat picking `ollama` with an empty key → no prompt,
   saves as today.
4. **Env-var path**: `OPENAI_API_KEY=sk-test` exported, empty key in setup →
   no prompt (factory resolves env), boot works.

Record observed output in the PR body.

## Step 4 — Docs and changelog

- `CHANGELOG.md`: user-visible entry under fixes — setup no longer saves a
  provider it cannot start; a broken provider config now opens setup instead of
  refusing to launch.
- `docs/start/troubleshooting.md`: the `OPENAI_API_KEY required` symptom is now
  worth an entry — old binaries still exhibit it; document the two manual
  escapes (env var, `config.toml` edit) for operators on ≤ v0.20.0-alpha.
- No config keys added or removed → **no schema bump**.

## Non-goals

- The `src/webui.rs` findings (false-success `ui start` report at :1003,
  `/dev/null` in `token_rejected` at :515, swallowed run-file write at :1001,
  Unix-only detach at :140) — real, separate concern, separate plan/PR.
- Widening the first-run-wizard condition at `app.rs:7819`.
- Any change to credential precedence (config over env stays — §3.6).
- Healing machines already locked by old binaries (impossible from inside a
  fixed binary they can't run; that's what the troubleshooting entry is for).

## Risk and rollback

- **Risk**: step 1 touches TUI boot for the healthy path too — the `match`
  must leave `Ok` byte-identical in behavior. The `Option<Agent>` refactor is
  confined to `async_bridge.rs` + the boot function; if it fans out further,
  STOP.
- **Rollback**: revert the single PR. No schema change, no migration, no
  on-disk state — reverting restores today's behavior exactly.

## STOP conditions

- The drift check shows `provider.rs` / `app.rs` / `async_bridge.rs` changed
  since `1744d3f` in the cited regions and re-verification finds the chain
  altered (e.g. someone already guards the empty key).
- `create_provider(<name>, None)` performs network I/O or other side effects
  for any provider reachable from the provisioner list.
- `Option<Agent>` forces signature changes outside `src/tui/async_bridge.rs`
  and `run_tui`/adjacent helpers in `src/tui/app.rs`.
- `open_setup_overlay("provider")` cannot render before the login gate /
  first-run wizard without layering conflicts — report the interaction instead
  of forcing it.
- The live drive in step 3.2 cannot heal without a restart — the Reload path
  has a gap this plan didn't predict; report it, don't patch around it.
