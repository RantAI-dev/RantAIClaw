# Plan 260: Correct config/lifecycle docs and improve config load-error DX

> **Executor instructions**: Follow step by step; verify each claim against the cited CODE before editing the doc; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- docs/ Cargo.toml scripts/bootstrap.sh src/config/schema.rs src/main.rs .env.example`
> If a cited code fact changed, re-verify against the live code before writing the doc.

## Status

- **Priority**: P2 (docs are a product surface here; stale-and-wrong beats missing)
- **Effort**: M
- **Risk**: LOW (docs + one config-load warning path)
- **Depends on**: none (but if plan 253 changes defaults, align J-numbers to the NEW defaults)
- **Category**: docs / dx
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

The docs are wrong in ways that actively misdirect operators, and a mistyped config key is silently ignored. Each fix is a claim verified against code.

## In-scope findings and current state (verify each against code before editing)

- **J1** `config.toml` path: default resolves to the PROFILE dir, not `~/.rantaiclaw/config.toml` (`src/config/schema.rs:3699-3720`). Fix `docs/reference/config.md:7-17`, `docs/operations/runbook.md:114,122`, `docs/start/troubleshooting.md:181`. The token-revocation procedure (`config.md:329-333`) points at the wrong file.
- **J2** Feature gating inverted: `Cargo.toml:264` has `kb` in default; `hardware` (`:276`) is not. Fix `commands.md:30,352` (drop "gated"/"off in default build" for kb) and `commands.md:28,336-340` (add "not in default build" for hardware). `config.md:763` and `README.md:314` are already correct.
- **J3** `docs/start/one-click-bootstrap.md:7-11` leads with unpublished `brew install` (`.github/release-notes-install.md:30` says planned); `:90,100` invert the default (`scripts/bootstrap.sh:1328-1341` runs `setup --force` by default; `--skip-setup`/`RANTAICLAW_SKIP_SETUP` at `bootstrap.sh:54,99` undocumented).
- **J4** `docs/start/troubleshooting.md:48-52` recommends deprecated no-op `--prefer-prebuilt` (`bootstrap.sh:69`); `:69-73` recommends `--features hardware` (makes the OOM build heavier). Replace with plain `./bootstrap.sh` and a genuine reduction (`--no-default-features --features tui`, verify it compiles).
- **J5** `docs/reference/commands.md:9-31` tables 22 of 33 CLI commands; absent incl. `update`/`rollback`/`uninstall`/`auth`/`profile`. `doctor` `--format json`/`--brief`/`--offline` and `doctor models` subcommand undocumented; `migrate --from zeroclaw` + 3 flags undocumented (`main.rs`).
- **J6** setup section catalog stale in `commands.md:62` AND `src/main.rs:257-258` (`setup --help`) — missing `approvals`+`login`. Canonical order: `src/onboard/wizard.rs:132-144`.
- **J7** `docs/reference/config.md:622` (600) vs `:645` (300) contradict; real default `schema.rs:2848` = 600.
- **J8** `src/gateway/config_api.rs:617-622` doc comment claims Telegram tokens are plaintext; they're encrypted (`schema.rs:4547`). Also name the channels genuinely NOT wrapped (Discord/Slack/Mattermost).
- **K2** No `deny_unknown_fields` anywhere → a mistyped key silently no-ops; parse errors go through `toml::Value` losing spans and name neither file nor key (`schema.rs:3998,4046-4048`). Add a warn-on-unknown-key pass + `config_path` in the parse-error context.
- **K3** `.env.example` omits every `KB_*` var, `RANTAICLAW_CONFIG_DIR`, skills/UI/skip-setup vars.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Markdown lint | `bash scripts/ci/docs_quality_gate.sh` (if present) | pass |
| Bootstrap syntax | `bash -n bootstrap.sh scripts/bootstrap.sh scripts/install.sh` | exit 0 |
| Build (K2) | `cargo build --lib` | exit 0 |
| Test (K2) | `cargo test --lib config::schema` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: the doc files above, `.env.example`, and `src/config/schema.rs` (ONLY the K2 unknown-key warning + parse-error context).
**Out of scope**: the docs-vs-code CI GATE (plan 261); default VALUE changes (plan 253 — but if 253 lands first, use its new defaults in J-fixes).

## Git workflow

- Branch: `docs/config-lifecycle-and-dx`
- Message e.g. `docs(config): correct paths, gating, and command reference; warn on unknown config keys`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Fix the doc claims (J1-J8)

Edit each doc per the verified code fact above. For J5, regenerate the top-level table from `rantaiclaw --help` output and add a `### doctor` section + correct the migrate row. For J6, update both `commands.md:62` and the clap `long_about` at `main.rs:257` (prefer generating the list from `canonical_sections()`). For J4, VERIFY `--no-default-features --features tui` compiles before publishing it.

**Verify**: `bash -n bootstrap.sh scripts/bootstrap.sh scripts/install.sh` exit 0; docs lint passes; `grep` each old wrong string returns nothing.

### Step 2 (K2): warn on unknown config keys; add file+key to parse errors

Walk the already-parsed `toml::Value` (`schema.rs:4000`) against `schema_for!(Config)` and `tracing::warn!` each unrecognized path with a nearest-match suggestion (no serde attribute changes — that would hard-fail configs carrying removed keys). Add `config_path.display()` to the `.context()` at the deserialize failure (`:4047`); optionally attempt a second spanned `toml::from_str::<Config>` purely to harvest a better error message.

**Verify**: Test-plan `unknown_key_warns` passes; `mistyped_key_error_names_the_file` passes.

### Step 3 (K3): complete `.env.example`

Append commented `# --- Knowledge Base ---` (the `KB_*` vars from `config.md:804-877`) and `# --- Paths & profiles ---` (`RANTAICLAW_CONFIG_DIR`, `RANTAICLAW_SKILLS_PROMPT_MODE`, `RANTAICLAW_UI_ALLOWED_HOSTS`, `RANTAICLAW_SKIP_SETUP`) with documented defaults inline.

**Verify**: `grep -c "KB_" .env.example` > 0.

## Test plan

- `config::schema`: `unknown_key_warns` — a config with a mistyped key loads and emits a warning naming the key (capture tracing or assert the warn path runs). `mistyped_key_error_names_the_file` — a type error's message contains the config path.
- Docs: no automated behavior test; the verification is grep + lint + `bash -n`.
- Verification: `cargo test --lib config::schema` + docs lint + `bash -n ...` → all pass.

## Done criteria

- [ ] docs lint passes; `bash -n bootstrap.sh scripts/bootstrap.sh scripts/install.sh` exit 0
- [ ] `cargo build --lib` + `cargo test --lib config::schema` pass (K2)
- [ ] every old wrong string (bare `~/.rantaiclaw/config.toml`, kb "gated", `--prefer-prebuilt` remedy, 300s note) is gone (`grep`)
- [ ] `.env.example` has `KB_*` + `RANTAICLAW_CONFIG_DIR`
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- A doc claim you're "correcting" is actually right (the code drifted the other way) — re-verify against code; fix whichever is wrong, report the ambiguity.
- The unknown-key walk produces false-positive warnings for legitimate keys removed by past migrations — the warn must tolerate known-removed keys (list them) or it becomes noise; report.

## Maintenance notes

- Reviewer: each doc fix must trace to a code fact (spot-check J1, J2, J7).
- Plan 261 adds the CI gate that keeps these from regressing; this plan clears the backlog it will then guard.
- If plan 253 changed defaults, J2/J7 fixes must reflect the NEW defaults.
