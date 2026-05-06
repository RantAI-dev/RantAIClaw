# Onboarding Depth v2 — Design Spec

**Status:** Approved by maintainer 2026-04-27, ready for implementation plan.
**Target release:** v0.5.0
**Branch:** `feat/onboarding-depth-v2`
**Reference baselines:** [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent), [openclaw/openclaw](https://github.com/openclaw/openclaw)

## Motivation

v0.4.2 fixed installation: `curl|bash` now drops a checksum-verified binary in the user's PATH. But after install, the user still has to remember to run `rantaiclaw onboard --interactive` themselves, and even when they do, the wizard collects 9 things and then leaves the bigger capabilities (skills, MCP servers, persona, daemon, autonomy) at silent defaults. By comparison, Hermes auto-runs its setup wizard at the end of `install.sh` and walks the user through 11+ capability-shaping steps.

This spec brings RantaiClaw's first-run experience to genuine Hermes parity (and ahead of it where RantaiClaw's existing surface — multi-channel, gateway_agents, ClawHub — gives us the option). Goal: after a single `curl|bash`, the user has a configured provider, a chosen persona, an installed starter skill pack, MCP servers wired up with auth, optionally a running daemon, and a clean `rantaiclaw doctor` report — without ever having to read a setup guide.

## Confirmed scope decisions

Ten clarifying questions answered before this spec was written.

| Decision | Locked outcome |
|---|---|
| Auto-trigger setup from `bootstrap.sh` | Yes; `--skip-setup` opt-out; equivalent to Hermes' `run_setup_wizard`. |
| Architecture | Hybrid: monolithic first-run wizard + modular `rantaiclaw setup <topic>` subcommands. |
| Skills bootstrap | Curated 5-skill general-assistant starter pack opt-in, then ClawHub multi-select. **No coding skills in the starter pack.** |
| MCP discovery | Curated 6-server multi-select with inline auth/OAuth collection. |
| Persona | 5-preset picker + 2–3 question interview, woven into a rendered `SYSTEM.md`. |
| CLI profiles | Yes — full `rantaiclaw profile create/list/use/clone/delete` with `-p <name>` global flag. |
| Voice | Skipped in v1. |
| Approval/autonomy | Hermes-style runtime accretion as default + L1–L4 presets + per-`gateway_agent` overrides + `mode = strict` for unattended/enterprise + audit log always on. |
| `rantaiclaw doctor` | Full depth: config schema + live API + system deps. |
| Daemon install offer | Conditional — only when at least one channel was configured. |
| Migration | `rantaiclaw migrate from-openclaw` (no `config export/import` in v1). |

## Architecture overview

`bootstrap.sh` ends with an automatic invocation of `rantaiclaw onboard --interactive` (Hermes pattern, with `--skip-setup` opt-out). `onboard` becomes a thin **orchestrator** that dispatches to 13 **setup section modules**, each independently exposed as `rantaiclaw setup <topic>` so users can re-run any single section later. Each section reads/writes the **profile-aware config** at `~/.rantaiclaw/profiles/<name>/config.toml` (default profile = `default`); the storage layout is profile-aware from day one even if users never create a second profile. Three new top-level commands ship alongside: `rantaiclaw doctor` (full-depth health check with config + live API + system deps), `rantaiclaw profile {create,list,use,clone,delete}` (CLI profile switcher), and `rantaiclaw migrate from-openclaw` (one-shot importer). The runtime gets a **command-approval accretion loop** — `[once / session / always / deny]` prompt for unmatched commands, with `[a]lways` appending to `~/.rantaiclaw/profiles/<name>/policy/command_allowlist.toml`. **Audit log** writes to `~/.rantaiclaw/profiles/<name>/audit.log` for every executed tool call regardless of approval mode.

### Five new top-level concepts

| Concept | Code home | Responsibility |
|---|---|---|
| **Setup section trait** | `src/onboard/section/mod.rs` | Common interface: `name()`, `summary()`, `is_already_configured()`, `run(ctx)`, `headless_hint()`. |
| **Setup orchestrator** | `src/onboard/orchestrator.rs` | Walks the section list, handles resume/skip-already-configured, persists between sections, prints section banner & step counter. |
| **Profile manager** | `src/profile/mod.rs` | Resolves `~/.rantaiclaw/profiles/<name>`, creates/lists/clones/deletes profiles, exposes a `Profile` struct that owns config + paths for memory/sessions/skills/policy. |
| **Doctor runner** | `src/doctor/mod.rs` | Composable check trait + bundled checks (config schema, provider ping, channel auth probe, MCP health, daemon registration, system deps). |
| **Approval gate** | `src/approval/mod.rs` | Runtime check called by the agent loop before tool execution; consults `mode` + allowlist; emits `[o/s/a/d]` prompt in interactive contexts; falls back to allowlist-only in headless. |

### Bootstrap-to-onboard wiring

`scripts/bootstrap.sh` adds a `maybe_run_setup()` step (mirrors Hermes' `run_setup_wizard`) — runs `rantaiclaw onboard --interactive` only when stdin and stdout are both TTYs, redirecting stdin from `/dev/tty` so curl|bash works. New flags: `--skip-setup` opts out; `RANTAICLAW_SKIP_SETUP=1` env var equivalent.

### Backward compatibility

Users with existing `~/.rantaiclaw/{config.toml, workspace/}` get a one-shot migration to `~/.rantaiclaw/profiles/default/...` on the next launch. Old paths become symlinks across the v0.5.0 → v0.7.0 deprecation window: created in v0.5.0 (silent fallback), warn-on-direct-access in v0.6.0, removed in v0.7.0.

## Components

Concrete file-level breakdown. New files marked **NEW**; existing files extended marked **EXT**.

### Bootstrap layer

| File | NEW/EXT | Responsibility |
|---|---|---|
| `scripts/bootstrap.sh` | EXT | Add `maybe_run_setup()` invoked at end. New flags: `--skip-setup`, `RANTAICLAW_SKIP_SETUP=1`. Detects TTY, redirects from `/dev/tty`. |
| `scripts/install.sh` | EXT | Pass-through new flags. |
| `docs/install.md` | EXT | Document auto-setup behavior + opt-out. |

### Profile manager

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/profile/mod.rs` | NEW | `Profile` struct, `ProfileManager`, path resolution, default profile bootstrap. |
| `src/profile/migration.rs` | NEW | One-shot legacy migrator with transitional symlinks. |
| `src/profile/commands.rs` | NEW | CLI subcommands `create / list / use / clone / delete`. |
| `src/main.rs` | EXT | New `Commands::Profile` enum branch. Add `-p, --profile <name>` global flag. |
| `src/config/loader.rs` | EXT | Profile-aware config resolution. |

### Setup orchestrator + sections

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/onboard/section/mod.rs` | NEW | `SetupSection` trait. |
| `src/onboard/orchestrator.rs` | NEW | Walks section list, handles `[skip / configure / re-configure]`, persists after each `run`, renders step counter. |
| `src/onboard/wizard.rs` | EXT | Becomes thin caller building section list and handing it to `Orchestrator::run`. |
| `src/onboard/section/workspace.rs` | NEW | Section: workspace setup. |
| `src/onboard/section/provider.rs` | NEW | Provider + model + API key (live-validated). |
| `src/onboard/section/persona.rs` | NEW | Preset picker (5 presets) + 2–3 question interview, renders templated `SYSTEM.md` + `persona.toml`. |
| `src/onboard/section/skills.rs` | NEW | Curated 5-skill starter pack opt-in, then ClawHub multi-select. |
| `src/onboard/section/mcp.rs` | NEW | Curated 6-server multi-select with inline auth/OAuth. |
| `src/onboard/section/channels.rs` | NEW | Channels (existing logic relocated). |
| `src/onboard/section/tunnel.rs` | NEW | Tunnel (existing logic relocated). |
| `src/onboard/section/tools.rs` | NEW | Tool mode + secrets encryption. |
| `src/onboard/section/hardware.rs` | NEW | Hardware probe (existing). |
| `src/onboard/section/memory.rs` | NEW | Memory backend + retention. |
| `src/onboard/section/project_context.rs` | NEW | Name/timezone/style; triggers persona re-render if persona section already ran. |
| `src/onboard/section/workspace_files.rs` | NEW | Workspace scaffolding. |
| `src/onboard/section/daemon.rs` | NEW | Conditionally offer `service install` if any channel was configured. |
| `src/onboard/section/doctor_handoff.rs` | NEW | End-of-wizard final step → run `rantaiclaw doctor` and print summary. |
| `src/main.rs` | EXT | `Commands::Setup { topic: SetupTopic }` branch with `SetupTopic` enumerating each section. |

### Approval / autonomy runtime

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/approval/mod.rs` | NEW | `ApprovalGate::check(tool, args, ctx) -> Decision { Allow, Deny }`. |
| `src/approval/prompt.rs` | NEW | Renders `[once / session / always / deny]` UI; pipe-safe via `/dev/tty`. |
| `src/approval/allowlist.rs` | NEW | Glob-matched read/write of `policy/command_allowlist.toml`; appends on `[a]lways` via `toml_edit` (preserves comments). |
| `src/approval/presets.rs` | NEW | L1/L2/L3/L4/custom preset definitions; expands a level into `(mode, command_allowlist, forbidden_paths)`. |
| `src/approval/yolo.rs` | NEW | `/yolo` slash-command handler; in-memory toggle for current session. |
| `src/audit/mod.rs` | NEW | Append-only writer to `~/.rantaiclaw/profiles/<name>/audit.log` with timestamp, agent_id, tool, command, decision, match-source. Includes redaction rules. |
| `src/agent/loop_.rs` | EXT | Wire `ApprovalGate::check` before tool execution; emit `audit::log` after. |
| `src/config/schema.rs` | EXT | Add `[autonomy]`, `[approvals]`, `command_allowlist`, `forbidden_paths`, plus per-`gateway_agents.<name>.{autonomy,approvals}` overrides. |

### Doctor command

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/doctor/mod.rs` | NEW | `DoctorCheck` trait, `Severity { Ok, Warn, Fail, Info }`, registry, `run_all()` reporter. |
| `src/doctor/checks/config.rs` | NEW | Schema validation, path existence, internal consistency. |
| `src/doctor/checks/provider.rs` | NEW | Pings `/models`, validates API key, checks model exists in catalog. |
| `src/doctor/checks/channels.rs` | NEW | Per-channel auth probe. |
| `src/doctor/checks/mcp.rs` | NEW | Spawn each configured MCP server, wait for `initialize` ack, then shutdown. |
| `src/doctor/checks/daemon.rs` | NEW | Detect `service install` state via systemd/launchd query. |
| `src/doctor/checks/system_deps.rs` | NEW | git/curl/tar/sha256sum/cosign/docker presence + versions. |
| `src/doctor/checks/policy.rs` | NEW | Validate `command_allowlist.toml`; warn on `mode=strict + empty allowlist`. |
| `src/main.rs` | EXT | `Commands::Doctor { format: Json|Text|Brief }` branch. |

### Migration

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/migrate/mod.rs` | NEW | `MigrateCommand` enum + dispatcher. |
| `src/migrate/openclaw.rs` | NEW | Detects `~/.openclaw`, dry-run preview, imports persona/memory/skills/channel tokens. |
| `src/main.rs` | EXT | `Commands::Migrate { source: MigrateSource }` branch. |

### Skills bootstrap

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/skills/bundled/` | NEW | Embed 5 starter skills as `include_str!` resources: `web-search`, `scheduler-reminders`, `summarizer`, `research-assistant`, `meeting-notes`. |
| `src/skills/clawhub.rs` | EXT | Add `list_top(n)` and `install_many(slugs)` for the multi-select picker. |

### MCP discovery

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/mcp/curated.rs` | NEW | Static list of curated MCP server definitions: name, install command, required secrets, OAuth URL helper. |
| `src/mcp/setup.rs` | NEW | Multi-select picker, per-server auth collection. |

### Persona

| File | NEW/EXT | Responsibility |
|---|---|---|
| `src/persona/mod.rs` | NEW | Persona type, preset registry, template renderer. |
| `src/persona/presets/` | NEW | 5 markdown templates: `default.md`, `concise_pro.md`, `friendly_companion.md`, `research_analyst.md`, `executive_assistant.md`. |

**Total: ~50 new files, ~10 extended files.** Estimated diff: 4–6k lines.

## Data flow

### First-run flow (canonical happy path)

```
User runs: curl -fsSL .../bootstrap.sh | bash
│
├─ bootstrap.sh
│  ├─ Detect platform → download rantaiclaw-<target>.tar.gz
│  ├─ SHA256 verify against published SHA256SUMS
│  ├─ Install binary → ~/.cargo/bin or ~/.local/bin
│  ├─ Auto-amend shell rc with PATH export (rustup pattern)
│  └─ maybe_run_setup()                                  ← NEW
│     └─ if [ -t 0 ] && [ -t 1 ] && ! --skip-setup
│        └─ exec rantaiclaw onboard --interactive < /dev/tty
│
├─ rantaiclaw onboard --interactive                      ← orchestrator
│  ├─ ProfileManager::ensure_default()                   → creates ~/.rantaiclaw/profiles/default/
│  ├─ Print welcome banner
│  └─ Orchestrator::run(SECTION_LIST, &mut SetupContext)
│     │
│     ├─ Section 1/13   workspace
│     ├─ Section 2/13   provider
│     ├─ Section 3/13   persona              ← NEW
│     ├─ Section 4/13   skills               ← NEW
│     ├─ Section 5/13   mcp                  ← NEW
│     ├─ Section 6/13   channels
│     ├─ Section 7/13   tunnel               (only if any channel set)
│     ├─ Section 8/13   tools
│     ├─ Section 9/13   hardware
│     ├─ Section 10/13  memory
│     ├─ Section 11/13  project_context      (re-renders persona if applicable)
│     ├─ Section 12/13  workspace_files
│     └─ Section 13/13  daemon (conditional) ← NEW (offered iff ≥1 channel was configured)
│
├─ Doctor handoff                                        ← NEW
│  └─ rantaiclaw doctor --brief
│
└─ Print success banner with first commands.
```

### State machine inside the orchestrator

For each section: check `is_already_configured(&profile)`. If no, run interactively. If yes, prompt `[skip / reconfigure / show value]`. After each `section.run()`, write the diff to `config.toml.staging`; on Ctrl+C, staging file preserved with `.onboard_progress` for resume; on normal exit, atomic rename `staging → config.toml`.

### Approval-gate runtime data flow (per tool call)

The agent loop calls `ApprovalGate::check(tool, args, ctx)` before every tool execution. The check pipeline:

1. **Forbidden-path check** (always first, can never be bypassed): if any path-arg matches `forbidden_paths.toml` globs → `Deny "forbidden_path: <pattern>"`.
2. **Session-yolo override** (in-memory only): if `session_yolo == on` → `Allow "yolo"`.
3. **Effective config resolution**: load `gateway_agents.<active_agent>.{autonomy,approvals} ?? root.{autonomy,approvals}`. Expand `level` to `(mode, allowlist, forbidden_paths)` (see §6.1 below).
4. **Explicit allowlist match**: if any glob in effective allowlist matches `<tool> <args>` → `Allow + audit "allowlist_hit"`. If any glob in `session_allowlist` matches → `Allow + audit "session_allowlist"`.
5. **Mode-dependent fallback**:
   - `off` → `Allow + audit "off"`
   - `strict` → `Deny + audit "strict_no_match"`
   - `smart` → LLM risk eval (low → Allow, high → recurse to manual fallback, deny → Deny)
   - `manual` →
     - **interactive** (stdin TTY OR `/dev/tty` available): render `[o/s/a/d]` prompt; on `[a]lways`, append glob to `allowlist.toml`.
     - **headless** (no TTY): `Deny + audit "manual_headless_deny"`.

After execution: `audit_writer.log(AuditEntry { ts, agent_id, profile, tool, args_redacted, decision, match_source, duration_ms, exit_code })`.

### Profile-aware path resolution

Active profile resolved by precedence (first match wins):
1. `-p, --profile <name>` (CLI flag)
2. `RANTAICLAW_PROFILE` env var
3. `~/.rantaiclaw/active_profile` (file)
4. `"default"`

For profile `<p>`, paths are:
```
~/.rantaiclaw/profiles/<p>/
  config.toml
  persona/{SYSTEM.md, persona.toml}
  workspace/
  memory/
  sessions/
  skills/
  policy/{command_allowlist.toml, forbidden_paths.toml}
  audit.log
  secrets/{api_keys.toml, channel_tokens.toml}
  runtime/{pid.lock, gateway.sock}
```

### Migration trigger paths

**Legacy migration** (automatic): on every startup, if `~/.rantaiclaw/config.toml` exists but `~/.rantaiclaw/profiles/` doesn't, atomically move flat layout to `profiles/default/`, write transitional symlinks, write `MIGRATION_NOTICE.md`, log to stderr.

**OpenClaw migration** (explicit): `rantaiclaw migrate from-openclaw` detects `~/.openclaw`, builds dry-run plan, prompts for confirmation, applies on Yes. See §7.2.

## Storage layout

The exact disk shape after a fresh install with one profile, one extra cloned profile, OpenClaw migration applied, and approval accretion exercised. Key invariants: profiles are self-contained; clone defaults preserve config + persona + skills but not memory/secrets/sessions; runtime files never copy.

```
~/.rantaiclaw/
├── active_profile                        # plain text: "personal"
├── version                               # "0.5.0"
├── README.md                             # one-liner pointing to `rantaiclaw --help`
├── profiles/
│   ├── default/                          # auto-created on first run
│   │   ├── config.toml
│   │   ├── config.toml.staging           # only present mid-wizard
│   │   ├── persona/{SYSTEM.md, persona.toml, imported_openclaw.md?}
│   │   ├── workspace/{README.md, AGENTS.md, TOOLS.md}
│   │   ├── memory/{MEMORY.md, USER.md, memory.db?, imported.md?}
│   │   ├── sessions/<ts>_<ulid>.jsonl
│   │   ├── skills/{web-search,scheduler-reminders,summarizer,research-assistant,meeting-notes,...}
│   │   ├── policy/{command_allowlist.toml, forbidden_paths.toml}
│   │   ├── audit.log
│   │   ├── secrets/{api_keys.toml, channel_tokens.toml}     # mode 0700
│   │   └── runtime/{pid.lock, gateway.sock}
│   └── work/                             # cloned via `profile create work --clone default`
│       └── ...
├── cache/                                # global, profile-agnostic
│   ├── models/openrouter.json
│   ├── clawhub/top-skills.json
│   └── doctor/last_run.json
└── logs/rantaiclaw-<date>.log
```

### Profile clone semantics

`rantaiclaw profile create <new> --clone <src>`:

| File / dir | Cloned by default | `--include-secrets` | `--include-memory` |
|---|---|---|---|
| `config.toml` (non-secret keys) | yes | — | — |
| `persona/` | yes | — | — |
| `policy/forbidden_paths.toml` | yes | — | — |
| `policy/command_allowlist.toml` | no (fresh start safer) | — | — |
| `skills/` (curated + user-installed) | yes (hardlinks where possible) | — | — |
| `secrets/` | no | yes via flag | — |
| `memory/` | no | — | yes via flag |
| `sessions/` | no | — | — |
| `audit.log` | no | — | — |
| `runtime/` | no never | — | — |
| `workspace/` | no (empty workspace in clone) | — | — |

### File format choices

- `config.toml` — TOML, schema-validated.
- `persona/persona.toml` — TOML metadata; renders `SYSTEM.md`.
- `persona/SYSTEM.md` — markdown; readable, editable, fed verbatim into agent system prompt.
- `policy/*.toml` — TOML with comments preserved on round-trip via `toml_edit`.
- `audit.log` — JSONL append-only, one line per executed tool call.
- `sessions/*.jsonl` — JSONL, one line per turn.
- `secrets/*.toml` — TOML; encrypted with libsodium when `secrets.encrypt = true`.

### Forbidden-paths defaults (seeded on first run)

```toml
forbidden = [
  "/etc/**",
  "/sys/**", "/proc/**",
  "~/.ssh/**", "~/.gnupg/**",
  "~/.aws/**", "~/.azure/**", "~/.config/gcloud/**",
  "~/.kube/**",
  "~/.rantaiclaw/secrets/**",
  "~/.rantaiclaw/profiles/*/secrets/**",
]
```

## Wizard step sequence

13 sections; each with run condition, prompts, side effects, and headless behavior.

### Section 1 — workspace (existing logic)

- **Run:** always. Silent.
- **Side effects:** create `profiles/<name>/{workspace,memory,sessions,skills,persona,policy,secrets,runtime}/`. Write `forbidden_paths.toml` defaults. Write empty `command_allowlist.toml`.

### Section 2 — provider (existing logic)

- **Prompts:** provider picker → masked API key (live-validated) → model picker (cached 24h) → temperature.
- **Headless:** `--provider`, `--model`, `--api-key` flags or env vars; if missing, exits with hint.

### Section 3 — persona (NEW)

- **Prompts:**
  1. Pick a persona: `[default | concise_pro | friendly_companion | research_analyst | executive_assistant]` with one-line descriptions.
  2. Primary role for this agent (one sentence) — woven into prompt.
  3. Tone: `formal | neutral | casual` (default neutral).
  4. Anything to avoid? (optional free text).
- **Side effects:** writes `persona/persona.toml` + renders `persona/SYSTEM.md` from chosen template with `{{name}}, {{timezone}}, {{role}}, {{tone}}, {{avoid}}` substituted.
- **Headless:** `--persona-preset <id>` or skip with default template.

### Section 4 — skills (NEW)

- **Prompts:**
  1. `Install the recommended starter pack? (5 skills) [Y/n]`. Pack: `web-search, scheduler-reminders, summarizer, research-assistant, meeting-notes`.
  2. `Browse ClawHub for more skills? [y/N]`. If yes, fetch top 20 from `clawhub.ai/api/v1/skills?sort=stars`, render multi-select.
- **Side effects:** copy bundled starter pack into `skills/<name>/SKILL.md`; install ClawHub picks via existing machinery.
- **Headless:** `--skip-skills` short-circuits; `--starter-pack` installs all 5; ClawHub multi-select interactive-only.

### Section 5 — mcp (NEW)

- **Prompts:**
  1. `Install zero-auth MCP servers? (web-fetch, time, filesystem) [Y/n]`.
  2. Multi-select among `Notion (token), Google Drive (OAuth), Slack (token), Google Calendar (OAuth), Gmail (OAuth), GitHub (token)`.
  3. For each picked, inline auth: token-based → masked input + spawn-and-validate; OAuth-based → opens local OAuth flow on `http://localhost:11500/oauth/<server>`, captures token.
- **Side effects:** appends to `mcp_servers` block; writes secrets to `secrets/api_keys.toml`.
- **Headless:** prints `rantaiclaw mcp add <name>` hint per server; doesn't install.

### Section 6 — channels (existing)

- **Prompts:** opt-in multi-select over 14 channels (existing logic preserved); per-channel token + auth probe.
- **Side effects:** writes `channels_config` block; tokens to `secrets/channel_tokens.toml`.

### Section 7 — tunnel (existing, conditional)

- **Run:** only if at least one channel needs webhook delivery (Discord HTTP, Slack events, Telegram webhook mode).
- **Prompts:** mode picker + token if ngrok/cloudflared.

### Section 8 — tools (existing)

- **Prompts:** Composio toggle + secrets encryption.

### Section 9 — hardware (existing, opt-in default off)

- **Prompts:** probe + attach STM32 / Arduino / RPi GPIO.

### Section 10 — memory (existing)

- **Prompts:** backend (`sqlite | markdown | lucid | none`) + auto-save + retention.

### Section 11 — project_context (existing)

- **Prompts:** name, timezone, communication style.
- **Side effects:** writes `project_context` block; **also re-renders `persona/SYSTEM.md`** (because name + timezone are persona substitutions).

### Section 12 — workspace_files (existing)

- **Side effects:** scaffolds `workspace/{README.md, AGENTS.md, TOOLS.md}`.

### Section 13 — daemon (NEW, conditional)

- **Run:** only if at least one channel was configured.
- **Prompts:**
  1. `Install rantaiclaw as a background service so the gateway auto-starts on boot? [Y/n]`.
  2. If yes → detect platform, invoke `rantaiclaw service install --service-init <detected>`.
  3. `Start it now? [Y/n]`.
- **Headless:** `--install-daemon` / `--no-install-daemon`; defaults to NO non-interactively.

### Doctor handoff (after all sections)

Always runs `rantaiclaw doctor --brief`; non-zero exit on any FAIL with actionable hints.

### Resume mid-wizard

If `config.toml.staging` exists, orchestrator prompts: `Found a partial setup from <ts ago>. Resume? [Y/n/s]`.

### Re-running a single section

`rantaiclaw setup <topic>` loads existing config, runs only the named section, with the same `[skip / reconfigure / show value]` UX. `rantaiclaw setup` (no topic) re-runs full wizard.

## Approval runtime

Four layers compose: forbidden_paths (always-deny) → session-yolo (in-memory) → explicit allowlist match → mode-dependent fallback. Everything that executes — allowed or denied — gets logged to `audit.log`.

### 6.1 Decision pipeline

(See data-flow section above for the full pseudocode.) Effective config resolution:

```rust
let auto = gateway_agents.<id>.autonomy.unwrap_or(root.autonomy);
let appr = gateway_agents.<id>.approvals.unwrap_or(root.approvals);
let (mode, allowlist, forbidden) = if auto.level == "custom" {
    (appr.mode, appr.command_allowlist, root.forbidden_paths)
} else {
    expand_preset(auto.level)  // L1..L4
};
```

Preset expansion:

- **L1** → `mode=manual`, `allowlist = root_allowlist`. Most restrictive — ask everything.
- **L2** → `mode=manual`, `allowlist = root_allowlist + read_only_seeds = ["file_read", "memory_*", "web_search", "tool_search", "list_*"]`.
- **L3** → `mode=manual`, `allowlist = read_only_seeds + safe_write_seeds = ["memory_write", "skill_install", "cron_*", "session_*"]`.
- **L4** → `mode=off`.

### 6.2 Inline prompt UI

```
┌─ Approval needed ─────────────────────────────────────────────┐
│ Tool:    shell                                                │
│ Command: rm -rf node_modules                                  │
│ Working dir: /home/shiro/myproject                            │
│ Agent: default-agent (profile: personal)                      │
└───────────────────────────────────────────────────────────────┘

  [o] once    — allow this single execution
  [s] session — allow pattern for the rest of this session
  [a] always  — add `rm -rf node_modules` to permanent allowlist
  [d] deny    — block this command (default after 60s)

> _
```

On `[a]lways`, a follow-up prompt offers glob choices (exact, two suggested wider globs, custom). Default = exact.

### 6.3 `/yolo` slash command

Mid-session toggle (in-memory only): `/yolo`, `/yolo on`, `/yolo off`, `/yolo for 10` (auto-disable after N tool calls). No-op with warning when `mode=strict` at gateway-agent level or when running headless. Always logged to `audit.log` with `match_source: "yolo"`.

### 6.4 Per-`gateway_agent` overrides

The `gateway_agents.<id>.{autonomy,approvals}` blocks shadow root. Resolution per-tool-call. Example:

```toml
[autonomy]
level = "L1"

[approvals]
mode = "manual"
timeout = 60

[gateway_agents.support_bot]
workspace_dir = "/var/lib/rantaiclaw/support"
[gateway_agents.support_bot.autonomy]
level = "custom"
[gateway_agents.support_bot.approvals]
mode = "strict"
command_allowlist = ["search_tickets *", "draft_reply *", "fetch_kb *"]
```

### 6.5 Smart-mode risk evaluator

Optional, only fires on `mode=smart`. Calls a small classifier prompt at the configured provider; 5s timeout falls through to manual; result cached 1h by `(tool, normalized_args)` hash; ~$0.0001 per check.

### 6.6 Audit log

Append-only JSONL:

```json
{"ts":"2026-04-27T13:42:11.421Z","profile":"personal","agent_id":"default-agent","tool":"shell","args_redacted":{"cmd":"git status"},"decision":"Allow","match_source":"allowlist_hit","matched_pattern":"git status","duration_ms":47,"exit_code":0}
```

**Redaction rules** (so logs are shareable):

- Path-like args matching `~/.ssh/**`, `~/.aws/**`, `~/.config/gcloud/**` → `[REDACTED]`.
- `Bearer ...`, `password=`, `api_key=`, `token=` → regex-redacted.
- Stdouts elided beyond 4 KB.
- Original args kept in parallel `audit.raw` mode 0600 if `audit.preserve_raw=true` (default false).

### 6.7 Allowlist file format

`profiles/<name>/policy/command_allowlist.toml` — handwritten or accreted, both supported. Accretion uses `toml_edit` to preserve comments.

### 6.8 Forbidden-paths file

Seeded on first run with the safe defaults listed in storage layout. Forbidden-path matches checked against `cwd`-relative resolved absolute paths, glob expansion args, and shell command substrings.

### 6.9 Agent-loop integration point

`src/agent/loop_.rs` — single insertion point before tool dispatch. The gate is the only path between LLM tool-call emission and execution. There is no bypass.

```rust
let decision = approval_gate.check(&tool, &args, &ctx).await?;
let result = match decision {
    Decision::Allow { .. } => Some(tool_executor.execute(&tool, &args).await),
    Decision::Deny { .. } => None,
};
audit_writer.log(AuditEntry::from(&ctx, &tool, &args, &decision, &result)).await?;
return decision_to_tool_response(decision, result);
```

## Migration spec

Two migrations: **legacy-layout** (auto, silent on clean state) and **openclaw** (explicit, dry-run-by-default).

### 7.1 Legacy-layout migration (automatic)

Triggers on every startup before config load. Detection:

```rust
fn needs_legacy_migration() -> bool {
    let root = home_dir().join(".rantaiclaw");
    root.join("config.toml").exists()
        && !root.join("profiles").exists()
        && !root.join("active_profile").exists()
}
```

Plan: move flat `~/.rantaiclaw/{config.toml, workspace/, memory/, sessions/, skills/, ...}` into `~/.rantaiclaw/profiles/default/`. Steps:

1. Acquire `~/.rantaiclaw/migrate.lock` (flock); skip if locked.
2. `mkdir -p ~/.rantaiclaw/profiles/default/`.
3. For each source path: `rename(src, dst)` (atomic on same filesystem; falls back to copy+delete on `EXDEV`).
4. Write `~/.rantaiclaw/active_profile` ← `default`.
5. Write `~/.rantaiclaw/version` ← current binary version.
6. Create transitional symlinks for one release cycle: `~/.rantaiclaw/config.toml → profiles/default/config.toml`, `~/.rantaiclaw/workspace → profiles/default/workspace`.
7. Write `~/.rantaiclaw/MIGRATION_NOTICE.md`.
8. Log to stderr.
9. Release lock.

**Symlink lifecycle:** v0.5.0 created; v0.6.0 warn-on-direct-access; v0.7.0 removed.

**Rollback:** lock + idempotent `rename` means partial state is recoverable on retry.

### 7.2 OpenClaw migration (explicit)

`rantaiclaw migrate from-openclaw [flags]`. Default: dry-run preview, then prompt.

```
$ rantaiclaw migrate from-openclaw

Detected OpenClaw install at /home/shiro/.openclaw

Migration plan (dry-run):
                            existing      action
  Persona (SOUL.md)              ─        copy → profiles/default/persona/imported_openclaw.md
  Memories (MEMORY.md)           yes      append → profiles/default/memory/imported.md
  Memories (USER.md)             ─        copy → profiles/default/memory/imported_user.md
  Skills (12 found)              5        copy 7 → profiles/default/skills/openclaw-imports/
  Channels (3 platforms)         ─        merge tokens (telegram, discord, slack)
  Approval allowlist             ─        merge 47 patterns → policy/command_allowlist.toml
  API keys (4)                   yes      SKIP (use --include-secrets to migrate)
  Workspace files                ─        copy AGENTS.md → workspace/AGENTS.md.imported

Apply migration? [y/N/?] _
```

**Flags:** `--dry-run`, `--yes`, `--overwrite`, `--include-secrets`, `--profile <name>`, `--source <path>`, `--preset {full|user-data|secrets-only}`.

**Per-category logic:**

- **Persona:** copy to `imported_openclaw.md`; never replaces active `SYSTEM.md`.
- **Memories:** `MEMORY.md` appends under heading; `USER.md` saves to `imported_user.md`.
- **Skills:** each skill dir copied; tagged with frontmatter `imported_from: openclaw`. Skip if exists unless `--overwrite`.
- **Channels:** merge tokens (only with `--include-secrets`); preserve paired-user allowlist; channels remain disabled until user enables.
- **Approval allowlist:** patterns appended with `# imported from openclaw` annotation; deduped by glob string.
- **API keys:** detected from `.env` or `secrets.yaml`; mapped to known keys; written to `secrets/api_keys.toml` + `secrets/channel_tokens.toml`; encrypted iff target has `secrets.encrypt=true`.
- **Workspace:** `AGENTS.md → workspace/AGENTS.md.imported`; other files into `workspace/imported_openclaw/`.

**Conflict resolution:** default skip-on-conflict for skills/memory/channels/keys; persona never replaced (always saved as imported); allowlist + forbidden_paths always merged (union); `--overwrite` flips skip → replace.

**Cleanup:** `rantaiclaw migrate cleanup-openclaw` archives + removes (with confirmation); never auto-deletes.

**Idempotence:** marker `~/.openclaw/.migrated_to_rantaiclaw` written on success; re-run prompts.

**Failure handling:** rolls back per-category on disk-full; preserves unmapped keys to `imports/openclaw_unmapped.toml`.

## Error handling, resumability, headless behavior

### 8.1 Interruptions (Ctrl+C, SIGTERM, network drop)

Two-phase commit per section: build `SectionDelta`, validate, open staging file, apply in-memory, run side effects (each emits a `CompensatingAction`), atomic rename on success. On `SIGINT`: rollback `CompensatingActions` in reverse order, remove staging, exit 130. On `SIGTERM`: same handler, 5s timeout. On network failure: per-section retry with exponential backoff (1s/4s/16s).

### 8.2 Partial state and resumability

`.onboard_progress` written after each section commit:

```json
{
  "schema": 1,
  "wizard_version": "0.5.0",
  "started_at": "2026-04-27T13:00:00Z",
  "last_completed": "channels",
  "completed_sections": ["workspace","provider","persona","skills","mcp","channels"],
  "remaining": ["tunnel","tools","hardware","memory","project_context","workspace_files","daemon"]
}
```

On resumable state, prompt: `Found a partial setup from 8 minutes ago. [Y]es resume / [n]o restart / [s]how / [q]uit`. Wizard-version skew warns + defaults to restart. Stale (>30 days) warns + defaults to restart.

### 8.3 Headless behavior

Detection: `!isatty(0) || !isatty(1)`.

| Component | Headless behavior |
|---|---|
| `bootstrap.sh maybe_run_setup()` | Skipped; prints CLI hint. |
| `rantaiclaw onboard` | Refuses interactive; falls back to defaults + flag inputs; prints CLI hint. |
| `rantaiclaw setup <topic>` | Same. |
| Approval gate `mode=manual` | Falls back to `Deny` for unmatched; logged as `manual_headless_deny`. |
| `mode=smart`, `mode=strict`, `mode=off` | Work the same. |
| `/yolo` | No-op + warning. |
| `rantaiclaw doctor` | Works the same; `--format json` for CI. |
| `rantaiclaw migrate from-openclaw` | Refuses without `--yes`; prints dry-run plan. |
| MCP OAuth | Refused with hint to set token via env var. |
| Channel auth probes | Work the same. |

### 8.4 Failure-mode surface

Every error path either retries with backoff, rolls back cleanly, or prints an actionable hint. Never silently swallow. The user is always one re-run away from a working state. Exhaustive table covered in component spec sections.

### 8.5 Logging discipline

Three streams:

| Stream | File | Rotation |
|---|---|---|
| CLI/wizard log | `~/.rantaiclaw/logs/rantaiclaw-<date>.log` | Daily, keep 14 days |
| Audit log | `~/.rantaiclaw/profiles/<name>/audit.log` | Manual via `rantaiclaw audit rotate` |
| Migration log | `~/.rantaiclaw/profiles/<name>/migration.log` | Append-only forever |

Stderr reserved for warnings + errors during interactive setup. `--verbose` / `RANTAICLAW_DEBUG=1` tees CLI log to stderr.

## Testing strategy + parallel-agent dispatch plan

### 9.1 Coverage by layer

| Layer | Files | Asserts |
|---|---|---|
| **Unit** | inside `src/approval/*`, `src/profile/*`, `src/persona/*`, `src/doctor/checks/*`, `src/migrate/*` | Pure logic |
| **Integration** | `tests/onboard_orchestrator.rs`, `tests/profile_lifecycle.rs`, `tests/migrate_legacy.rs`, `tests/migrate_openclaw.rs`, `tests/approval_gate.rs`, `tests/doctor_checks.rs`, `tests/onboard_headless.rs`, `tests/onboard_resumability.rs`, `tests/onboard_rollback.rs`, `tests/audit_resilience.rs` | Cross-module behavior |
| **CLI** | `scripts/lib/test_setup_subcommands.sh`, `scripts/lib/test_bootstrap_auto_setup.sh` | Subcommand exit codes; bootstrap auto-trigger |
| **E2E binary** | `tests/e2e_install_to_chat.sh` | Full path: `bootstrap.sh|bash → wizard scripted → rantaiclaw chat` |
| **Snapshot** | `tests/snapshots/` (insta) | Doctor reports, migration plans, persona renders, audit-log shape |

**Coverage gates** (enforced via `cargo-llvm-cov` in `test-rust-build.yml`):

| Module | Min line coverage |
|---|---|
| `src/approval/` | 95% (security-critical) |
| `src/profile/` | 90% |
| `src/migrate/` | 90% (data-loss risk) |
| `src/onboard/orchestrator.rs` + `src/onboard/section/*` | 85% |
| `src/doctor/` | 80% |
| `src/persona/` | 80% |
| `src/audit/` | 90% |

**Performance gates:**

- Cold `rantaiclaw onboard --interactive` (skip-all): < 200 ms before first prompt.
- `rantaiclaw doctor` (full): < 30 s; `--brief --offline` < 1 s.
- Approval-gate `check()`: < 1 ms median for allowlist-hit; < 5 ms for forbidden-path glob expansion.
- Wizard total (scripted Yes-to-everything, mocked APIs): < 60 s.

### 9.2 Backwards-compatibility tests

- `tests/compat_v041_to_v050.rs` — fixture pre-migration → post-migration.
- `tests/compat_symlink_lifecycle.rs` — v0.5.0/v0.6.0/v0.7.0 controlled by `wizard_version`.
- `tests/compat_old_session_jsonl.rs` — v0.4.1 sessions readable by v0.5.0.

### 9.3 Security-relevant tests (non-negotiable)

`tests/security/` directory:

| Test | Asserts |
|---|---|
| `test_forbidden_path_cannot_be_overridden_by_allowlist` | If `~/.ssh/**` forbidden, no allowlist permits access. |
| `test_strict_mode_denies_yolo` | `/yolo` is a no-op + warning under `mode=strict`. |
| `test_audit_log_records_denials` | Every `Decision::Deny` produces an audit entry. |
| `test_secrets_redacted_in_audit` | Bearer tokens, password=, ~/.aws/credentials → redacted. |
| `test_approval_timeout_defaults_to_deny` | 60s timeout → `Decision::Deny`, never `Allow`. |
| `test_per_agent_strict_overrides_root_off` | gateway_agents override resolved correctly. |
| `test_session_yolo_does_not_persist` | Restart clears `/yolo`. |
| `test_migration_does_not_copy_secrets_by_default` | OpenClaw migration without `--include-secrets` → no API keys. |
| `test_audit_write_failure_does_not_fail_open` | Audit-log unwritable → tool execution proceeds + stderr warning, but Decision unchanged. |

### 9.4 Parallel-agent dispatch graph

11 agents across 5 waves, ~3.5 hours wall-clock peak vs. ~12 hours sequential. No two parallel agents touch the same file, eliminating merge conflicts.

```
Wave 1: Agent 0 (Foundation, solo)
  - src/profile/{mod,migration,commands}.rs
  - src/main.rs Commands::Profile branch
  - src/config/loader.rs profile-aware
  - tests/profile_lifecycle.rs
  - tests/migrate_legacy.rs
  ~45 min agent-time

Wave 2: Agents A, B, C, D, E (parallel)
  A: src/approval/*, src/audit/*, src/agent/loop_.rs (gate insertion only)
  B: src/doctor/*, src/main.rs (doctor branch)
  C: src/persona/*, src/onboard/section/persona.rs
  D: src/skills/bundled/*, src/skills/clawhub.rs ext, src/onboard/section/skills.rs
  E: src/mcp/curated.rs, src/mcp/setup.rs, src/onboard/section/mcp.rs
  ~60 min agent-time each

Wave 3: Agent F (Orchestrator, solo)
  - src/onboard/section/mod.rs (trait)
  - src/onboard/orchestrator.rs
  - src/onboard/wizard.rs (rewrite)
  - extract existing 9 sections into src/onboard/section/*
  ~75 min agent-time

Wave 4: Agents G, H, I (parallel)
  G: scripts/bootstrap.sh (auto-trigger), src/main.rs (Commands::Setup), shell tests
  H: src/onboard/section/daemon.rs, doctor_handoff.rs
  I: src/migrate/*, tests/migrate_openclaw.rs
  ~45 min agent-time each

Wave 5: Agent J (Integration, solo)
  - tests/e2e_install_to_chat.sh
  - docs/install.md (auto-setup section)
  - docs/onboarding.md (NEW)
  - README.md refresh
  - insta snapshots, coverage assertion
  ~30 min agent-time
```

**Conflict-avoidance rules each agent gets:**

1. Edit only the files in your scope; never touch files allocated to a different agent.
2. If you need an out-of-scope file to compile, stub it or feature-flag it.
3. Run `cargo check --all-targets` and unit tests before declaring done.
4. Conventional commit messages; one logical commit per file group; never `git add .`.
5. Branch lives at `feat/onboarding-depth-v2`; rebase onto wave start tag before pushing.

**Coordination commits between waves:** orchestrator (human) tags `wave-N-complete` after each wave; next wave branches from the tag.

### 9.5 Acceptance criteria for the merged PR

The PR `feat/onboarding-depth-v2 → main` is mergeable iff:

- All ten clarifying-question decisions visibly implemented (verified by §9.6 traceability matrix).
- All tests in §9.1 pass on CI.
- Coverage gates met.
- All security tests in §9.3 pass.
- Compatibility tests in §9.2 pass.
- Performance gates met on `test-benchmarks.yml`.
- E2E `tests/e2e_install_to_chat.sh` succeeds against a v0.5.0-rc artifact built from branch tip.
- README + `docs/install.md` + `docs/onboarding.md` updated.
- `RELEASE_v0.5.0.md` drafted.

### 9.6 Traceability matrix

| Decision | Implementation | Test |
|---|---|---|
| Auto-trigger from bootstrap.sh | `scripts/bootstrap.sh::maybe_run_setup` | `scripts/lib/test_bootstrap_auto_setup.sh` |
| Modular `setup <topic>` subcommands | `src/main.rs Commands::Setup`, `src/onboard/section/*` | `scripts/lib/test_setup_subcommands.sh` |
| Curated 5-skill starter pack + ClawHub multi-select | `src/skills/bundled/*`, `src/onboard/section/skills.rs` | `tests/onboard_skills_section.rs` |
| Curated 6-server MCP multi-select + inline auth | `src/mcp/curated.rs`, `src/mcp/setup.rs`, `src/onboard/section/mcp.rs` | `tests/onboard_mcp_section.rs` |
| Persona preset picker + interview | `src/persona/*`, `src/onboard/section/persona.rs` | `tests/persona_rendering.rs`, snapshots |
| CLI profiles | `src/profile/*`, `src/main.rs Commands::Profile` | `tests/profile_lifecycle.rs` |
| Voice — skipped | (no code) | (no test) |
| Approval accretion + L1-L4 + per-agent overrides + audit log | `src/approval/*`, `src/audit/*`, `src/agent/loop_.rs`, `src/config/schema.rs` ext | `tests/approval_gate.rs`, `tests/security/*` |
| Doctor full depth | `src/doctor/*` | `tests/doctor_checks.rs`, snapshots |
| Conditional daemon offer | `src/onboard/section/daemon.rs` | `tests/onboard_daemon_section.rs` |
| OpenClaw migration | `src/migrate/*` | `tests/migrate_openclaw.rs` |
| Storage layout migration (architectural) | `src/profile/migration.rs` | `tests/migrate_legacy.rs`, `tests/compat_v041_to_v050.rs` |

## Out of scope (explicitly deferred)

- Voice / TTS / wake words — full feature epic, not in v1.
- `rantaiclaw config export` / `import` for cross-machine config promotion — useful but skipped per Q10 decision.
- Locale README variants (zh-CN, ja, ru) — none currently exist.
- Cosign verification *inside* the installer (manual recipe in docs is sufficient for now).
- Homebrew formula publication — placeholder mention only.

## Risk register

| Risk | Mitigation |
|---|---|
| Storage migration corrupts user data | Lock + atomic `rename` + idempotent retry; legacy symlinks for ≥1 release; `tests/migrate_legacy.rs` exhaustive. |
| Approval gate has a bypass | 95% line coverage gate; `tests/security/*` non-negotiable; gate is the only path between LLM tool emission and execution. |
| Auto-trigger from bootstrap.sh annoys users | `--skip-setup` opt-out; `RANTAICLAW_SKIP_SETUP=1`; `[skip / reconfigure / show]` per section. |
| Wizard becomes too long | Each section can be skipped; resume mid-wizard supported; modular `setup <topic>` lets users come back later. |
| Parallel-agent merge conflicts | Dispatch graph designed so no two parallel agents touch the same file; rebase discipline; wave coordination commits. |
| OpenClaw migration loses secrets unintentionally | `--include-secrets` opt-in; dry-run default; never auto-deletes source. |
| Persona file diverges from name/timezone after `setup project_context` | Project-context section explicitly re-renders `SYSTEM.md` from the saved `persona.toml` template. |

## References

- [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent) — reference baseline for setup wizard depth and modular `hermes setup <topic>` UX.
- [openclaw/openclaw](https://github.com/openclaw/openclaw) — reference baseline for `openclaw onboard --install-daemon` and pairing-based DM policy.
- [docs/install.md](../../install.md) — current install flow (will be extended with auto-setup behavior).
- [docs/superpowers/specs/2026-04-25-installer-ux-upgrade-design.md](2026-04-25-installer-ux-upgrade-design.md) — preceding installer UX work that v0.5.0 builds on top of.
