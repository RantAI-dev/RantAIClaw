# Changelog

All notable changes to RantaiClaw are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
