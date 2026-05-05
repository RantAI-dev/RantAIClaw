# All Setup Provisioners — Roadmap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port every legacy `dialoguer`-based `SetupSection` and every other configurable subsystem in `Config` to a `TuiProvisioner` impl, so `/setup` covers 100% of the user-facing setup surface in-TUI with no context switch. Group provisioners under category sub-pickers (channels, integrations, hardware, runtime) so `/setup` stays navigable as the list grows. Validate where possible (auth pings, file checks). Phase out the legacy `dialoguer` setup wizard at the end.

**Architecture:** Each port is a single `TuiProvisioner` impl that mirrors its legacy `SetupSection` flow (reading prompt order from the existing `src/onboard/section/<name>.rs`) but emits `Prompt`/`Choose`/`Message`/`Done`/`Failed` events. Concrete validation (Telegram `getMe`, Slack `auth.test`, etc.) is moved into shared helpers in `src/onboard/provision/validate/` so each provisioner only references the helper. The picker grows automatically: `available()` lists all registered provisioners; the picker groups them by `category()` (a new method added to the trait). Legacy `SetupSection` impls stay until every section has a TuiProvisioner peer; final task removes them and the dialoguer wizard.

**Tech Stack:** Rust, `ratatui` (existing `SetupOverlay`), `tokio::mpsc`, `reqwest` (validation pings), `serde`/`toml` (config writes). No new external dependencies.

**Out of scope:**
- New CLI subcommands (existing `rantaiclaw setup <name> --non-interactive` works for every registered provisioner).
- Re-design of any `Config` struct (writes only — schema is treated as the contract).
- Web UI for setup (this plan is TUI-only).

---

## File Structure

**New files (one per provisioner):**
- `src/onboard/provision/provider.rs` — `ProviderProvisioner`
- `src/onboard/provision/approvals.rs` — `ApprovalsProvisioner`
- `src/onboard/provision/skills.rs` — `SkillsProvisioner`
- `src/onboard/provision/mcp.rs` — `McpProvisioner`
- `src/onboard/provision/channels/telegram.rs` — `TelegramProvisioner`
- `src/onboard/provision/channels/discord.rs` — `DiscordProvisioner`
- `src/onboard/provision/channels/slack.rs` — `SlackProvisioner`
- `src/onboard/provision/channels/whatsapp_cloud.rs` — `WhatsAppCloudProvisioner` (Meta Cloud API; distinct from existing `whatsapp-web`)
- `src/onboard/provision/channels/signal.rs` — `SignalProvisioner`
- `src/onboard/provision/channels/matrix.rs` — `MatrixProvisioner`
- `src/onboard/provision/channels/mattermost.rs` — `MattermostProvisioner`
- `src/onboard/provision/channels/imessage.rs` — `IMessageProvisioner`
- `src/onboard/provision/channels/lark.rs` — `LarkProvisioner`
- `src/onboard/provision/channels/dingtalk.rs` — `DingTalkProvisioner`
- `src/onboard/provision/channels/nextcloud_talk.rs` — `NextcloudTalkProvisioner`
- `src/onboard/provision/channels/qq.rs` — `QqProvisioner`
- `src/onboard/provision/channels/email.rs` — `EmailProvisioner`
- `src/onboard/provision/channels/irc.rs` — `IrcProvisioner`
- `src/onboard/provision/channels/linq.rs` — `LinqProvisioner`
- `src/onboard/provision/memory.rs` — `MemoryProvisioner`
- `src/onboard/provision/runtime.rs` — `RuntimeProvisioner`
- `src/onboard/provision/proxy.rs` — `ProxyProvisioner`
- `src/onboard/provision/tunnel.rs` — `TunnelProvisioner`
- `src/onboard/provision/gateway.rs` — `GatewayProvisioner`
- `src/onboard/provision/browser.rs` — `BrowserProvisioner`
- `src/onboard/provision/web_search.rs` — `WebSearchProvisioner`
- `src/onboard/provision/multimodal.rs` — `MultimodalProvisioner`
- `src/onboard/provision/peripherals.rs` — `PeripheralsProvisioner`
- `src/onboard/provision/hardware.rs` — `HardwareProvisioner`
- `src/onboard/provision/composio.rs` — `ComposioProvisioner`
- `src/onboard/provision/secrets.rs` — `SecretsProvisioner`
- `src/onboard/provision/agents.rs` — `AgentsProvisioner` (sub-agent delegation)
- `src/onboard/provision/model_routes.rs` — `ModelRoutesProvisioner`
- `src/onboard/provision/embedding_routes.rs` — `EmbeddingRoutesProvisioner`

**New shared helpers:**
- `src/onboard/provision/validate/mod.rs` — module root
- `src/onboard/provision/validate/http.rs` — `probe_get`, `probe_post` (mockable HTTP probes for auth-test endpoints)
- `src/onboard/provision/validate/file.rs` — `assert_path_writable`, `assert_path_exists`
- `src/onboard/provision/validate/process.rs` — `validate_command_on_path` (replaces ad-hoc which/where checks)

**New tests (one per provisioner):**
- `tests/provision_provider.rs`, `tests/provision_approvals.rs`, `tests/provision_skills.rs`, `tests/provision_mcp.rs`, `tests/provision_<channel>.rs` (×15), `tests/provision_<surface>.rs` (×14)

**Modified files:**
- `src/onboard/provision/traits.rs` — add `category()` method to `TuiProvisioner` trait, `ProvisionerCategory` enum.
- `src/onboard/provision/mod.rs` — register every new module.
- `src/onboard/provision/registry.rs` — every `provisioner_for` arm and `available()` entry.
- `src/tui/commands/setup.rs` — `/setup` picker groups by category.
- `src/tui/app.rs` — sub-pickers per category use the registry instead of hard-coded `CHANNEL_TYPES` const.
- `src/main.rs` — eventually delete `Commands::Setup` legacy branch (Phase G); keep `--non-interactive` working via existing headless driver.

**Files to delete (Phase G):**
- `src/onboard/wizard.rs` (6492 lines — the `dialoguer` wizard)
- `src/onboard/section/*.rs` (six files; replaced by provisioners)
- `src/onboard/section/mod.rs`

---

## Template — Standard provisioner task structure

Every provisioner port follows the same shape. Tasks B1–E14 reference this template instead of repeating the boilerplate. **Each task block enumerates the section-specific schedule (prompt order, config writes, validation) so the engineer has every fact needed; the surrounding TDD scaffold is identical.**

For each provisioner, the engineer:

- [ ] **Step 1** — Read the legacy file (`src/onboard/section/<name>.rs` for core sections, the dialoguer block in `src/onboard/wizard.rs::setup_channels` for channels, or design from `Config` schema for new surfaces). Note prompt order, defaults, validation logic, and the exact config field paths it writes.

- [ ] **Step 2** — Write the failing event-flow test in `tests/provision_<name>.rs`:

```rust
use rantaiclaw::onboard::provision::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, TuiProvisioner,
};
use rantaiclaw::onboard::provision::<module>::<Name>Provisioner;
use tokio::sync::mpsc;

#[tokio::test]
async fn provisioner_writes_config_after_prompts() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = rantaiclaw::config::Config::default();
    let profile = rantaiclaw::profile::Profile {
        name: "test".into(),
        root: tmp.path().to_path_buf(),
    };
    let (etx, mut erx) = mpsc::channel(32);
    let (rtx, rrx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        <Name>Provisioner.run(&mut config, &profile, ProvisionIo { events: etx, responses: rrx }).await
    });
    while let Some(ev) = erx.recv().await {
        match ev {
            ProvisionEvent::Prompt { id, default, .. } => {
                let v = match id.as_str() {
                    // ── enumerate the section-specific reply script here ──
                    _ => default.unwrap_or_default(),
                };
                rtx.send(ProvisionResponse::Text(v)).await.unwrap();
            }
            ProvisionEvent::Choose { multi: false, .. } => {
                rtx.send(ProvisionResponse::Selection(vec![0])).await.unwrap();
            }
            ProvisionEvent::Choose { multi: true, .. } => {
                rtx.send(ProvisionResponse::Selection(vec![])).await.unwrap();
            }
            ProvisionEvent::Done { .. } | ProvisionEvent::Failed { .. } => break,
            _ => {}
        }
    }
    task.await.unwrap().unwrap();
    // ── enumerate the assertions on `config` after run ──
}
```

- [ ] **Step 3** — Run test: `cargo test -p rantaiclaw --test provision_<name>` → FAIL.
- [ ] **Step 4** — Implement the provisioner in `src/onboard/provision/<name>.rs`. Mirror the legacy prompt schedule. Use `validate::*` helpers for any auth-test pings. End with `Done { summary }`.
- [ ] **Step 5** — Register in `src/onboard/provision/mod.rs` (`pub mod <name>;`) and `src/onboard/provision/registry.rs` (factory arm + `available()` entry with `category`).
- [ ] **Step 6** — Run test → PASS.
- [ ] **Step 7** — Commit: `git commit -m "feat(onboard): <Name>Provisioner ports legacy <name> section"`.

---

# Phase A — Framework polish

## Task A1: Add `ProvisionerCategory` to the `TuiProvisioner` trait

Categories let the picker group provisioners. Without this, the picker becomes a flat list of 35+ entries.

**Files:**
- Modify: `src/onboard/provision/traits.rs`
- Modify: `src/onboard/provision/whatsapp_web.rs`, `src/onboard/provision/persona.rs` (existing impls)
- Test: inline in `traits.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod cat_tests {
    use super::*;
    #[test]
    fn category_variants_distinct() {
        assert_ne!(ProvisionerCategory::Core, ProvisionerCategory::Channel);
        assert_ne!(ProvisionerCategory::Channel, ProvisionerCategory::Integration);
        assert_ne!(ProvisionerCategory::Integration, ProvisionerCategory::Runtime);
        assert_ne!(ProvisionerCategory::Runtime, ProvisionerCategory::Hardware);
    }
}
```

- [ ] **Step 2:** `cargo test -p rantaiclaw onboard::provision::traits::cat_tests` → FAIL.

- [ ] **Step 3: Add the enum + trait method**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionerCategory {
    /// Core agent setup (provider, persona, skills, mcp, approvals).
    Core,
    /// Communication channels (telegram, discord, whatsapp, …).
    Channel,
    /// Integrations (composio, secrets, browser automation, web search).
    Integration,
    /// Runtime/infrastructure (memory, runtime, proxy, tunnel, gateway, multimodal).
    Runtime,
    /// Hardware/peripherals (peripherals, hardware boards).
    Hardware,
    /// Routing (model_routes, embedding_routes, agents).
    Routing,
}

#[async_trait]
pub trait TuiProvisioner: Send {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// Category for picker grouping. Default: Core.
    fn category(&self) -> ProvisionerCategory { ProvisionerCategory::Core }
    async fn run(
        &self,
        config: &mut crate::config::Config,
        profile: &crate::profile::Profile,
        io: ProvisionIo,
    ) -> Result<()>;
}
```

Update existing `WhatsAppWebProvisioner::category()` → `ProvisionerCategory::Channel`. Update `PersonaProvisioner::category()` → `ProvisionerCategory::Core` (or omit; default fits).

- [ ] **Step 4:** Run tests → PASS, build clean.
- [ ] **Step 5:** Commit `feat(onboard): provisioner categories for picker grouping`.

## Task A2: Validation helpers

**Files:**
- Create: `src/onboard/provision/validate/mod.rs`, `validate/http.rs`, `validate/file.rs`, `validate/process.rs`
- Test: inline `#[cfg(test)]` blocks per file

- [ ] **Step 1: Tests**

```rust
// validate/http.rs tests
#[tokio::test]
async fn probe_get_returns_status() {
    let mock = mockito::Server::new_async().await;
    let m = mock.mock("GET", "/test").with_status(200).create_async().await;
    let url = format!("{}/test", mock.url());
    let r = probe_get(&url, &[]).await.unwrap();
    assert_eq!(r.status, 200);
    m.assert_async().await;
}

// validate/file.rs tests
#[test]
fn assert_path_writable_succeeds_for_tempdir() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(assert_path_writable(tmp.path()).is_ok());
}
#[test]
fn assert_path_writable_fails_for_root() {
    assert!(assert_path_writable(std::path::Path::new("/__nonexistent_root__")).is_err());
}

// validate/process.rs tests
#[test]
fn validate_command_on_path_finds_sh() {
    assert!(validate_command_on_path("sh").is_ok());
}
#[test]
fn validate_command_on_path_rejects_missing() {
    assert!(validate_command_on_path("__definitely_not_a_command__").is_err());
}
```

- [ ] **Step 2:** `cargo test -p rantaiclaw onboard::provision::validate` → FAIL.

- [ ] **Step 3: Implementation**

```rust
// validate/http.rs
use anyhow::{Context, Result};
pub struct ProbeResult { pub status: u16, pub body: String }

pub async fn probe_get(url: &str, headers: &[(&str, &str)]) -> Result<ProbeResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let mut rb = client.get(url);
    for (k, v) in headers { rb = rb.header(*k, *v); }
    let resp = rb.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(ProbeResult { status, body })
}

pub async fn probe_post(url: &str, headers: &[(&str, &str)], body: &str) -> Result<ProbeResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let mut rb = client.post(url).body(body.to_string());
    for (k, v) in headers { rb = rb.header(*k, *v); }
    let resp = rb.send().await.with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(ProbeResult { status, body })
}

// validate/file.rs
use std::path::Path;
pub fn assert_path_writable(p: &Path) -> anyhow::Result<()> {
    if p.exists() {
        let test_file = p.join(".rantaiclaw_writetest");
        std::fs::write(&test_file, b"")?;
        std::fs::remove_file(&test_file)?;
        Ok(())
    } else if let Some(parent) = p.parent() {
        let test_file = parent.join(".rantaiclaw_writetest");
        std::fs::write(&test_file, b"")?;
        std::fs::remove_file(&test_file)?;
        Ok(())
    } else {
        anyhow::bail!("path has no parent: {}", p.display())
    }
}
pub fn assert_path_exists(p: &Path) -> anyhow::Result<()> {
    if p.exists() { Ok(()) } else { anyhow::bail!("path does not exist: {}", p.display()) }
}

// validate/process.rs
pub fn validate_command_on_path(cmd: &str) -> anyhow::Result<std::path::PathBuf> {
    which::which(cmd).map_err(|e| anyhow::anyhow!("{cmd} not found on PATH: {e}"))
}
```

`mod.rs`: `pub mod http; pub mod file; pub mod process;`. Also `pub mod` add to `src/onboard/provision/mod.rs`.

Add `which = "6"` and `mockito = "1"` to dev-dependencies in `Cargo.toml` if not already present.

- [ ] **Step 4:** Tests PASS.
- [ ] **Step 5:** Commit `feat(onboard): validation helpers (http probes, file checks, command lookup)`.

## Task A3: Picker auto-grows from registry

The picker today hard-codes Persona + Channels. Make it iterate `available()` and group by `category()`.

**Files:**
- Modify: `src/tui/commands/setup.rs`
- Modify: `src/tui/app.rs` — replace hard-coded `CHANNEL_TYPES` with registry filter on `category() == Channel`.

- [ ] **Step 1: Test (extends `tests/tui_setup_overlay.rs`):**

```rust
#[test]
fn slash_setup_picker_includes_every_registered_core_provisioner() {
    use rantaiclaw::onboard::provision::{available, ProvisionerCategory, provisioner_for};
    let registry = CommandRegistry::new();
    let (mut ctx, _r, _t) = TuiContext::test_context();
    let r = registry.dispatch("/setup", &mut ctx).unwrap();
    let picker = match r {
        CommandResult::OpenListPicker(p) => p,
        _ => panic!(),
    };
    let keys: std::collections::HashSet<_> = picker.items.iter().map(|i| i.key.clone()).collect();
    for (name, _desc) in available() {
        // We only assert Core provisioners appear at the top level;
        // Channel provisioners belong in the sub-picker.
        if let Some(p) = provisioner_for(name) {
            if matches!(p.category(), ProvisionerCategory::Core
                | ProvisionerCategory::Integration
                | ProvisionerCategory::Runtime
                | ProvisionerCategory::Hardware
                | ProvisionerCategory::Routing)
            {
                assert!(keys.contains(name), "expected `{name}` in top-level picker");
            }
        }
    }
    // The "channels" aggregate entry must also appear.
    assert!(keys.contains("channels"));
}
```

- [ ] **Step 2:** Test FAIL.

- [ ] **Step 3: Implementation**

```rust
// src/tui/commands/setup.rs — execute() body
use rantaiclaw::onboard::provision::{available, provisioner_for, ProvisionerCategory};

let mut items = Vec::new();
let mut seen_channel = false;

for (name, desc) in available() {
    let cat = provisioner_for(name).map(|p| p.category()).unwrap_or(ProvisionerCategory::Core);
    match cat {
        ProvisionerCategory::Channel => { seen_channel = true; /* listed in sub-picker */ }
        _ => {
            items.push(ListPickerItem {
                key: name.to_string(),
                primary: cat_label(cat).into(),
                secondary: format!("{} — {}", name, desc),
            });
        }
    }
}
// Sort by category label, then by name.
items.sort_by(|a, b| a.primary.cmp(&b.primary).then(a.secondary.cmp(&b.secondary)));

if seen_channel {
    items.insert(0, ListPickerItem {
        key: "channels".into(),
        primary: "Channels".into(),
        secondary: "Telegram, Discord, Slack, WhatsApp, …".into(),
    });
}

fn cat_label(c: ProvisionerCategory) -> &'static str {
    match c {
        ProvisionerCategory::Core => "Core",
        ProvisionerCategory::Channel => "Channels",
        ProvisionerCategory::Integration => "Integrations",
        ProvisionerCategory::Runtime => "Runtime",
        ProvisionerCategory::Hardware => "Hardware",
        ProvisionerCategory::Routing => "Routing",
    }
}
```

In `src/tui/app.rs::open_channel_sub_picker`, replace the `CHANNEL_TYPES` const with a filter:

```rust
fn open_channel_sub_picker(&mut self) {
    let items: Vec<ListPickerItem> = crate::onboard::provision::available()
        .into_iter()
        .filter_map(|(name, desc)| {
            let p = crate::onboard::provision::provisioner_for(name)?;
            (p.category() == crate::onboard::provision::ProvisionerCategory::Channel).then_some(
                ListPickerItem {
                    key: name.to_string(),
                    primary: name.to_string(),
                    secondary: desc.to_string(),
                },
            )
        })
        .collect();
    let picker = ListPicker::new(
        ListPickerKind::SetupChannel,
        "Select channel type",
        items,
        None,
        "no channel provisioners available",
    );
    self.list_picker = Some(picker);
}
```

Delete the `CHANNEL_TYPES` const (not used after this).

- [ ] **Step 4:** Test PASS, manual smoke (`/setup` shows Persona + Channels for now since only those are registered; will grow as Phase B-E land).
- [ ] **Step 5:** Commit `feat(tui): /setup picker auto-grows from provisioner registry`.

---

# Phase B — Core sections (5 tasks)

## Task B1: `ProviderProvisioner` (LLM provider + API key + model)

**Legacy reference:** `src/onboard/section/provider.rs:1-107`.

**Prompt schedule:**
1. `Choose` — provider list. Single-select. Options pulled from `crate::providers::available_providers()` (use the same enum the legacy section reads).
2. `Prompt` (secret) — API key. Default: `None`. Validation: pre-flight ping via `validate::http::probe_get` to the provider's `/v1/models` endpoint with the key as `Bearer`. On 200 OK → continue; on 401 → re-prompt with "invalid key" message; on network error → warn + continue (defer to runtime).
3. `Choose` — model list. Single-select. Options fetched live from the provider's `/v1/models` endpoint after the key is validated. If list fetch fails, fall back to a hardcoded "Top 5" per provider (read from `crate::providers::default_models()`).

**Config writes:**
- `config.providers.<provider>.api_key = Some(encrypt(key))` — use `crate::secrets::encrypt_for_profile(profile, key)` per existing pattern.
- `config.providers.<provider>.default_model = Some(model)`.
- `config.providers.<provider>.enabled = true`.

**Validation:** Before writing config, re-probe `/v1/models` with the chosen key+model to confirm. On failure, emit `Failed { error }` and bail without writing.

**Done summary:** `"Provider <name> configured: model=<model>"`.

**Standard provisioner test/impl/commit cycle (see Template).**

## Task B2: `ApprovalsProvisioner` (L1–L4 autonomy tier)

**Legacy reference:** `src/onboard/section/approvals.rs:1-182`.

**Prompt schedule:**
1. `Choose` (single-select) — tier. Options: `["L1 — Read only", "L2 — Read + safe writes (default)", "L3 — Full local exec", "L4 — Autonomous (use with care)"]`. Default: cursor on `L2`.

**Config writes:**
- Write three preset TOML files under `<profile>/policy/`: `autonomy.toml`, `command_allowlist.toml`, `forbidden_paths.toml`.
- Source: `crate::approval::policy_writer::write_preset(profile, tier)` — re-use existing helper.

**Validation:** After write, instantiate `crate::approval::ApprovalGate::from_profile(profile)` to verify the files parse cleanly. On parse failure, emit `Failed`.

**Done summary:** `"Approvals tier set: <tier>"`.

**Standard cycle.**

## Task B3: `SkillsProvisioner` (bundled + ClawHub install)

**Legacy reference:** `src/onboard/section/skills.rs:1-218`.

**Prompt schedule:**
1. `Choose` (multi-select) — bundled starter pack. Options: `["web-search", "summarizer", "research-assistant", "scheduler-reminders", "meeting-notes"]`. Default: all selected.
2. `Choose` (multi-select) — ClawHub picks. Options fetched from `crate::skills::clawhub::list_top(20)`. On network failure: skip this prompt with a `Message { Warn }`.
3. `Prompt` (text, optional) — additional ClawHub slug. Empty = skip.

**Config writes:**
- Calls `crate::skills::bundled::install_starter_pack_filtered(profile, &selected_bundled)`.
- For each ClawHub pick: `crate::skills::clawhub::install_one(profile, slug).await`.
- No `Config` mutation — skills live in `<profile>/skills/`.

**Validation:** After install, list `<profile>/skills/` and emit a `Message { Success, "Installed N skills: <names>" }`.

**Note:** This task depends on the audit §7 ClawHub `install_one` rewrite (separate plan: `2026-04-29-setup-audit.md` §7). If that rewrite hasn't shipped, ClawHub picks will write stub `SKILL.md` placeholders — port the provisioner anyway and call out the dependency.

**Done summary:** `"Skills installed: <count> bundled, <count> ClawHub"`.

**Standard cycle.**

## Task B4: `McpProvisioner` (curated + custom)

**Legacy reference:** `src/onboard/section/mcp.rs:1-129`.

**Prompt schedule:**
1. `Choose` (multi-select) — curated MCP servers. Options from `crate::mcp::curated::list()`. Each item shows name + auth requirement.
2. For each selected server with `auth_required = true`:
   - `Prompt` (secret) — auth token / API key for that server.
3. `Choose` (single-select) — install zero-auth bundle? Options: `["Yes — install all zero-auth servers", "No"]`. Default: Yes.
4. `Prompt` (text, optional) — custom MCP server: command (e.g. `npx my-mcp-server`). Empty = skip.
5. If custom command provided: `Prompt` for human-readable name.

**Config writes:**
- For each curated server: `config.mcp_servers.insert(slug, McpServerConfig { ... })` per `crate::mcp::setup::register_mcp`.
- For custom: `config.mcp_servers.insert(name, McpServerConfig { command, ... })`.

**Validation:**
- Each registered server: `crate::mcp::setup::validate_mcp_startup(&server, &auth)` (5s spawn + initialize). On failure: `Message { Warn, "<name> failed validation; registered anyway" }`.
- Run validation for zero-auth servers too (closes audit §2).

**Done summary:** `"MCP servers registered: <count>; failed validation: <count>"`.

**Standard cycle.**

## Task B5: Polish `PersonaProvisioner`

The current `PersonaProvisioner` (Phase 0) writes a stub. Mirror the full `src/onboard/section/persona.rs:1-106` flow: agent name, template, system-prompt overrides, output file.

**Prompt schedule:**
1. `Prompt` — agent name. Default: `"RantaiClawAgent"`.
2. `Choose` (single-select) — template. Options from `crate::persona::available_templates()`: `["default", "concise", "verbose", "research-assistant", "executive-assistant", "friendly-companion"]`.
3. `Prompt` (text, multi-line allowed via Ctrl+J in overlay) — additional system-prompt notes. Default: `None`. Empty = no overrides.
4. `Prompt` — output file path. Default: `<profile>/persona.md`.

**Config writes:**
- File: render template with `crate::persona::renderer::render(template, name, notes)` and write to chosen path.
- Config: `config.identity.persona_path = Some(path)`.

**Validation:** After write, re-load with `crate::persona::Persona::load(&path)` and verify it parses.

**Done summary:** `"Persona saved: <name> (<template>) → <path>"`.

**Standard cycle.** Replace existing `PersonaProvisioner::run` body — keep tests in `tests/provision_persona.rs` updated.

---

# Phase C — Channels (15 tasks)

Each channel provisioner ports a single block of `src/onboard/wizard.rs::setup_channels`. The dialoguer wizard had a giant match on channel type; we split each branch into its own provisioner.

## Task C1: `TelegramProvisioner`

**Prompt schedule:**
1. `Prompt` (secret) — bot token (BotFather output, format `<digits>:<base64-ish>`).
2. `Prompt` (text, optional) — allowed chat IDs (comma-separated integers; empty = deny-all).
3. `Choose` — bot mode. Options: `["Direct messages only", "Group + DMs"]`.

**Validation:**
- `validate::http::probe_get("https://api.telegram.org/bot<token>/getMe", &[])` — 200 OK with `ok: true` → success; 401 → "invalid token, re-enter"; network → warn.

**Config writes:** `config.channels_config.telegram = Some(TelegramConfig { bot_token, allowed_chat_ids, group_mode, ... })`.

**Standard cycle.**

## Task C2: `DiscordProvisioner`

**Prompt schedule:**
1. `Prompt` (secret) — bot token.
2. `Prompt` (text, optional) — guild ID (single guild restriction).
3. `Prompt` (text, optional) — allowed user IDs (comma-separated).
4. `Choose` — modes: `["Respond to @-mention only", "Respond to all messages", "Respond to all (incl. other bots)"]`.

**Validation:** `probe_get("https://discord.com/api/v10/users/@me", &[("Authorization", "Bot <token>")])` → 200 with `username` → success.

**Config writes:** `config.channels_config.discord = Some(DiscordConfig { bot_token, guild_id, allowed_user_ids, mention_only, process_bot_messages, ... })`.

**Standard cycle.**

## Task C3: `SlackProvisioner`

**Prompt schedule:**
1. `Prompt` (secret) — bot token (`xoxb-...`).
2. `Prompt` (secret, optional) — app-level token (`xapp-...` for Socket Mode).
3. `Prompt` (text, optional) — channel ID restriction.
4. `Prompt` (text, optional) — allowed user IDs.

**Validation:** `probe_post("https://slack.com/api/auth.test", &[("Authorization", "Bearer <token>")], "")` → JSON `ok: true` → success; report `team` + `user`.

**Config writes:** `config.channels_config.slack = Some(SlackConfig { bot_token, app_token, channel_id, allowed_user_ids, ... })`.

**Standard cycle.**

## Task C4: `WhatsAppCloudProvisioner` (Cloud API, distinct from existing `whatsapp-web`)

**Prompt schedule:**
1. `Prompt` (secret) — access token from Meta Business Suite.
2. `Prompt` — phone number ID (numeric).
3. `Prompt` — webhook verify token (user-defined, sent back to Meta).
4. `Prompt` (secret, optional) — app secret for webhook signature verification.
5. `Prompt` (text, optional) — allowed phone numbers (comma-separated E.164, or `*`).

**Validation:** `probe_get("https://graph.facebook.com/v19.0/<phone_number_id>", &[("Authorization", "Bearer <token>")])` → 200 with `display_phone_number` → success.

**Config writes:** `config.channels_config.whatsapp` (preserving any `session_path`/`pair_phone` from `whatsapp-web`):

```rust
config.channels_config.whatsapp = Some(WhatsAppConfig {
    access_token: Some(token),
    phone_number_id: Some(phone_id),
    verify_token: Some(verify),
    app_secret: secret,
    session_path: existing.and_then(|c| c.session_path.clone()),
    pair_phone: existing.and_then(|c| c.pair_phone.clone()),
    pair_code: existing.and_then(|c| c.pair_code.clone()),
    allowed_numbers,
});
```

**Note:** This name conflicts with `whatsapp-web` in the picker; register as `whatsapp-cloud` to disambiguate. Both write to the same `WhatsAppConfig` struct (it supports both modes per `src/config/schema.rs:2540`).

**Standard cycle.**

## Task C5: `SignalProvisioner`

**Prompt schedule:**
1. `Prompt` — signal-cli daemon socket path. Default: `/var/run/signal-cli/socket`.
2. `Prompt` — your Signal phone number (E.164).
3. `Prompt` (text, optional) — allowed numbers (comma-separated E.164, or `*`).

**Validation:** `validate::file::assert_path_exists(socket_path)` — Signal socket must exist.

**Config writes:** `config.channels_config.signal = Some(SignalConfig { socket_path, account, allowed_numbers, ... })`.

**Standard cycle.**

## Task C6: `MatrixProvisioner`

**Prompt schedule:**
1. `Prompt` — homeserver URL (e.g. `https://matrix.org`).
2. `Prompt` (secret) — access token.
3. `Prompt` (text, optional) — bot user ID (e.g. `@bot:matrix.org`).
4. `Prompt` (text, optional) — device ID.
5. `Prompt` — listen room ID (e.g. `!abc123:matrix.org`).
6. `Prompt` (text, optional) — allowed user IDs.

**Validation:** `probe_get(format!("{homeserver}/_matrix/client/r0/account/whoami"), &[("Authorization", "Bearer <token>")])` → 200 with `user_id` → success.

**Config writes:** `config.channels_config.matrix = Some(MatrixConfig { homeserver_url, access_token, user_id, device_id, room_id, allowed_user_ids })`.

**Standard cycle.**

## Task C7: `MattermostProvisioner`

**Prompt schedule:**
1. `Prompt` — server URL.
2. `Prompt` (secret) — personal access token.
3. `Prompt` — team ID.
4. `Prompt` (text, optional) — channel ID restriction.
5. `Prompt` (text, optional) — allowed user IDs.

**Validation:** `probe_get(format!("{server}/api/v4/users/me"), &[("Authorization", "Bearer <token>")])` → 200 with `username` → success.

**Config writes:** `config.channels_config.mattermost = Some(MattermostConfig { ... })`.

**Standard cycle.**

## Task C8: `IMessageProvisioner`

**Prompt schedule:**
1. `Message { Info }` — "iMessage requires macOS with Full Disk Access for Terminal/iTerm. See docs/channels-imessage.md."
2. `Choose` — confirm prerequisites: `["I have granted Full Disk Access", "Cancel setup"]`.
3. `Prompt` (text, optional) — allowed contacts (comma-separated phone numbers or emails).

**Validation:** Read `~/Library/Messages/chat.db` (1 byte) to verify access; if `Permission denied`, fail with helpful message pointing to System Settings → Privacy.

**Config writes:** `config.channels_config.imessage = Some(IMessageConfig { allowed_contacts, ... })`.

**Note:** Skip on non-macOS — emit `Failed { error: "iMessage is macOS-only" }` and bail. Use `cfg!(target_os = "macos")` at runtime.

**Standard cycle.**

## Task C9: `LarkProvisioner`

**Prompt schedule:**
1. `Prompt` — app ID (from Lark Developer Console).
2. `Prompt` (secret) — app secret.
3. `Prompt` — verification token.
4. `Prompt` (secret, optional) — encrypt key.
5. `Prompt` (text, optional) — allowed open IDs.

**Validation:** `probe_post("https://open.larksuite.com/open-apis/auth/v3/tenant_access_token/internal", &[], json!({"app_id": app_id, "app_secret": secret}).to_string())` → 200 with `tenant_access_token` → success.

**Config writes:** `config.channels_config.lark = Some(LarkConfig { ... })`.

**Standard cycle.**

## Task C10: `DingTalkProvisioner`

**Prompt schedule:**
1. `Prompt` — app key.
2. `Prompt` (secret) — app secret.
3. `Prompt` — robot code.
4. `Prompt` (text, optional) — allowed user IDs.

**Validation:** `probe_get(format!("https://api.dingtalk.com/v1.0/oauth2/accessToken?appkey={key}&appsecret={secret}"), &[])` → 200 with `accessToken` → success.

**Config writes:** `config.channels_config.dingtalk = Some(DingTalkConfig { ... })`.

**Standard cycle.**

## Task C11: `NextcloudTalkProvisioner`

**Prompt schedule:**
1. `Prompt` — Nextcloud server URL.
2. `Prompt` — username.
3. `Prompt` (secret) — app password (generated in Nextcloud → Settings → Security).
4. `Prompt` (text, optional) — room tokens to listen in.

**Validation:** `probe_get(format!("{server}/ocs/v2.php/cloud/user"), &[("OCS-APIRequest", "true"), ("Authorization", "Basic <base64(user:pass)>")])` → 200 with `displayname` → success.

**Config writes:** `config.channels_config.nextcloud_talk = Some(NextcloudTalkConfig { ... })`.

**Standard cycle.**

## Task C12: `QqProvisioner`

**Prompt schedule:**
1. `Prompt` — QQ bot framework webhook URL (e.g. `http://127.0.0.1:5700`).
2. `Prompt` (secret, optional) — access token.
3. `Prompt` (text, optional) — allowed group IDs.
4. `Prompt` (text, optional) — allowed user IDs.

**Validation:** `probe_get(format!("{webhook}/get_status"), &[])` → 200 → success.

**Config writes:** `config.channels_config.qq = Some(QqConfig { ... })`.

**Standard cycle.**

## Task C13: `EmailProvisioner`

**Prompt schedule:**
1. `Prompt` — IMAP host. Default: `imap.gmail.com`.
2. `Prompt` — IMAP port. Default: `993`.
3. `Prompt` — IMAP folder. Default: `INBOX`.
4. `Prompt` — SMTP host. Default: `smtp.gmail.com`.
5. `Prompt` — SMTP port. Default: `587`.
6. `Prompt` — from address.
7. `Prompt` (secret) — IMAP/SMTP password (or app password).
8. `Prompt` — IDLE timeout (seconds). Default: `1740`.
9. `Prompt` — poll interval (seconds). Default: `60`.

**Validation:** Open IMAP connection with `imap` crate; SELECT folder; LOGOUT. Report on failure.

**Config writes:** `config.channels_config.email = Some(EmailConfig { ... })`.

**Standard cycle.**

## Task C14: `IrcProvisioner`

**Prompt schedule:**
1. `Prompt` — server (e.g. `irc.libera.chat`).
2. `Prompt` — port. Default: `6697`.
3. `Choose` — TLS: `["Yes (recommended)", "No"]`.
4. `Prompt` — nickname.
5. `Prompt` (text, optional) — NickServ password.
6. `Prompt` (multi-line / comma-separated) — channels to join.

**Validation:** Open TCP connection to `(server, port)` with 5s timeout. Don't fully handshake — just confirm the port is open.

**Config writes:** `config.channels_config.irc = Some(IrcConfig { ... })`.

**Standard cycle.**

## Task C15: `LinqProvisioner`

**Prompt schedule:**
1. `Prompt` (secret) — Linq Partner API token.
2. `Prompt` — sender phone number (E.164).
3. `Prompt` (secret) — webhook signing secret.

**Validation:** `probe_get("https://api.linq.com/v1/account", &[("Authorization", "Bearer <token>")])` → 200 → success.

**Config writes:** `config.channels_config.linq = Some(LinqConfig { ... })`.

**Standard cycle.**

---

# Phase D — Tier-2 surfaces (14 tasks)

These have no legacy `SetupSection` — design from `Config` schema. Each is a small handful of prompts.

## Task D1: `MemoryProvisioner`

**Prompt schedule:**
1. `Choose` (single-select) — backend: `["sqlite (default, embedded)", "lucid (high-performance)", "postgres (server)", "markdown (file-based)", "none (no memory)"]`.
2. If sqlite: `Prompt` — db path. Default: `<profile>/memory.db`.
3. If postgres: `Prompt` — DSN (e.g. `postgres://user:pass@host:5432/db`). `Prompt` (secret) — password if not in DSN.
4. If markdown: `Prompt` — directory path. Default: `<profile>/memory/`.
5. If lucid: `Prompt` — server URL. `Prompt` (secret) — API key.

**Validation:** sqlite/markdown → `assert_path_writable`. postgres → connect via `tokio_postgres`. lucid → `probe_get` against server.

**Config writes:** `config.memory = MemoryConfig { backend, ... }`.

**Standard cycle.**

## Task D2: `RuntimeProvisioner`

**Prompt schedule:**
1. `Choose` — runtime kind: `["native (default)", "docker"]`.
2. If docker: `Prompt` — docker image name. Default: `rantaiclaw/runtime:latest`.
3. If docker: `Prompt` — workspace mount path. Default: `<profile>/workspace`.

**Validation:** native → no-op. docker → `validate::process::validate_command_on_path("docker")` + `docker info` exit 0.

**Config writes:** `config.runtime = RuntimeConfig { kind, ... }`.

**Standard cycle.**

## Task D3: `ProxyProvisioner`

**Prompt schedule:**
1. `Choose` — enable proxy: `["No", "Yes — for all", "Yes — for selected services"]`.
2. If enabled: `Prompt` — HTTP proxy URL.
3. `Prompt` — HTTPS proxy URL.
4. `Prompt` (text, optional) — fallback proxy URL.
5. `Prompt` (text, optional) — no-proxy bypass list (CSV).
6. If "selected services": `Choose` (multi-select) — services to proxy. Options: `["providers", "channels", "mcp", "skills"]`.

**Config writes:** `config.proxy = ProxyConfig { enabled, http, https, ... }`.

**Standard cycle.**

## Task D4: `TunnelProvisioner`

**Prompt schedule:**
1. `Choose` — provider: `["None", "Cloudflare Tunnel", "Tailscale", "ngrok", "Custom command"]`.
2. Per-provider prompts (Cloudflare: tunnel ID + cred file; Tailscale: hostname + auth key; ngrok: auth token; custom: command + URL pattern regex).

**Validation:** `validate_command_on_path` for the provider's CLI (`cloudflared`, `tailscale`, `ngrok`).

**Config writes:** `config.tunnel = TunnelConfig { provider, ... }`.

**Standard cycle.**

## Task D5: `GatewayProvisioner`

**Prompt schedule:**
1. `Choose` — enable webhook gateway: `["No", "Yes"]`.
2. If yes: `Prompt` — bind address. Default: `127.0.0.1:8080`. Warn if `0.0.0.0` (public).
3. `Prompt` (secret) — webhook signing secret.
4. `Prompt` (text, optional) — allowed source IPs (CIDR).

**Validation:** Try `bind` to address; release immediately. Fail if bound.

**Config writes:** `config.gateway = GatewayConfig { enabled, bind, secret, allowed_ips, ... }`.

**Standard cycle.**

## Task D6: `BrowserProvisioner`

**Prompt schedule:**
1. `Choose` — browser backend: `["None", "Chromium (headless)", "Computer-use (Anthropic)"]`.
2. If chromium: `Prompt` — chromium binary path. Auto-detect via `which::which("chromium")` / `chromium-browser` / `google-chrome`.
3. If computer-use: `Prompt` — viewport width. `Prompt` — viewport height. `Prompt` — screenshot quality.

**Validation:** Chromium binary exists + `--version` exit 0.

**Config writes:** `config.browser = BrowserConfig { backend, ... }`.

**Standard cycle.**

## Task D7: `WebSearchProvisioner`

**Prompt schedule:**
1. `Choose` — provider: `["DuckDuckGo (no key)", "Tavily", "Serper", "Brave", "None"]`.
2. If keyed: `Prompt` (secret) — API key.
3. `Prompt` — max results per query. Default: `10`.

**Validation:** `probe_get` against provider's auth/health endpoint with key.

**Config writes:** `config.web_search = WebSearchConfig { provider, api_key, max_results }`.

**Standard cycle.**

## Task D8: `MultimodalProvisioner`

**Prompt schedule:**
1. `Choose` (multi-select) — enabled modalities: `["image", "audio", "video", "pdf"]`.
2. For each enabled: `Prompt` — max file size in MB. Defaults per modality (image=10, audio=50, video=200, pdf=20).
3. `Choose` — vision provider: `["openai", "anthropic", "gemini", "local"]`. (Pulls from `config.providers` for available ones.)

**Config writes:** `config.multimodal = MultimodalConfig { ... }`.

**Standard cycle.**

## Task D9: `PeripheralsProvisioner`

**Prompt schedule:**
1. `Choose` (multi-select) — boards to register: `["STM32 (USB serial)", "Raspberry Pi GPIO", "Arduino (USB)"]`.
2. Per board: `Prompt` for serial path / GPIO chip / USB device. Auto-detect via `validate::process::list_serial_ports`.

**Config writes:** `config.peripherals.boards.push(PeripheralBoardConfig { ... })`.

**Note:** Reference `docs/hardware-peripherals-design.md`.

**Standard cycle.**

## Task D10: `HardwareProvisioner`

**Prompt schedule:**
1. `Choose` (multi-select) — hardware groups: `["motors", "sensors", "displays", "cameras"]`.
2. Per group: `Prompt` for device count + interface (I2C/SPI/UART).

**Config writes:** `config.hardware = HardwareConfig { ... }`.

**Standard cycle.**

## Task D11: `ComposioProvisioner`

**Prompt schedule:**
1. `Prompt` (secret) — Composio API key.
2. `Choose` (multi-select) — tool packs to enable. Options fetched live from `https://backend.composio.dev/api/v2/tools`.

**Validation:** `probe_get("https://backend.composio.dev/api/v2/auth/whoami", &[("X-API-Key", "<key>")])` → 200 → success.

**Config writes:** `config.composio = ComposioConfig { api_key, enabled_tools, ... }`.

**Standard cycle.**

## Task D12: `SecretsProvisioner`

**Prompt schedule:**
1. `Choose` — backend: `["Local file (encrypted)", "OS keyring", "1Password CLI", "AWS Secrets Manager", "HashiCorp Vault"]`.
2. Per backend: appropriate config prompts (path, account, region, vault URL+token).

**Validation:** Each backend tests round-trip: write a sentinel secret, read it back, delete it.

**Config writes:** `config.secrets = SecretsConfig { backend, ... }`.

**Standard cycle.**

## Task D13: `AgentsProvisioner` (sub-agent delegation)

**Prompt schedule:**
1. `Choose` (multi-select) — built-in delegate agents to enable: `["researcher", "coder", "planner", "reviewer", "debugger"]`.
2. `Prompt` (text, optional) — custom agent name. If provided: `Prompt` for system prompt + `Choose` for default model.

**Config writes:** `config.agents.insert(name, DelegateAgentConfig { ... })`.

**Standard cycle.**

## Task D14: `ModelRoutesProvisioner` + `EmbeddingRoutesProvisioner`

Two provisioners, same shape — embeddings = same flow with different `Config` field. Implement together; commit separately.

**Prompt schedule (each):**
1. `Choose` — add a route or skip: `["Add route", "Done"]` (loop until Done).
2. Per route: `Prompt` for pattern (regex on user message). `Choose` for target provider+model from `config.providers`. `Prompt` for priority (integer).

**Config writes:** `config.model_routes.push(ModelRouteConfig { pattern, provider, model, priority })`. Same for `embedding_routes`.

**Standard cycle for each.**

---

# Phase E — Picker integration

## Task E1: Top picker auto-grouping with collapsible sections

After Phases B-D, the registry has 35+ entries. Flat list is ugly. Update `SetupCommand::execute` to render category headers as non-selectable rows:

**Files:**
- Modify: `src/tui/widgets/list_picker.rs` — add a `header: bool` field to `ListPickerItem`. When `header = true`, the item renders muted/bold and Enter on it is a no-op.
- Modify: `src/tui/commands/setup.rs` — emit a header item before each group.

**Test (`tests/tui_setup_picker.rs`):**

```rust
#[test]
fn setup_topic_picker_groups_by_category() {
    let registry = CommandRegistry::new();
    let (mut ctx, _, _) = TuiContext::test_context();
    let r = registry.dispatch("/setup", &mut ctx).unwrap();
    let picker = match r { CommandResult::OpenListPicker(p) => p, _ => panic!() };
    let labels: Vec<&str> = picker.items.iter().map(|i| i.primary.as_str()).collect();
    // Group headers must come before their members.
    let core_idx = labels.iter().position(|s| *s == "── Core ──").unwrap();
    let channel_idx = labels.iter().position(|s| *s == "── Channels ──").unwrap();
    assert!(core_idx < channel_idx);
}
```

- [ ] Standard test/impl/commit cycle.

## Task E2: Sub-pickers with search-as-you-type

The Channel sub-picker with 16+ entries needs the same search filter as `/sessions`. The `ListPicker` already supports it — verify `SetupChannel` kind uses the same `render_fullscreen` path and search box.

- [ ] **Step 1:** Verify by inspection: `grep -n "ListPickerKind::SetupChannel" src/tui/app.rs` — confirm rendering branch.
- [ ] **Step 2:** Add manual smoke test: `/setup` → Channels → type `mat` → expect filter to show only `matrix` and `mattermost`.
- [ ] **Step 3:** If broken, fix the render path; otherwise commit a docs note.

---

# Phase F — Validation suite

## Task F1: End-to-end provisioner test runner

Iterate every registered provisioner; run it with a scripted fake driver that auto-replies to every Prompt with the default and every Choose with index 0. Assert each provisioner reaches `Done` or `Failed` (not infinite hang) within 5s.

**File:** `tests/all_provisioners_smoke.rs`

```rust
#[tokio::test]
async fn every_provisioner_completes_within_5s_on_default_responses() {
    use rantaiclaw::onboard::provision::{available, provisioner_for, ProvisionEvent, ProvisionIo, ProvisionResponse};
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    for (name, _desc) in available() {
        let p = provisioner_for(name).expect("registry returns Some for listed name");
        let tmp = tempfile::tempdir().unwrap();
        let mut config = rantaiclaw::config::Config::default();
        let profile = rantaiclaw::profile::Profile {
            name: "test".into(), root: tmp.path().to_path_buf(),
        };
        let (etx, mut erx) = mpsc::channel(32);
        let (rtx, rrx) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            p.run(&mut config, &profile, ProvisionIo { events: etx, responses: rrx }).await
        });
        let result = timeout(Duration::from_secs(5), async {
            while let Some(ev) = erx.recv().await {
                match ev {
                    ProvisionEvent::Prompt { default, .. } => {
                        let _ = rtx.send(ProvisionResponse::Text(default.unwrap_or_default())).await;
                    }
                    ProvisionEvent::Choose { multi: false, .. } => {
                        let _ = rtx.send(ProvisionResponse::Selection(vec![0])).await;
                    }
                    ProvisionEvent::Choose { multi: true, .. } => {
                        let _ = rtx.send(ProvisionResponse::Selection(vec![])).await;
                    }
                    ProvisionEvent::Done { .. } | ProvisionEvent::Failed { .. } => return Ok(()),
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }).await;
        assert!(result.is_ok(), "{name} did not complete within 5s");
        let _ = task.await;
    }
}
```

- [ ] Standard test/impl/commit cycle. Test FAILS until every provisioner is ported (Phase B-D); use `#[ignore]` on this test until then OR run it as a CI integration check that verifies coverage.

---

# Phase G — Legacy removal

## Task G1: Remove `dialoguer` setup wizard + `SetupSection` impls

After all provisioners ship and `tests/all_provisioners_smoke.rs` is green, the legacy wizard is dead code.

**Files to delete:**
- `src/onboard/wizard.rs` (~6492 lines)
- `src/onboard/section/{provider,channels,persona,skills,mcp,approvals}.rs`
- `src/onboard/section/mod.rs`

**Files to modify:**
- `src/main.rs` — delete `Commands::Setup` legacy non-interactive branch (the one that calls `onboard::wizard::run_setup`); keep the new headless branch that uses `provisioner_for`. The non-interactive driver must support every category now.
- `src/onboard/mod.rs` — drop `pub mod wizard; pub mod section;`.

**Cargo.toml:**
- Remove `dialoguer` from dependencies (verify nothing else uses it: `grep -rn "use dialoguer" src/`).

- [ ] **Step 1:** Verify zero references: `grep -rn "onboard::wizard\|onboard::section" src/ tests/` returns nothing.
- [ ] **Step 2:** Delete files (`git rm`).
- [ ] **Step 3:** Update `src/main.rs` to use the new headless driver for every section.
- [ ] **Step 4:** Run full suite: `cargo test -p rantaiclaw --features whatsapp-web && cargo clippy --all-targets -- -D warnings`.
- [ ] **Step 5:** Commit `refactor(onboard): remove dialoguer setup wizard; TuiProvisioners are the canonical setup`.

## Task G2: Documentation update

**Files:**
- Modify: `docs/commands-reference.md` — `/setup` is the only setup interface; document the picker categories.
- Modify: `docs/CLAUDE.md` (if exists) or main README — replace any `rantaiclaw setup --interactive` references with `/setup` inside the TUI.

- [ ] **Step 1:** Update docs.
- [ ] **Step 2:** Commit `docs: update setup references — /setup is the canonical entry point`.

---

## Self-Review Notes

**1. Spec coverage** — User asked for "all setup needed" with no exception. Plan covers:
- All 6 legacy `SetupSection` impls (Phase B + persona-polish; whatsapp-web already shipped).
- All 15 channel types from the legacy `setup channels` flow (Phase C).
- 14 additional configurable surfaces from `Config` schema that didn't have a `SetupSection` (Phase D): memory, runtime, proxy, tunnel, gateway, browser, web_search, multimodal, peripherals, hardware, composio, secrets, agents, model_routes + embedding_routes.
- Picker integration (Phase E) so the registry growth doesn't make `/setup` unusable.
- Test coverage that asserts every provisioner completes (Phase F).
- Legacy removal (Phase G) so the surface area stops drifting.

**2. Placeholder scan** — Each task names the exact prompt sequence (Prompt vs Choose, IDs, defaults), the exact `Config` field path written, and the validation method. The standard test+impl+commit cycle is templated at the top with a runnable test scaffold. Tasks B1–E2 reference the template instead of repeating the boilerplate, but each enumerates the section-specific data (prompts, config writes, validation) that's the actual content the engineer needs.

**3. Type consistency** — `ProvisionerCategory` enum (Task A1) is consumed by Tasks A3, B1-B5, C1-C15, D1-D14, E1. `validate::http::probe_get/probe_post` (Task A2) is consumed by every Phase B+C task that does live validation. `available()` and `provisioner_for(name)` are the registry contract used by Tasks A3, E1, F1, G1 — signatures match.

**4. Risk flags:**
- Phase B3 (`SkillsProvisioner`) depends on the audit §7 `clawhub::install_one` rewrite. If that hasn't shipped, ClawHub picks write stub placeholders. Plan calls this out; ship Phase B3 anyway.
- Phase D12 (`SecretsProvisioner`) needs cross-platform care — OS keyring requires Secret Service on Linux, Keychain on macOS, Credential Manager on Windows. The `keyring` crate handles this but each backend needs a smoke test.
- Phase G1 (legacy removal) is a single big-bang commit. Prefer doing it after all per-section ports have soaked in `main` for at least a week so any missed usage surfaces.

**5. Validation matrix (per CLAUDE.md §8):** Each task ends with `cargo test -p rantaiclaw`. Phase F adds `cargo test --test all_provisioners_smoke`. Phase G adds the full clippy gate `cargo clippy --all-targets -- -D warnings`.

**6. Estimated effort:** 35 tasks across 7 phases. Phase A is ~1-2 days (framework + helpers). Phase B is ~2-3 days (5 sections, dense prompts). Phase C is ~3-5 days (15 channels, mostly mechanical). Phase D is ~3-5 days (14 surfaces, some need new helpers). Phases E-G are ~1-2 days. Total: ~2-3 weeks for one engineer, much less in parallel via subagent-driven-development with one provisioner per subagent.
