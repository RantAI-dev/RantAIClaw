# Changelog

All notable changes to RantaiClaw are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.8-alpha] — 2026-05-07

UI consistency cut driven by the v0.6.7 tester recommendation: *"Change
the shitty on chat ui or infos to proper tui comp ui."* Seven info
commands now open dedicated TUI panels instead of dumping `System:`
chat blobs. Three picker/wizard polish fixes. One alias dropped.

### Added

- **`InfoPanel` widget** at `src/tui/widgets/info_panel.rs` — bordered
  modal matching the `list_picker` visual language: sky-bold title,
  optional subtitle, sectioned typed rows (`KeyValue` / `Status` / `Bullet`
  / `InlineList` / `Plain` / `Spacer`), scrollable when content overflows,
  Esc closes, ↑/↓/PgUp/PgDn scroll. Brand colors stay in sync with
  `list_picker` and `setup_overlay` so the surfaces feel like one app.
- New `CommandResult::OpenInfoPanel(InfoPanel)` variant; `TuiApp.info_panel`
  field; render integration in both inline and fullscreen paths;
  alt-screen toggle picks up the panel.

### Changed

- **`/channels`** — text-blob → InfoPanel. Sections: Always available /
  Runtime (auto-start state) / Configured (per-channel status with the
  same icon-vocabulary as `/doctor`) / Not configured (compact comma-
  list) / Logs.
- **`/config`** — text-blob → InfoPanel. Sections: Runtime / Persisted
  with pointer at `~/.rantaiclaw/profiles/<active>/config.toml`.
- **`/doctor`** — text-blob → InfoPanel + content expansion. Was
  3 trivial checks (session store, model, TUI); now adds Channels
  (auto-start state + each configured channel), Skills (count loaded),
  Workspace (`~/.rantaiclaw/`, `profiles/`).
- **`/insights`** — text-blob → InfoPanel. Sections: Sessions (total +
  current age) / Messages (total + current + per-session avg) / Tokens
  (this session).
- **`/status`** — text-blob → InfoPanel. Sections: Agent / Session.
- **`/usage`** — text-blob → InfoPanel. Sections: Tokens / Model
  (active + context window).
- **`/skill`** (no args) — text-blob → InfoPanel listing all loaded
  skills with descriptions; usage hint section. `/skill <name>` opens
  a per-skill detail panel.

### Polish

- **First-run wizard welcome footer**: "skip any step with Esc" → "Esc
  to cancel". Esc on Welcome quits the wizard (there's nothing to skip
  yet); the wording was misleading. Mid-step screens still say "skip"
  which is correct semantics there.
- **List picker cross-page navigation**: pressing ↓ at the last item
  of a page now advances to the first item of the next page (and
  symmetric for ↑ at the first item of a non-first page). Pre-v0.6.8
  ↓ wrapped to row 1 of the same page, leaving testers stuck on
  page 1 of 3 in the ClawHub picker without realizing PgDn was
  required to advance.
- **`/personality` picker** now opens on the actual current preset and
  marks that row with `· current` in the secondary line. Pre-v0.6.8
  the picker hardcoded `Some("default")` as the preselect, ignoring
  whatever was actually saved in `<profile>/persona/persona.toml`.

### Removed

- **`/platforms` alias** — was a v0.6.4 alias for `/channels` for
  muscle memory, but tester feedback flagged the duplicate output as
  noise. The single canonical command is `/channels`.

### Compatibility

- No on-disk-state changes. No new deps.
- `CommandResult::OpenInfoPanel(...)` is additive; existing callers
  using `Message(...)` continue to render as inline chat lines.

## [0.6.7-alpha] — 2026-05-07

Two TUI fixes from v0.6.6-alpha tester feedback. One UX gap deferred
(ClawHub picker default selection) for v0.6.8.

### Fixed

- **Channel events leak into the local TUI scrollback** — every incoming
  channel message ("[telegram] from @user: ..."), the "Processing
  message..." progress line, every reply ("Reply (4208ms): ..."), and
  delivery failures were `println!`/`eprintln!` to stdout. In TUI mode
  stdout is the alt-screen, so the channel chatter corrupted the
  rendering and exposed Telegram conversations the local user wasn't
  meant to see. v0.6.7 routes all four through `tracing::info!` /
  `tracing::error!` instead. Channel activity is now visible only in
  `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log`. Daemon-mode operators who
  relied on stdout for live message tracing should now `RUST_LOG=info`
  + tail the log file. (`src/channels/mod.rs` lines ~1324, ~1374,
  ~1563, ~1589.)

### Added

- **Restart-needed cue when channels are added/removed mid-session** —
  `reload_config` now compares channel count before vs after and pushes
  a `⚠` system message into chat scrollback if it changed. Tester report
  was "Telegram works only after restarting `rantaiclaw`" — true, and
  the cue makes the requirement visible. Auto-restart of the
  `start_channels` task is the v0.6.8 follow-up; doing it cleanly needs
  `start_channels` to accept a cancellation token to avoid orphaning
  supervised listener tasks.

### Deferred

- ClawHub picker installs 0 skills despite "✓ Installed from ClawHub"
  banner. Picker likely defaults to nothing-selected; user pressed
  Enter without picking. Fix is a UX adjustment in `src/onboard/section/skills.rs`
  — either default-check top-3 skills or warn on empty selection. Will
  land in v0.6.8.

### Compatibility

- No on-disk-state changes. No new deps.

## [0.6.6-alpha] — 2026-05-07

Diagnostic upgrade for the channel auto-start path. Tester reported v0.6.5
showed "polling" for Telegram in `/channels` but the bot still didn't
reply — meaning the dispatch happened but `start_channels` either errored
mid-build or the listener silently failed. Pre-v0.6.6 had no way for the
user to see what went wrong; the warn was logged to a file the user
didn't know to check.

### Added

- `src/channels/auto_start_state.rs` — global Mutex<AutoStartState> with
  variants `NotDispatched`, `Starting`, `Terminated`, `Failed{message}`.
  The TUI auto-start callback marks the state through the spawn lifecycle.
- `/channels` now reads the snapshot and shows one of:
  - `running` — start_channels is past the build phase and likely in the
    dispatch loop
  - `starting…` — within the first 5 seconds of startup
  - `FAILED — see error below` + the formatted error chain
  - `stopped (dispatch loop exited)`
  - `configured · not started in this process`
- Footer hint at the bottom of `/channels` always points at
  `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log` for full provenance.

### Fixed

- `/channels` no longer reports `polling` when the spawn task errored.
  Tester report: "Telegram still not working even though it reports to
  polling." Status was misleading; this gives an honest answer.

### Compatibility

- No on-disk-state changes. No new deps.

## [0.6.5-alpha] — 2026-05-07

Build-pipeline fix. v0.6.3-alpha and v0.6.4-alpha binaries reported their
version as `0.6.2-alpha` because Swatinem/rust-cache was restoring a
target/ from a previous v0.6.2-alpha build, and cargo's incremental
compilation didn't re-emit `main.rs` with the new `CARGO_PKG_VERSION`
even though Cargo.toml had been bumped. The binaries were otherwise
identical functionally — they had all the v0.6.3 + v0.6.4 fixes — but
the version string was wrong, which testers flagged as confusing.

### Fixed

- **Wrong `--version` output on alpha builds** — `pub-release.yml` now
  runs `cargo clean -p rantaiclaw --target <target>` between the cache
  restore step and the build step. This invalidates only the
  `rantaiclaw` package's incremental-compilation fingerprint while
  leaving dependency builds cached, so `env!("CARGO_PKG_VERSION")`
  re-expands fresh against the current `Cargo.toml`. Build-time impact:
  ~30-60 sec extra on cache-warm runs (the rantaiclaw crate recompiles
  from scratch instead of incrementally). Negligible on cache-cold
  runs. (`.github/workflows/pub-release.yml`.)

### Compatibility

- **No source code changes.** This is a CI-only fix. The shipped binary
  is functionally identical to v0.6.4-alpha; only the `--version` output
  is corrected. If you trust your v0.6.4-alpha build is doing the right
  thing functionally (e.g. Telegram replies), it's the same code.

## [0.6.4-alpha] — 2026-05-07

Follow-up to v0.6.3-alpha tester feedback. Fixes the channel deadlock
(Telegram bot configured but never replied), makes the channel state
visible, and lands the deferred back button.

### Fixed

- **Telegram / Discord / Slack / etc. don't reply when running bare
  `rantaiclaw`** — the TUI process was the canonical "all-in-one"
  runtime in user expectations, but it was only running the local-chat
  agent actor. Configured channels needed a separate `rantaiclaw daemon`
  to be polled, which wasn't documented or discoverable. v0.6.4 spawns
  `start_channels` as a background task alongside the TUI when any
  channel is configured. Failure-mode discipline: channel-startup
  errors are logged but don't crash the TUI; the user can still chat
  locally. (`src/tui/app.rs` `run_tui`.)
- **`/platforms` always reported "TUI active" only** — was a hardcoded
  string. Now reflects the actual `channels_config` from
  `<profile>/config.toml` and indicates whether each channel is
  configured + whether the TUI is polling it. Refreshes on
  `reload_config` so post-`/setup` runs see the new state.
  (`src/tui/commands/config.rs`.)

### Added

- **`/channels` command** — first-class command to list configured +
  active channels. `/platforms` now aliases to `/channels`.
- **Back button in the first-run wizard** — `Ctrl+B` walks the phase
  history one step back. Safe cases (PickChannels ↔ PickIntegrations
  ↔ Welcome) work fully. RunningProvisioner steps are skipped on
  rewind (the running task wrote to `config.toml`; surgical rewind
  isn't safe). For redoing a required section, `Esc` + re-run with
  `/setup <section>` remains the supported path. (`src/tui/first_run_wizard.rs`
  `back()`, `src/tui/app.rs` Ctrl+B handler.)

### Compatibility

- No new deps. No on-disk-state changes vs v0.6.3-alpha.
- Bare `rantaiclaw` now uses more memory + CPU when channels are
  configured (it spawns the channel listeners). For TUI-only mode
  with no channels, the auto-start branch is a no-op.

## [0.6.3-alpha] — 2026-05-07

Bug-fix cut driven by Sulthan + Alifia's first round of v0.6.1-alpha
testing (`bugs-123.pdf`). Five tester-reported bugs fixed; one
deferred (back-button navigation needs wizard rework).

### Fixed

- **Provider 401 immediately after `/setup provider`** — `reload_config`
  was reading the encrypted `config.toml` and pushing it straight to
  the agent actor without running the secret-decrypt pass that
  `Config::load_or_init` performs at startup. The agent received an
  encrypted blob in `config.api_key`, the HTTP request builder rejected
  the malformed Authorization header, and OpenRouter / OpenAI
  responded "401 Unauthorized: Missing Authentication header". Reload
  now runs the same `decrypt_optional_secret` pass for `api_key`,
  `composio.api_key`, `browser.computer_use.api_key`,
  `web_search.brave_api_key`, `storage.provider.config.db_url`, and
  every `agents.*.api_key`. (`src/tui/app.rs` `reload_config`)
- **`/skills` shows "No skills loaded" even after the starter pack
  installed** — v0.5.0 introduced a per-profile skills dir
  (`<profile>/skills/`) but the loader still looked at the v0.4.x
  workspace-level path (`<workspace>/skills/`). The bundled installer
  + ClawHub both write to the new path; the picker was reading from
  the old one. Loader now checks both, profile-level wins on conflict,
  deduped by name. Empty-state hint corrected to point at the actual
  v0.5.0+ path. (`src/skills/mod.rs` `load_workspace_skills`,
  `src/tui/commands/skills.rs`)
- **`/skill` and `/skills` produced identical output** — both opened
  the same picker. `/skill` (no args) now prints usage + an inline
  list of loaded skills; `/skills` keeps the interactive picker.
  `/skill <name>` unchanged. (`src/tui/commands/skills.rs`)
- **`/resume` shows "Resumed session ... (N messages)" but no
  history** — messages were loaded into `context.messages` but never
  pushed into the scrollback display queue. The user saw a fresh-looking
  TUI even though the agent had the history. Resume now replays each
  loaded message into `scrollback_queue` so the conversation actually
  appears. (`src/tui/app.rs` `ListPickerKind::Session` arm)
- **ClawHub install fails with 404 on auxiliary files** — a stale
  upstream `README.md` reference in a manifest was breaking the entire
  install. SKILL.md remains required (a skill without it is rejected
  per the bundled-format contract); other files (README, LICENSE, etc.)
  are now best-effort with a `tracing::warn!` on 404. (`src/skills/clawhub.rs`)

### Deferred

- **No back button in the wizard / setup picker** — substantial state-
  machine work to add reverse navigation across the seven setup steps.
  Filed as a follow-up; for now testers can `Esc` to cancel and re-run
  the section.

### Compatibility

- No new deps. No on-disk-state changes vs v0.6.2-alpha.
- Skills installed under the v0.4.x `<workspace>/skills/` layout still
  load (back-compat path retained).

## [0.6.2-alpha] — 2026-05-06

Lifecycle commands — closes the "how do I uninstall?" / "how do I update?"
completeness gap surfaced during v0.6.1-alpha tester onboarding. New module
`src/lifecycle/` with two commands.

### Added

- **`rantaiclaw uninstall`** — remove profile data, optionally the binary.
  Default scope is the active profile only; `--all` removes the entire
  `~/.rantaiclaw/` tree; `--purge` adds binary self-deletion. `--keep-secrets`
  preserves `.secret_key` for re-install. `--dry-run` prints the plan and
  exits 0. Coordinates with the daemon service unit (calls `service uninstall`
  automatically when present). Best-effort: comments out PATH amendments the
  installer may have added to `~/.bashrc` / `~/.zshrc` / `~/.profile` /
  `~/.config/fish/config.fish` with a date-stamped marker.
- **`rantaiclaw update`** — self-replace the binary against a published
  GitHub release. Verifies SHA256 against `SHA256SUMS`. Atomic swap on
  Unix (rename + rename, with `.old` backup and rollback on failure). On
  Windows, stages the new binary as `<exe>.new.exe`; the next launch
  detects and self-swaps before doing anything else.
  Flags: `--check`, `--channel stable|prerelease`, `--to <tag>`,
  `--allow-downgrade`, `-y/--yes`. Honors `RANTAICLAW_RELEASE_BASE_URL`
  for testing against staging or self-hosted releases.
- Refuses self-modification on cargo-managed binaries (`~/.cargo/bin/`)
  with a hint to use `cargo install rantaiclaw --force` or
  `cargo uninstall rantaiclaw` instead.

### Changed

- `src/main.rs` short-circuits `Update` and `Uninstall` before
  `Config::load_or_init` so they work on partially broken installs.
- Every launch applies a pending Windows update before doing anything
  else (no-op on Unix; cheap stat call on Windows).

### Compatibility

- **No deps added.** Implementation reuses existing `reqwest`, `sha2`,
  `hex`, `tempfile`. Archive extraction shells out to `tar` (Linux/macOS
  native, ships in Windows 10 1803+). This keeps the Cargo.toml dep
  surface unchanged from v0.6.1-alpha — a deliberate alignment with the
  bloat-audit "no new deps for one feature" rule.
- Configs and on-disk state from v0.6.1-alpha load unchanged.

### Tests

- `src/lifecycle/binary_path.rs` — InstallKind classification + cargo
  refusal.
- `src/lifecycle/uninstall.rs` — dry-run, default-active-profile-only,
  `--all` full wipe, `--keep-secrets` preserves `.secret_key`, shell rc
  amendment is commented out (not deleted).
- `src/lifecycle/update.rs` — semver comparison incl. prerelease ordering,
  SHA256SUMS line parsing for multiple formats, tag normalization.

## [0.6.1-alpha] — 2026-05-06

Alpha cut for **Sulthan + Alifia** to start E2E testing on real hardware. No
runtime behavior changes vs v0.5.3; this is a docs + PM-alignment build that
ships the first feature-grouped task structure under v0.6.0 — Product
Completeness Beta in ClickUp.

### Added

- **9 pillar docs** under `docs/pillars/` — one per product surface, with
  competitor-parity matrix vs OpenClaw + Hermes-agent, maturity table,
  architecture diagram, trait extension point, CLI/config, and roadmap.
  Pillar 1 = Setup, 2 = Providers, 3 = Tools/Approvals, 4 = Skills/MCP,
  5 = Channels, 6 = Memory/Profiles, 7 = Gateway/Daemon, 8 = Install/Release,
  9 = Documentation.
- **`docs/project/codebase-bloat-audit-2026-05-06.md`** — date-stamped
  source-code bloat audit (top 10 findings, dep hit list, module split
  candidates, niche features to feature-gate).
- **`docs/project/docs-cleanup-plan-2026-05-06.md`** — proposed lean docs
  structure aligned with ClickUp pillars (Phase A delivery).

### Changed

- `docs/README.md` rewritten as a 30-second decision-tree entry hub. Dropped
  the multilingual mirror claim that was never implemented.
- `docs/SUMMARY.md` rewritten as a unified TOC reflecting the new IA
  (start / reference / pillars / operations / security / hardware /
  contributing / project).
- `CLAUDE.md` §4.1 amended to drop EN/ZH/JA/RU parity claim and document the
  new pillar-anchored doc system + archive convention for superseded planning
  artifacts.

### Moved

- `docs/superpowers/{plans,specs}/*` (9 files, ~7,600 LoC of dated planning
  artifacts that were superseded by shipped releases v0.5.0–v0.5.3) →
  `docs/project/archive/superpowers/{plans,specs}/`. Content preserved per
  CLAUDE.md §4.1 archive convention.

### Compatibility

- **No runtime changes.** Configs and on-disk state from v0.5.3 load
  unchanged.
- Binary version string is now `0.6.1-alpha`; downstream tooling that
  pin-matches on `0.5.x` will need to widen the pattern.

## [0.5.3] — 2026-04-30

Model-default refresh — every provider's wizard menu and quick-mode
default now points at the actually-current generation. Older
generations stay in the menus as fallbacks for users on older API
tiers (with explicit `(previous flagship)` / `(legacy)` suffixes).

### Changed

- **OpenAI**: `gpt-5.2` → `gpt-5.5`. Codex variant: `gpt-5-codex` →
  `gpt-5.5-codex`. Menu adds gpt-5.3 / 5.2 / 5.1-codex-mini as
  fallbacks. (#45)
- **Anthropic**: `claude-sonnet-4-5-20250929` → `claude-sonnet-4-6`.
  Menu adds `claude-opus-4-7` and keeps `haiku-4-5` + legacy
  `sonnet-4-5`. (#40 → #45 verified)
- **Gemini / Google**: `gemini-2.5-pro` → `gemini-3-pro`. Adds
  `gemini-3-flash`; previous tier kept. (#45)
- **Moonshot / Kimi**: `kimi-k2.5` → `kimi-k2.6`. Older revisions stay
  in the menu. (#45)
- **GLM / Z.AI**: `glm-5` → `glm-5.1`. (#45)
- **MiniMax**: `MiniMax-M2.5` → `MiniMax-M2.7` (with M2.7-highspeed
  variant). (#45)
- **Qwen**: `qwen-plus` → `qwen3.6-plus`; coder track moves to
  `qwen3.6-coder-plus`. (#45)
- **Meta / Llama family** (groq, fireworks, together-ai, nvidia,
  llamacpp): default flips from the `llama-3.3-70b` family to
  **`llama-spark`** — Meta's latest generation. (#45)
- **OpenRouter / Venice / Astrai** curated lists pick up
  cross-provider entries: `gpt-5.5`, `gpt-5.5-codex`, `gemini-3-pro`,
  `gemini-3-flash`, `kimi-k2.6`, `glm-5.1`, `meta-llama/llama-spark`.
  (#45)
- **Bedrock**: `…sonnet-4-5-20250929-v1:0` → `…sonnet-4-6-v1:0`. (#45)

### Compatibility

- **Backward compatible.** Explicit `--model` overrides and existing
  configs are honored unchanged. Only the *next* `onboard` run picks
  up the new default for users who didn't pin one.
- DeepSeek left at V3.2 pending confirmation on the newer model id —
  if you have it, drop the name and the wizard wires it in next.

## [0.5.2] — 2026-04-29

Setup-flow polish + visual fixes from a real end-to-end audit. Every
fix is testable today; the audit doc that drove the batch lives at
`docs/superpowers/specs/2026-04-29-setup-audit.md`.

### Added

- **Hermes-style splash for `agent` + `setup`** — `RANTAICLAW` rendered
  in chunky ANSI Shadow figlet at the top, brand logo as 30×16 Braille
  pixel art on the left of a framed pane, gradient sky → blue → navy →
  muted colours from the rantai-agents web palette (#5eb8ff / #3b8cff /
  #040b2e / #6b7280). Adapts to terminal width: full splash at ≥80
  cols, smaller `slant` font fallback at ≥60, single-line banner
  below that. (#40)
- **Polished interactive TUI** — empty-chat splash, Hermes-style
  bottom status bar (`$ model │ tokens/window pct% │ N msgs │ session
  age`), Braille spinner during streaming, rounded brand borders, sky
  highlight on the input prefix `▎ $ you`, `Rantaiclaw v… · session
  …` header chip. (#40)
- **Slash-command autocomplete dropdown** — pops up the moment the
  input buffer starts with `/`, filters by prefix on every keystroke.
  Two-column layout: command name in sky-blue, description in muted
  gray, truncated with `…`. Tab completes; Enter completes-then-submits;
  Esc dismisses; ↑/↓ navigates. (#40)
- **`/help` modal overlay** — Claude-Code-style centered panel with
  `general` + `commands` tabs (Tab to switch, Esc to close), sky-blue
  active-tab chip, brand-coloured shortcut grid, repo URL footer. (#40)
- **WhatsApp Web QR is now actually scannable** — new
  `src/channels/qr_terminal.rs` renders `Event::PairingQrCode`
  payloads as Unicode block-character QR via the `qrcode` crate
  (added as a hard dep), framed and printed to stderr with header +
  footer. Works on any modern terminal; user can point a phone at it
  during pairing. Same module formats the human-readable pair-code
  flow. (#41)
- **Doctor `channels.auth` runs real probes** — Telegram `getMe`,
  Discord `users/@me`, Slack `auth.test`, WhatsApp Cloud
  `/v18.0/<phone_id>`, WA Web session-DB existence. 5s timeout per
  channel. `--offline` falls back to the original config-only sanity
  scan. (#42)
- **MCP zero-auth servers validate at setup time** — same
  `validate_mcp_startup` spawn-and-wait the authed branch already had,
  so a missing `npx`/`uvx` surfaces during `setup mcp` instead of at
  first agent run. (#42)
- **Approval-preset round-trip self-check** — after writing
  `autonomy.toml` / `command_allowlist.toml` / `forbidden_paths.toml`,
  the writer re-reads each freshly-written file and verifies the
  schema. Catches preset-bundle drift, schema mismatches, and
  encoding bugs at setup time. Files the call did NOT freshly write
  (idempotent no-op on user-edited content) are skipped. (#42)
- **Setup-time warning when WhatsApp Web is selected without
  `--features whatsapp-web`** — yellow warn block + rebuild
  instructions + explicit confirmation prompt (default no), so users
  cannot silently configure something that won't run. (#42)

### Fixed

- **`.secret_key` migration left encrypted api_keys un-decryptable** —
  v0.5.0's flat-to-profile migration moved `config.toml` but left
  `~/.rantaiclaw/.secret_key` behind; the SecretStore derived its key
  path from `config_path.parent()` and spawned a fresh key in the
  profile dir, leaving the encrypted blob un-decryptable on next
  launch. Migration's movables list now includes `.secret_key`,
  `secrets/`, and defensively `.onboard_progress`. (#39)
- **ClawHub install was always writing a stub SKILL.md** — old
  `install_one` looked for a `latestVersion.readme` field that the
  live API does not expose, so every "install from ClawHub" produced
  a one-line placeholder. New three-step fetch walks
  `GET /skills/:slug → version` → `GET /skills/:slug/versions/:v →
  files[*]` → per-file fetch with SHA-256 verify against the
  manifest. Path-traversal guard, capped exponential 429 backoff,
  partial-install cleanup on failure. (#41)
- **`ClawHubSkill.tags` shape mismatch** — upstream returns
  `{ "latest": "x.y.z" }`, Rust expected `Vec<String>`,
  `serde(default)` silently emptied it. Retyped to
  `serde_json::Value`. (#41)
- **Plain Enter wasn't submitting prose in the TUI** — the input
  handler only submitted on slash-command buffers; for prose it
  inserted a newline silently, leaving users unable to send a message
  on terminals that don't pass `Ctrl+Enter` as a Ctrl-modified
  KeyCode::Enter. Now plain Enter always submits; multi-line via
  Ctrl+J or Shift+Enter. (#43)
- **`tracing::warn!` from agent path corrupted the TUI alt-screen** —
  no subscriber was installed for the bare `rantaiclaw` launch path,
  so tracing fell through to default-stderr and warnings interleaved
  with the chat. Routed to `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log`
  via a per-launch file writer. `RUST_LOG` still honoured. (#43)
- **`hardware`/`peripheral-rpi`/`probe` features failed to compile**
  because `firmware/rantaiclaw-arduino/zeroclaw-arduino.ino` was the
  legacy filename. Renamed to match the rust `include_str!` path. (#43)

### Changed

- **Friendlier chat-side errors when an agent turn fails** — the
  TUI's `finalize_error` now recognises common shapes (API key
  missing, rate limit, model unavailable) and rewrites them into a
  short actionable block. Unknown errors fall through verbatim with
  the multi-attempt tail compacted to "+N more attempts". (#43)
- `RenderTheme::default()` now uses the rantai-agents brand palette
  (sky / blue / mint / coral / muted) instead of generic 8-colour
  ANSI, so the TUI matches the splash. (#40)
- `CommandResult::Message` outputs now land in chat history (as a
  `system`-role message) instead of the one-line status bar slot.
  Previous behaviour silently truncated long output and disappeared
  on the next keystroke. (#40)

### Compatibility

- All v0.5.0 / v0.5.1 configs continue to load; the migration fix in
  #39 only affects users who hadn't yet hit the bug.
- `--all-features` builds **except** `channel-matrix` (matrix-sdk 0.16
  hits Rust's recursion limit; needs an upstream `#![recursion_limit]`
  bump). The exclusion is documented inline in `Cargo.toml`.

## [0.5.1] — 2026-04-28

Linux portability hotfix.

### Fixed

- **Release Linux binaries built against GLIBC 2.35 baseline** — v0.4.x and
  v0.5.0 release artifacts were built on `ubuntu-latest` (Ubuntu 24.04, GLIBC
  2.39) and refused to load on Ubuntu 22.04 LTS, Debian 12, RHEL 9, and most
  other LTS distros with `libc.so.6: version 'GLIBC_2.39' not found`. All
  three Linux runners (x86_64, aarch64, armv7) are now pinned to
  `ubuntu-22.04` so artifacts run on every modern distro from Ubuntu 22.04
  onward. (#38)

## [0.5.0] — 2026-04-28

The "onboarding depth v2" release. The setup story is now modular,
re-runnable, and policy-aware end to end. Existing flat-layout
configurations migrate automatically on first run.

### Added

- **Profile system** (Wave 1) — multi-profile storage layout under
  `~/.rantaiclaw/profiles/<name>/`. New `rantaiclaw profile {list, create,
  use, clone, delete, current}` subcommands plus a global `--profile <name>`
  flag (precedence: CLI flag > `RANTAICLAW_PROFILE` env > `active_profile`
  file > `default`). Legacy single-profile installs auto-migrate on first
  load via `ProfileManager::ensure_default`.
- **Approval gate + audit log** (Wave 2A) — every tool call now flows
  through a single approval policy gate (`src/approval/`) before
  execution. Policy combines an autonomy mode, a command allowlist, and a
  forbidden-paths list; decisions are written to a tamper-evident audit
  log under `<profile>/audit/`. Security tests cover bypass, path
  escapes, and approval-gate edge cases.
- **`rantaiclaw doctor`** (Wave 2B) — diagnostics across config,
  policy, system deps, and daemon registration. Three output formats
  (`text`, `json`, `brief`); each finding ships with an actionable hint.
- **Persona presets + interview** (Wave 2C) — five curated presets
  (default, executive-assistant, friendly-companion, research-analyst,
  concise-pro) plus an interactive interview that renders `persona.toml`
  and a `SYSTEM.md` prompt. Snapshot tests lock the rendering.
- **Skills starter pack + ClawHub** (Wave 2D) — 5-skill bundled starter
  pack (web-search, scheduler-reminders, summarizer, research-assistant,
  meeting-notes) installed in headless mode; ClawHub multi-select picker
  for additional skills, sorted by stars.
- **MCP curated picker** (Wave 2E) — 9 vetted servers (3 zero-auth,
  6 authenticated) with inline auth flow during setup.
- **Setup orchestrator** (Wave 3) — `rantaiclaw setup [<topic>] [--force]
  [--non-interactive]` walks the canonical six-section list (provider →
  approvals → channels → persona → skills → mcp) or dispatches to a
  single section. `--non-interactive` makes every section emit a hint
  and continue, suitable for CI / scripted bootstraps.
- **L1-L4 policy presets** (Wave 4A) — `rantaiclaw setup approvals`
  picks between L1 Strict, L2 Smart, L3 Trusted, and L4 Auto presets;
  per-agent overrides supported via `[agents.<name>.autonomy]`.
- **Daemon handoff on profile switch** (Wave 4B) — `profile use` now
  signals a running daemon to drain and re-launch under the new
  profile via a sentinel file; daemon lifecycle hooks write/clear it
  on start/stop.
- **OpenClaw / ZeroClaw migration** (Wave 4C) — `rantaiclaw migrate
  --from {openclaw, zeroclaw, auto}` imports config + workspace from a
  legacy install into a fresh profile. `--include-secrets` is opt-in;
  the source is never deleted.
- **End-to-end smoke tests** (Wave 5) — `tests/setup_e2e.rs` drives the
  compiled binary through `setup --non-interactive` and `doctor --brief`
  against a temp `$HOME`, asserting every section runs and the doctor
  surfaces the expected gaps.

### Changed

- `rantaiclaw onboard` is now a legacy alias for `rantaiclaw setup`. It
  prints a one-line `note:` and continues to work through v0.5.0; new
  recipes should use `setup`.
- `scripts/bootstrap.sh` post-install banner now points at `rantaiclaw
  setup` (and `rantaiclaw doctor`) instead of `rantaiclaw onboard
  --interactive`.

### Breaking

- **Storage layout migration.** Configs and workspace files move from
  the flat `~/.rantaiclaw/{config.toml, workspace/, ...}` layout to the
  per-profile `~/.rantaiclaw/profiles/<name>/{config.toml, workspace/,
  ...}` layout. The migration is automatic on first run; the old paths
  are left in place as compatibility symlinks for at least one release.
- The canonical setup section list grew from five to six (added
  `approvals` between `provider` and `channels`). Tests pinning the
  list size or order need to be updated alongside.

### Compatibility

- v0.4.x configs auto-migrate on first run (`tests/compat_v041_to_v050.rs`,
  `tests/migrate_legacy.rs`).
- All Wave 2-4 security tests pass; the approval gate is the only path
  between LLM tool emission and execution.
