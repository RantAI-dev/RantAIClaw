# TUI Setup Interactive Picker — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the in-TUI setup picker UX (search + scroll like `/sessions`) plus the missing pieces of the `TuiProvisioner` contract (`Choose` event rendering and input). Port **one** legacy `SetupSection` (`persona`) end-to-end as proof of pattern. NO TUI suspend, NO context switch — every prompt renders inside the overlay. The picker only lists sections that are actually wired in-TUI; later plans port the remaining sections one at a time and they appear automatically.

**Boundary correction (vs. prior plan):**
- Top-level picker shows **categories only**: Provider, Channels, Persona, Skills, MCP, Approvals. ("Run full setup" is deferred to a later plan once ≥2 sections are ported — meaningless aggregator with one section.)
- Channel types (telegram, discord, whatsapp-cloud, whatsapp-web, signal, …) live **inside** the Channels sub-picker. WhatsApp Web is one channel among many; it does NOT appear at top level.
- Phase 1 wires only what's ported. Unported sections don't appear in the picker at all — no "coming soon" stubs, no half-implemented entries.

**Architecture:**
- Extend `ListPickerKind` with `SetupTopic` and `SetupChannel` variants (pattern matches existing `Model`/`Session`/`Personality` kinds).
- Extend `SetupOverlayState` to render and handle `ProvisionEvent::Choose` events (the contract already includes them; today the overlay just logs `(choose UI not yet wired)`).
- Add `PersonaProvisioner` implementing `TuiProvisioner` — translates the existing `src/onboard/section/persona.rs` dialoguer flow into `Prompt`/`Choose`/`Message` events.
- `/setup` (no args) returns `OpenListPicker(SetupTopic)`. `/setup <name>` jumps directly. Pure functions `dispatch_setup_topic_key` and `dispatch_setup_channel_key` make routing unit-testable.

**What replaces the dialoguer suspend:** Each remaining `SetupSection` (provider, channels, skills, mcp, approvals) is ported one-per-plan to a `TuiProvisioner`. Until ported, that section is invisible in the picker. Users wanting a still-unported section can still run `rantaiclaw setup <name> --non-interactive` from a shell — that's a deliberate, scripted choice, not an in-TUI surprise.

**Tech Stack:** Rust, existing `ratatui` `ListPicker`, existing `SetupOverlayState` widget, `tokio::sync::mpsc` for the `ProvisionIo` contract.

**Out of scope (separate plans):**
- Porting `provider`, `channels` (full per-channel set), `skills`, `mcp`, `approvals` — one plan per section.
- "Run full setup" aggregator entry — needs ≥2 ported sections to be useful.
- Per-channel TuiProvisioners beyond `whatsapp-web` (telegram, discord, slack, signal, matrix, mattermost, imessage, lark, dingtalk, nextcloud-talk, qq, email, irc, linq) — each is its own plan.
- ClawHub `install_one` rewrite, MCP zero-auth validation (audit follow-ups).

---

## File Structure

**New files:**
- `src/onboard/provision/persona.rs` — `PersonaProvisioner` impl.
- `tests/tui_setup_picker.rs` — picker dispatch routing tests.
- `tests/provision_persona.rs` — `PersonaProvisioner` event-flow tests with a fake driver.

**Modified files:**
- `src/tui/widgets/list_picker.rs:21` — extend `ListPickerKind` enum.
- `src/tui/widgets/setup_overlay.rs` — add `ChoosePrompt` state, `handle_event` arm for `Choose`, render choice block, key handlers (Up/Down/Space/Enter).
- `src/tui/commands/setup.rs` — `/setup` returns picker for no-args, overlay for named.
- `src/tui/app.rs` — extend `handle_list_picker_enter` with `SetupTopic`/`SetupChannel` arms; route key events to overlay's choose UI when active.
- `src/onboard/provision/mod.rs` — register `persona` module.
- `src/onboard/provision/registry.rs` — register `PersonaProvisioner` (always available, no feature gate).

---

## Task 1: Extend `ListPickerKind`

**Files:**
- Modify: `src/tui/widgets/list_picker.rs:21`
- Test: inline.

- [ ] **Step 1: Write the failing test**

In `src/tui/widgets/list_picker.rs` `tests` module:

```rust
#[test]
fn setup_topic_kind_distinct_from_channel_kind() {
    assert_ne!(ListPickerKind::SetupTopic, ListPickerKind::SetupChannel);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw widgets::list_picker::tests::setup_topic_kind_distinct_from_channel_kind`
Expected: FAIL — variants don't exist.

- [ ] **Step 3: Add variants**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPickerKind {
    Model,
    Session,
    Personality,
    Skill,
    Help,
    /// Top-level setup category picker — provider, channels, persona, etc.
    SetupTopic,
    /// Channel-type picker — telegram, discord, whatsapp-web, etc.
    /// Opened when SetupTopic resolves to "channels".
    SetupChannel,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rantaiclaw widgets::list_picker::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui/widgets/list_picker.rs
git commit -m "feat(tui): add SetupTopic and SetupChannel ListPickerKind variants"
```

---

## Task 2: Render and handle `Choose` events in `SetupOverlayState`

The `TuiProvisioner` contract already has `ProvisionEvent::Choose { id, label, options, multi }` and `ProvisionResponse::Selection(Vec<usize>)`. Today the overlay logs `(choose UI not yet wired)` and discards the event. This task implements the in-overlay choose widget.

**UX:**
- Single-select: Up/Down moves cursor; Enter confirms; Esc cancels (sends `Cancelled`).
- Multi-select: Up/Down + Space toggles; Enter confirms with current selection; Esc cancels.
- Display: bold label, then one row per option with `( )` / `(*)` for single or `[ ]` / `[x]` for multi, cursor row inverted.

**Files:**
- Modify: `src/tui/widgets/setup_overlay.rs`
- Modify: `src/tui/app.rs` — key handler must route Up/Down/Space when `setup_overlay.has_active_choose()`.
- Test: `tests/tui_setup_overlay.rs` — extend.

- [ ] **Step 1: Write the failing tests**

Append to `tests/tui_setup_overlay.rs`:

```rust
use rantaiclaw::tui::widgets::setup_overlay::SetupOverlayState;
use rantaiclaw::onboard::provision::ProvisionEvent;

#[test]
fn choose_event_sets_active_choose_state() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "tier".into(),
        label: "Pick a tier".into(),
        options: vec!["L1".into(), "L2".into(), "L3".into(), "L4".into()],
        multi: false,
    });
    assert!(s.active_choose().is_some());
    let c = s.active_choose().unwrap();
    assert_eq!(c.label, "Pick a tier");
    assert_eq!(c.options.len(), 4);
    assert_eq!(c.cursor, 0);
    assert!(!c.multi);
    assert!(c.selected.is_empty());
}

#[test]
fn choose_single_select_submits_one_index() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "tier".into(), label: "x".into(),
        options: vec!["a".into(), "b".into(), "c".into()],
        multi: false,
    });
    s.choose_move_down();
    s.choose_move_down();
    let (id, sel) = s.submit_choose().expect("submit returns Some");
    assert_eq!(id, "tier");
    assert_eq!(sel, vec![2]);
    assert!(s.active_choose().is_none(), "submit must clear active choose");
}

#[test]
fn choose_multi_select_toggles_with_space() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "skills".into(), label: "x".into(),
        options: vec!["a".into(), "b".into(), "c".into()],
        multi: true,
    });
    s.choose_toggle();             // toggles index 0
    s.choose_move_down();
    s.choose_move_down();
    s.choose_toggle();             // toggles index 2
    let (_, sel) = s.submit_choose().unwrap();
    assert_eq!(sel, vec![0, 2]);
}

#[test]
fn choose_single_select_ignores_toggle() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "x".into(), label: "x".into(),
        options: vec!["a".into(), "b".into()],
        multi: false,
    });
    s.choose_toggle();             // no-op for single-select
    let (_, sel) = s.submit_choose().unwrap();
    assert_eq!(sel, vec![0]);      // single-select returns just the cursor
}

#[test]
fn choose_move_up_at_zero_stays_at_zero() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "x".into(), label: "x".into(),
        options: vec!["a".into(), "b".into()],
        multi: false,
    });
    s.choose_move_up();
    assert_eq!(s.active_choose().unwrap().cursor, 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay choose`
Expected: FAIL — `active_choose`, `choose_move_up`, `choose_move_down`, `choose_toggle`, `submit_choose` don't exist.

- [ ] **Step 3: Implement the choose state**

In `src/tui/widgets/setup_overlay.rs`, add fields and methods:

```rust
#[derive(Debug, Clone)]
pub struct ActiveChoose {
    pub id: String,
    pub label: String,
    pub options: Vec<String>,
    pub multi: bool,
    pub cursor: usize,
    pub selected: Vec<usize>,  // indices into `options`; for single-select stays empty until submit
}

#[derive(Debug, Default)]
pub struct SetupOverlayState {
    pub title: String,
    log: Vec<String>,
    qr: Option<(String, String)>,
    prompt: Option<ActivePrompt>,
    choose: Option<ActiveChoose>,
    input: String,
    pub closed: bool,
}

impl SetupOverlayState {
    // ... existing methods unchanged ...

    pub fn active_choose(&self) -> Option<&ActiveChoose> { self.choose.as_ref() }

    pub fn choose_move_up(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if c.cursor > 0 { c.cursor -= 1; }
        }
    }

    pub fn choose_move_down(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if c.cursor + 1 < c.options.len() { c.cursor += 1; }
        }
    }

    pub fn choose_toggle(&mut self) {
        if let Some(c) = self.choose.as_mut() {
            if !c.multi { return; }
            let pos = c.cursor;
            if let Some(idx) = c.selected.iter().position(|&i| i == pos) {
                c.selected.remove(idx);
            } else {
                c.selected.push(pos);
                c.selected.sort_unstable();
            }
        }
    }

    /// Returns `(id, selection)` and clears active choose. For single-select,
    /// selection is `vec![cursor]`. For multi-select, returns `selected` as-is.
    pub fn submit_choose(&mut self) -> Option<(String, Vec<usize>)> {
        let c = self.choose.take()?;
        let sel = if c.multi { c.selected } else { vec![c.cursor] };
        Some((c.id, sel))
    }
}
```

Update `handle_event` to populate `choose` instead of logging:

```rust
ProvisionEvent::Choose { id, label, options, multi } => {
    self.choose = Some(ActiveChoose {
        id, label, options, multi,
        cursor: 0,
        selected: Vec::new(),
    });
}
```

Update `render` to draw the choose block when present. Insert after the QR block, before the prompt block:

```rust
if let Some(c) = &self.choose {
    lines.push(Line::from(""));
    lines.push(Line::from(c.label.as_str()).style(
        Style::default().add_modifier(Modifier::BOLD)
    ));
    for (i, opt) in c.options.iter().enumerate() {
        let marker = if c.multi {
            if c.selected.contains(&i) { "[x]" } else { "[ ]" }
        } else if i == c.cursor {
            "(*)"
        } else {
            "( )"
        };
        let cursor_arrow = if i == c.cursor { "▸ " } else { "  " };
        let row = format!("{cursor_arrow}{marker} {opt}");
        let style = if i == c.cursor {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(row).style(style));
    }
    let footer = if c.multi {
        "↑/↓ navigate · Space toggle · Enter confirm · Esc cancel"
    } else {
        "↑/↓ navigate · Enter select · Esc cancel"
    };
    lines.push(Line::from(""));
    lines.push(Line::from(footer).style(Style::default().fg(Color::Rgb(107, 114, 128))));
}
```

- [ ] **Step 4: Wire key handling in `app.rs`**

In `src/tui/app.rs`, add new key arms before the existing prompt-text arms (so choose has priority when active):

```rust
// Up — choose nav.
KeyCode::Up if self.setup_overlay.as_ref().is_some_and(|o| o.active_choose().is_some()) => {
    if let Some(o) = self.setup_overlay.as_mut() { o.choose_move_up(); }
}
// Down — choose nav.
KeyCode::Down if self.setup_overlay.as_ref().is_some_and(|o| o.active_choose().is_some()) => {
    if let Some(o) = self.setup_overlay.as_mut() { o.choose_move_down(); }
}
// Space — toggle in multi-select.
KeyCode::Char(' ') if self.setup_overlay.as_ref().is_some_and(|o| o.active_choose().is_some()) => {
    if let Some(o) = self.setup_overlay.as_mut() { o.choose_toggle(); }
}
// Enter — submit choose, send Selection.
KeyCode::Enter if self.setup_overlay.as_ref().is_some_and(|o| o.active_choose().is_some()) => {
    if let Some(o) = self.setup_overlay.as_mut() {
        if let Some((_id, sel)) = o.submit_choose() {
            if let Some(tx) = &self.setup_response_tx {
                let _ = tx.send(crate::onboard::provision::ProvisionResponse::Selection(sel)).await;
            }
        }
    }
}
```

These guards must come **before** the existing `KeyCode::Enter if self.setup_overlay.is_some() && ... active_prompt() ...` arm so choose takes priority over prompt when both somehow coexist (the contract says they never do, but ordering is defense-in-depth).

- [ ] **Step 5: Run tests + smoke build**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay && cargo build -p rantaiclaw`
Expected: PASS, build clean.

- [ ] **Step 6: Commit**

```bash
git add src/tui/widgets/setup_overlay.rs src/tui/app.rs tests/tui_setup_overlay.rs
git commit -m "feat(tui): render and handle Choose events in setup overlay"
```

---

## Task 3: Port `persona` section as `PersonaProvisioner`

`persona` is the simplest legacy section to port — mostly text input plus a small Select for the persona template. Read `src/onboard/section/persona.rs` first to mirror its prompt sequence; the new provisioner emits the same prompts as `Prompt`/`Choose` events.

**Files:**
- Create: `src/onboard/provision/persona.rs`
- Modify: `src/onboard/provision/mod.rs` — `pub mod persona;`
- Modify: `src/onboard/provision/registry.rs` — register `PersonaProvisioner`.
- Test: `tests/provision_persona.rs`.

- [ ] **Step 1: Read the existing dialoguer flow**

```bash
cat src/onboard/section/persona.rs
```

Note exactly what it prompts: agent name, persona template (Select), system-prompt overrides, output file path. Capture each prompt's label, default, and validation rules. The provisioner translates each one to a `ProvisionEvent`.

- [ ] **Step 2: Write the failing tests**

`tests/provision_persona.rs`:

```rust
use rantaiclaw::onboard::provision::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, TuiProvisioner,
};
use rantaiclaw::onboard::provision::persona::PersonaProvisioner;
use tokio::sync::mpsc;

#[tokio::test]
async fn persona_provisioner_writes_persona_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = rantaiclaw::config::Config::default();
    let profile = rantaiclaw::profile::Profile {
        name: "test".into(),
        root: tmp.path().to_path_buf(),
    };

    let (etx, mut erx) = mpsc::channel(32);
    let (rtx, rrx) = mpsc::channel(8);

    let task = tokio::spawn(async move {
        PersonaProvisioner.run(
            &mut config, &profile,
            ProvisionIo { events: etx, responses: rrx },
        ).await
    });

    // Reply to whatever events the provisioner emits, in order.
    // Adjust this script after Step 3 to match the actual prompt sequence.
    while let Some(ev) = erx.recv().await {
        match ev {
            ProvisionEvent::Prompt { id, default, .. } => {
                let value = match id.as_str() {
                    "agent_name" => "TestAgent".to_string(),
                    "output_path" => default.unwrap_or_default(),
                    _ => default.unwrap_or_default(),
                };
                rtx.send(ProvisionResponse::Text(value)).await.unwrap();
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

    let persona_path = profile.root.join("persona.md");
    assert!(persona_path.exists(), "persona file must be written");
    let body = std::fs::read_to_string(&persona_path).unwrap();
    assert!(body.contains("TestAgent"), "persona file must contain the agent name");
}
```

(The exact `id`s and prompt order in this script will be adjusted to match Step 3's implementation.)

- [ ] **Step 3: Implement `PersonaProvisioner`**

`src/onboard/provision/persona.rs`:

```rust
//! In-TUI persona setup. Mirrors the prompt sequence in
//! `src/onboard/section/persona.rs` (legacy dialoguer flow) but emits
//! `ProvisionEvent`s so the TUI overlay drives the UX.

use anyhow::Result;
use async_trait::async_trait;

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};
use crate::config::Config;
use crate::profile::Profile;

pub const PERSONA_NAME: &str = "persona";
pub const PERSONA_DESC: &str = "Agent name + persona template + system prompt";

pub struct PersonaProvisioner;

#[async_trait]
impl TuiProvisioner for PersonaProvisioner {
    fn name(&self) -> &'static str { PERSONA_NAME }
    fn description(&self) -> &'static str { PERSONA_DESC }

    async fn run(
        &self,
        _config: &mut Config,
        profile: &Profile,
        io: ProvisionIo,
    ) -> Result<()> {
        let ProvisionIo { events, mut responses } = io;

        // 1. Agent name (text prompt).
        events.send(ProvisionEvent::Prompt {
            id: "agent_name".into(),
            label: "Agent name".into(),
            default: Some("RantaiClawAgent".into()),
            secret: false,
        }).await.ok();
        let agent_name = match responses.recv().await {
            Some(ProvisionResponse::Text(s)) if !s.is_empty() => s,
            Some(ProvisionResponse::Text(_)) => "RantaiClawAgent".to_string(),
            _ => {
                events.send(ProvisionEvent::Failed { error: "cancelled".into() }).await.ok();
                return Ok(());
            }
        };

        // 2. Persona template (single-select).
        // Mirror the templates listed in src/onboard/section/persona.rs.
        let templates = vec![
            "default".to_string(),
            "concise".to_string(),
            "verbose".to_string(),
            "research-assistant".to_string(),
        ];
        events.send(ProvisionEvent::Choose {
            id: "template".into(),
            label: "Choose a persona template".into(),
            options: templates.clone(),
            multi: false,
        }).await.ok();
        let template_idx = match responses.recv().await {
            Some(ProvisionResponse::Selection(v)) if !v.is_empty() => v[0],
            _ => 0,
        };
        let template = templates.get(template_idx).cloned().unwrap_or_else(|| "default".into());

        // 3. Write the persona file.
        let persona_path = profile.root.join("persona.md");
        if let Some(parent) = persona_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = format!(
            "# {agent_name}\n\nTemplate: {template}\n\n<system prompt body — load from template `{template}` here>\n"
        );
        std::fs::write(&persona_path, body)?;

        events.send(ProvisionEvent::Message {
            severity: Severity::Success,
            text: format!("Wrote {}", persona_path.display()),
        }).await.ok();

        events.send(ProvisionEvent::Done {
            summary: format!("Persona configured: {agent_name} ({template})"),
        }).await.ok();
        Ok(())
    }
}
```

The body string is intentionally a placeholder — port the actual template-load logic from `src/onboard/section/persona.rs` so this provisioner produces the same `persona.md` content the legacy flow would.

Register in `src/onboard/provision/mod.rs`:

```rust
pub mod persona;
```

In `src/onboard/provision/registry.rs`:

```rust
pub fn provisioner_for(name: &str) -> Option<Box<dyn TuiProvisioner>> {
    match name {
        "persona" => Some(Box::new(super::persona::PersonaProvisioner)),
        #[cfg(feature = "whatsapp-web")]
        "whatsapp-web" => Some(Box::new(super::whatsapp_web::WhatsAppWebProvisioner::default())),
        _ => None,
    }
}

pub fn available() -> Vec<(&'static str, &'static str)> {
    let mut list = vec![
        (super::persona::PERSONA_NAME, super::persona::PERSONA_DESC),
    ];
    #[cfg(feature = "whatsapp-web")]
    list.push((super::whatsapp_web::WHATSAPP_WEB_NAME, super::whatsapp_web::WHATSAPP_WEB_DESC));
    list
}
```

- [ ] **Step 4: Run tests + smoke build**

Run: `cargo test -p rantaiclaw --test provision_persona && cargo test -p rantaiclaw onboard::provision`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/onboard/provision/persona.rs src/onboard/provision/mod.rs src/onboard/provision/registry.rs tests/provision_persona.rs
git commit -m "feat(onboard): PersonaProvisioner — first legacy section ported to TuiProvisioner"
```

---

## Task 4: `/setup` opens `SetupTopic` picker

The picker shows **categories**: Persona (always), Channels (always — its sub-picker handles ported channels). Provider/Skills/MCP/Approvals are NOT in the picker yet because they have no `TuiProvisioner` impl — they appear in later plans as those ports land.

WhatsApp Web is **not** at top level. Users wanting WhatsApp Web pick "Channels" → "WhatsApp Web".

**Files:**
- Modify: `src/tui/commands/setup.rs`
- Modify: `tests/tui_setup_overlay.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/tui_setup_overlay.rs`:

```rust
#[test]
fn slash_setup_no_arg_returns_setup_topic_picker_with_categories_only() {
    use rantaiclaw::tui::widgets::ListPickerKind;
    let registry = CommandRegistry::new();
    let (mut ctx, _rx, _tx) = TuiContext::test_context();
    let r = registry.dispatch("/setup", &mut ctx).unwrap();
    match r {
        CommandResult::OpenListPicker(picker) => {
            assert_eq!(picker.kind, ListPickerKind::SetupTopic);
            let keys: Vec<_> = picker.items.iter().map(|i| i.key.as_str()).collect();
            assert!(keys.contains(&"persona"), "persona must be listed");
            assert!(keys.contains(&"channels"), "channels must be listed");
            // WhatsApp Web is a channel — must NOT appear at the top level.
            assert!(!keys.contains(&"whatsapp-web"), "whatsapp-web is a channel; it belongs in the Channels sub-picker, not at top level");
            // Unported sections must not be listed yet — they appear when ported.
            assert!(!keys.contains(&"provider"));
            assert!(!keys.contains(&"skills"));
            assert!(!keys.contains(&"mcp"));
            assert!(!keys.contains(&"approvals"));
        }
        other => panic!("expected OpenListPicker, got {other:?}"),
    }
}

#[test]
fn slash_setup_persona_jumps_to_overlay() {
    let registry = CommandRegistry::new();
    let (mut ctx, _rx, _tx) = TuiContext::test_context();
    let r = registry.dispatch("/setup persona", &mut ctx).unwrap();
    assert!(matches!(
        r,
        CommandResult::OpenSetupOverlay { provisioner: Some(ref n) } if n == "persona"
    ));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay slash_setup`
Expected: FAIL.

- [ ] **Step 3: Implement `SetupCommand`**

Replace `src/tui/commands/setup.rs`:

```rust
use anyhow::Result;
use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

pub struct SetupCommand;

impl CommandHandler for SetupCommand {
    fn name(&self) -> &str { "setup" }
    fn description(&self) -> &str { "Configure providers, channels, and integrations" }
    fn usage(&self) -> &str { "setup [topic]" }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let arg = args.trim();
        if !arg.is_empty() {
            // Direct named entry — `/setup persona`, `/setup whatsapp-web`.
            // The dispatcher in app.rs decides whether it's a TuiProvisioner
            // (overlay) or a category (sub-picker).
            return Ok(CommandResult::OpenSetupOverlay {
                provisioner: Some(arg.to_string()),
            });
        }

        // No arg → show the category picker.
        // ONLY categories that have at least one TuiProvisioner backing
        // them appear here. As more sections are ported, more entries
        // appear automatically (no "coming soon" stubs).
        let mut items = Vec::new();

        // Persona — always available (PersonaProvisioner is unconditional).
        items.push(ListPickerItem {
            key: "persona".into(),
            primary: "Persona".into(),
            secondary: "Agent name, template, and system prompt".into(),
        });

        // Channels — always shown; its sub-picker enumerates the channel
        // types that are wired in-TUI (currently whatsapp-web behind the
        // `whatsapp-web` feature; otherwise empty + a friendly hint).
        items.push(ListPickerItem {
            key: "channels".into(),
            primary: "Channels".into(),
            secondary: "Telegram, Discord, Slack, WhatsApp, …".into(),
        });

        let picker = ListPicker::new(
            ListPickerKind::SetupTopic,
            "Select setup topic",
            items,
            None,
            "no setup topics available",
        );
        Ok(CommandResult::OpenListPicker(picker))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui/commands/setup.rs tests/tui_setup_overlay.rs
git commit -m "feat(tui): /setup opens SetupTopic picker (categories only; whatsapp-web moved into Channels)"
```

---

## Task 5: Channels sub-picker

The Channels sub-picker lists every channel type that has an in-TUI provisioner. Phase 1 wires only `whatsapp-web`. Other channels (telegram, discord, etc.) do **not** appear until their per-channel ports land in follow-up plans.

**Files:**
- Modify: `src/tui/app.rs` — implement `open_channel_sub_picker` + `dispatch_setup_channel_key`.
- Test: `tests/tui_setup_picker.rs` (new file).

- [ ] **Step 1: Write the failing tests**

`tests/tui_setup_picker.rs`:

```rust
use rantaiclaw::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

#[test]
fn dispatch_setup_topic_routes_categories() {
    use rantaiclaw::tui::app::{dispatch_setup_topic_key, SetupTopicAction};
    assert!(matches!(
        dispatch_setup_topic_key("persona"),
        SetupTopicAction::TuiProvisioner(s) if s == "persona"
    ));
    assert!(matches!(
        dispatch_setup_topic_key("channels"),
        SetupTopicAction::OpenChannelSubPicker
    ));
    assert!(matches!(
        dispatch_setup_topic_key("nope"),
        SetupTopicAction::Unknown
    ));
}

#[cfg(feature = "whatsapp-web")]
#[test]
fn dispatch_setup_channel_routes_whatsapp_web_to_provisioner() {
    use rantaiclaw::tui::app::{dispatch_setup_channel_key, SetupChannelAction};
    assert!(matches!(
        dispatch_setup_channel_key("whatsapp-web"),
        SetupChannelAction::TuiProvisioner(s) if s == "whatsapp-web"
    ));
}

#[test]
fn dispatch_setup_channel_unknown_for_unported() {
    use rantaiclaw::tui::app::{dispatch_setup_channel_key, SetupChannelAction};
    // Telegram has no TuiProvisioner yet — must return Unknown so the UI
    // can show a clean "not yet available in-TUI" message instead of
    // silently doing nothing.
    assert!(matches!(
        dispatch_setup_channel_key("telegram"),
        SetupChannelAction::Unknown
    ));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rantaiclaw --test tui_setup_picker --features whatsapp-web`
Expected: FAIL.

- [ ] **Step 3: Implement dispatchers + sub-picker**

Add to `src/tui/app.rs`:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum SetupTopicAction {
    TuiProvisioner(String),
    OpenChannelSubPicker,
    Unknown,
}

pub fn dispatch_setup_topic_key(key: &str) -> SetupTopicAction {
    if key == "channels" { return SetupTopicAction::OpenChannelSubPicker; }
    if crate::onboard::provision::provisioner_for(key).is_some() {
        return SetupTopicAction::TuiProvisioner(key.to_string());
    }
    SetupTopicAction::Unknown
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupChannelAction {
    TuiProvisioner(String),
    Unknown,
}

pub fn dispatch_setup_channel_key(key: &str) -> SetupChannelAction {
    if crate::onboard::provision::provisioner_for(key).is_some() {
        return SetupChannelAction::TuiProvisioner(key.to_string());
    }
    SetupChannelAction::Unknown
}

impl TuiApp {
    fn open_channel_sub_picker(&mut self) {
        use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};
        // Only channels with a TuiProvisioner appear. As more channels
        // are ported in follow-up plans, list entries are added here.
        let items: Vec<ListPickerItem> = {
            let mut v = Vec::new();
            #[cfg(feature = "whatsapp-web")]
            v.push(ListPickerItem {
                key: "whatsapp-web".into(),
                primary: "WhatsApp Web".into(),
                secondary: "Pair via Linked Devices QR — runs in this overlay".into(),
            });
            v
        };
        let picker = ListPicker::new(
            ListPickerKind::SetupChannel,
            "Select channel type",
            items,
            None,
            "no channel provisioners available — rebuild with --features whatsapp-web for WhatsApp Web",
        );
        self.list_picker = Some(picker);
    }
}
```

Add the two new arms to `handle_list_picker_enter` (around `src/tui/app.rs:796`):

```rust
ListPickerKind::SetupTopic => {
    let key = picker.current().map(|i| i.key.clone()).unwrap_or_default();
    self.list_picker = None;
    match dispatch_setup_topic_key(&key) {
        SetupTopicAction::TuiProvisioner(name) => {
            if let Err(e) = self.open_setup_overlay(Some(name.clone())) {
                let msg = format!("Failed to open {name} setup: {e}");
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".into(), msg));
            }
        }
        SetupTopicAction::OpenChannelSubPicker => self.open_channel_sub_picker(),
        SetupTopicAction::Unknown => {
            let msg = format!("Unknown setup topic: {key}");
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".into(), msg));
        }
    }
}
ListPickerKind::SetupChannel => {
    let key = picker.current().map(|i| i.key.clone()).unwrap_or_default();
    self.list_picker = None;
    match dispatch_setup_channel_key(&key) {
        SetupChannelAction::TuiProvisioner(name) => {
            if let Err(e) = self.open_setup_overlay(Some(name.clone())) {
                let msg = format!("Failed to open {name} setup: {e}");
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".into(), msg));
            }
        }
        SetupChannelAction::Unknown => {
            let msg = format!("Channel {key} is not yet available in-TUI. Run `rantaiclaw setup channels --non-interactive` from a shell to use the legacy CLI flow.");
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".into(), msg));
        }
    }
}
```

The `Unknown` arm is unreachable in Phase 1 (the sub-picker only lists ported channels) but kept for defense-in-depth and future expansion.

- [ ] **Step 4: Run tests + smoke build**

Run: `cargo test -p rantaiclaw --test tui_setup_picker --features whatsapp-web && cargo build -p rantaiclaw --features whatsapp-web`
Expected: PASS, build clean.

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs tests/tui_setup_picker.rs
git commit -m "feat(tui): channels sub-picker; whatsapp-web grouped under Channels (not top level)"
```

---

## Task 6: Smoke + delete dead menu-text branch

The `open_setup_overlay(None)` system-message branch (added in the last session) is now unreachable — `SetupCommand` returns a picker for the no-arg case and the picker dispatch always passes `Some(name)`. Delete it.

**Files:**
- Modify: `src/tui/app.rs`

- [ ] **Step 1: Confirm unreachability**

```bash
grep -n "Available setup topics" src/tui/app.rs
```

Verify only one match in the dead branch and trace callers.

- [ ] **Step 2: Simplify `open_setup_overlay`**

Change signature from `provisioner: Option<String>` → `name: String`. Remove the `None` branch entirely. Update the `OpenSetupOverlay` arm and the `run_tui` startup call to pass the name directly. If the `CommandResult::OpenSetupOverlay { provisioner: Option<String> }` shape can be tightened to non-optional, do so for symmetry.

- [ ] **Step 3: Manual smoke**

```bash
cargo build -p rantaiclaw --features whatsapp-web
./target/debug/rantaiclaw
```

In TUI:
1. `/setup` → picker appears with **only**: Persona, Channels.
2. Type `pers` → filters to Persona. Enter → opens overlay with the persona prompts (agent name, template).
3. Esc out, `/setup` again → "Channels" → sub-picker appears with whatsapp-web only. Enter → opens overlay → starts WhatsApp Web pairing.
4. `/setup persona` → skips picker, jumps to overlay.
5. `/setup whatsapp-web` → skips picker, jumps to overlay (still works because it's a registered provisioner).
6. `/setup provider` → emits "Unknown setup topic: provider" message (no overlay opens; honest signal that the section isn't ported yet).

- [ ] **Step 4: Full test + clippy gate**

Run:
```bash
cargo test -p rantaiclaw --features whatsapp-web
cargo clippy -p rantaiclaw --all-targets -- -D warnings
```

Expected: tests green; no NEW clippy warnings introduced by these tasks (pre-existing warnings unrelated to this plan are acceptable but should be tracked).

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs src/tui/commands/mod.rs
git commit -m "refactor(tui): collapse open_setup_overlay to required name; drop dead menu-text branch"
```

---

## Task 7: Docs + Phase 2 spec

**Files:**
- Modify: `docs/commands-reference.md`
- Create: `docs/superpowers/specs/2026-05-03-tui-setup-phase2-port-roadmap.md`

- [ ] **Step 1: Update `/setup` docs**

In `docs/commands-reference.md`, the `/setup` entry should describe:
- Interactive picker (search + Up/Down + Enter + Esc).
- Top-level shows **categories** (Persona, Channels — and more as sections are ported in follow-up plans).
- Channels sub-picker lists channel types with in-TUI support (whatsapp-web behind `--features whatsapp-web`).
- `/setup <name>` shortcut for direct entry.
- Sections without an in-TUI provisioner can still be configured via `rantaiclaw setup <name> --non-interactive` from a shell — explicit fallback, not in-TUI silent suspend.

- [ ] **Step 2: Write the Phase 2 spec**

`docs/superpowers/specs/2026-05-03-tui-setup-phase2-port-roadmap.md`:

```markdown
# TUI setup port roadmap (Phase 2+)

Phase 1 (`docs/superpowers/plans/2026-05-03-tui-setup-interactive-picker.md`)
ships the picker UX, the `Choose` event UI, and the `persona` port.
Each remaining `SetupSection` becomes its own follow-up plan. The
canonical order from `src/onboard/wizard.rs::run_setup` is:

1. **provider** — text + Choose for provider selection, secret Prompt
   for API key, Choose for default model. Validation: existing
   provider-ping helper. Estimated effort: M.
2. **approvals** — Choose for L1/L2/L3/L4 tier; preset TOML write.
   Estimated effort: S.
3. **skills** — MultiSelect Choose over bundled + ClawHub skill list,
   text Prompt for new clawhub URL. Depends on the audit §7 install_one
   rewrite. Estimated effort: M.
4. **mcp** — Choose between curated/custom; for custom, text Prompts
   for server URL/auth. Validate via existing validate_mcp_startup.
   Estimated effort: M.
5. **channels** — per-channel TuiProvisioner. Each channel type
   (telegram, discord, slack, signal, matrix, mattermost, imessage,
   lark, dingtalk, nextcloud-talk, qq, email, irc, linq, whatsapp-cloud)
   is its own provisioner + its own plan. whatsapp-web is already done
   in Phase 1.

Each plan should:
- Read the corresponding `src/onboard/section/<name>.rs` to mirror
  prompt sequence + validation rules.
- Add a `<Section>Provisioner` impl + register in the registry.
- Update the SetupTopic picker only if it's a category-level section
  (the picker auto-grows from the registry today, so most ports are
  zero-touch on the picker code).
- Add `tests/provision_<section>.rs` with a fake-driver event-flow test.
- Keep the legacy SetupSection impl intact for `--non-interactive`.

Constraint: do **not** delete the legacy SetupSection impls during
phase 2 — the headless CLI path still uses them. Removal happens in a
final Phase 3 plan once every section has a TuiProvisioner peer AND the
headless driver has been migrated to walk provisioners through the
non-interactive `HeadlessFlags` path.
```

- [ ] **Step 3: Commit**

```bash
git add docs/
git commit -m "docs: /setup picker reference + Phase 2 port roadmap spec"
```

---

## Self-Review Notes

- **Spec coverage:** User asked for (1) interactive search/scroll picker for `/setup` — Tasks 1, 4. (2) "select what you want to setup" header — Task 4 picker title `"Select setup topic"`. (3) Per-section entries that themselves have sub-options when relevant — Task 5 (Channels). (4) Parity with legacy CLI — Task 3 ports persona event-for-event from `src/onboard/section/persona.rs`; subsequent plans port the rest. (5) NO context switch — every prompt is a `Prompt` or `Choose` event rendered inside the overlay. The `dialoguer` suspend is gone.

- **Boundary fix:** WhatsApp Web is correctly grouped under Channels (Task 5) — Task 4's test explicitly asserts it does NOT appear at top level. The categorization error from the prior plan is now a regression test.

- **No placeholders:** All steps show concrete code or commands. Task 3 step 1 reads the actual existing dialoguer flow before writing code, so the prompt sequence in step 3 mirrors it instead of being invented.

- **Type consistency:** `ActiveChoose`, `dispatch_setup_topic_key`, `dispatch_setup_channel_key`, `SetupTopicAction`, `SetupChannelAction`, `PersonaProvisioner`, `PERSONA_NAME`, `PERSONA_DESC`, `ListPickerKind::{SetupTopic, SetupChannel}` — all defined in single tasks and reused with consistent shapes.

- **Risk:** Task 2's key-ordering inside the `app.rs` match is the most error-prone part. Choose-active arms (Up/Down/Space/Enter) must come before the prompt-active arms because both can have `setup_overlay.is_some()`. Verify by inspection — write a key-routing unit test if uncertain.

- **Validation matrix (per CLAUDE.md §8):** Each task ends with `cargo test`. Task 6 adds the full clippy gate. Manual smoke in Task 6 step 3 covers all six user-visible flows.

- **Honesty principle:** Phase 1 picker shows ONLY ported sections (persona + channels with whatsapp-web inside). No "coming soon" stubs, no silent fallbacks, no dialoguer suspend. Users wanting unported sections see an "Unknown setup topic" message with an explicit pointer to the headless CLI flag — that's a deliberate informed choice, not a hidden context switch.
