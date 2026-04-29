# Changelog

All notable changes to RantaiClaw are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
