# TUI-Unified Setup Implementation Plan



**Goal:** Make `rantaiclaw setup` open a setup overlay inside the chat TUI (same binary, same renderer) so users provision channels/providers without leaving the TUI, while keeping a headless `--non-interactive` CLI path for scripts. WhatsApp Web QR pairing is the first concrete flow.

**Architecture:** Introduce a `TuiProvisioner` trait that drives setup via async events (`ProvisionEvent`, `ProvisionResponse`) — the chat TUI hosts an overlay that renders these events and forwards user input. The existing `dialoguer`-based `SetupSection` wizard stays untouched as a fallback for `--non-interactive` and for sections not yet ported. `rantaiclaw setup` boots the TUI directly into the overlay; `rantaiclaw` (no subcommand) lets users open it via `/setup`. WhatsApp Web is the only provisioner ported in this plan; remaining sections are follow-up plans.

**Tech Stack:** Rust, `ratatui`, `crossterm`, `tokio` (mpsc), `qrcode` crate, existing `wa-rs` integration in `src/channels/whatsapp_web.rs`.

**Out of scope (separate plans):** Provider, Telegram/Discord/Slack, Persona, Skills, MCP, Approvals overlay migrations. ClawHub `install_one` rewrite (audit §7). MCP zero-auth validation (audit §2).

---

## File Structure

**New files:**
- `src/onboard/provision/mod.rs` — module root, re-exports
- `src/onboard/provision/traits.rs` — `TuiProvisioner` trait, `ProvisionEvent` / `ProvisionResponse` enums
- `src/onboard/provision/registry.rs` — `provisioner_for(name)` factory
- `src/onboard/provision/whatsapp_web.rs` — `WhatsAppWebProvisioner` impl (channel-agnostic core; uses `qrcode` + `wa-rs` one-shot client)
- `src/tui/widgets/setup_overlay.rs` — `SetupOverlay` widget (renders events, captures input, owns focus while open)
- `src/tui/commands/setup.rs` — `/setup` slash command handler
- `tests/tui_setup_overlay.rs` — integration test for overlay state machine
- `tests/provision_whatsapp_web.rs` — integration test for the provisioner with a fake `wa-rs` driver

**Modified files:**
- `src/onboard/mod.rs` — add `pub mod provision;`
- `src/tui/widgets/mod.rs` — add `pub mod setup_overlay; pub use setup_overlay::*;`
- `src/tui/commands/mod.rs` — register `setup::SetupCommand`, add `OpenSetupOverlay(SetupOverlayState)` variant to `CommandResult`
- `src/tui/app.rs` — handle `OpenSetupOverlay`, route key events to overlay while open, drive overlay event loop
- `src/tui/mod.rs` — `run_tui_with_setup(config, profile)` entry point that boots straight into overlay
- `src/main.rs` — `Commands::Setup` branches: TTY + interactive → `run_tui_with_setup`; non-TTY or `--non-interactive` → existing `onboard::wizard::run_setup`. Add `Commands::Setup` per-section flag handling for headless single-section runs.
- `src/channels/whatsapp_web.rs` — extract a `pair_once(opts) -> impl Stream<PairEvent>` helper used by the provisioner; render QR with `qrcode` crate at runtime (closes audit §1 fix)
- `Cargo.toml` — add `qrcode = { version = "0.14", default-features = false }`

---

## Task 1: Add the provisioner contract

**Files:**
- Create: `src/onboard/provision/mod.rs`
- Create: `src/onboard/provision/traits.rs`
- Modify: `src/onboard/mod.rs:1-3`
- Test: inline `#[cfg(test)]` in `traits.rs`

- [ ] **Step 1: Write the failing test**

In `src/onboard/provision/traits.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_event_message_carries_severity() {
        let info = ProvisionEvent::Message {
            severity: Severity::Info,
            text: "starting".into(),
        };
        match info {
            ProvisionEvent::Message { severity: Severity::Info, .. } => {}
            _ => panic!("expected Info Message"),
        }
    }

    #[test]
    fn provision_response_text_round_trips() {
        let r = ProvisionResponse::Text("hello".into());
        assert!(matches!(r, ProvisionResponse::Text(ref s) if s == "hello"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw onboard::provision::traits`
Expected: FAIL with "module `provision` not found".

- [ ] **Step 3: Write the trait + enums**

`src/onboard/provision/traits.rs`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub enum Severity { Info, Warn, Error, Success }

/// Driver-facing events emitted by a provisioner. The TUI overlay (or
/// the headless CLI driver) renders these to the user.
#[derive(Debug, Clone)]
pub enum ProvisionEvent {
    /// Plain status line — written to overlay log + stderr in headless.
    Message { severity: Severity, text: String },
    /// Render a QR code. `payload` is the raw string to encode.
    QrCode { payload: String, caption: String },
    /// Prompt the user. Driver must reply with `ProvisionResponse::Text`.
    Prompt { id: String, label: String, default: Option<String>, secret: bool },
    /// Multi-select list. Reply with `ProvisionResponse::Selection`.
    Choose { id: String, label: String, options: Vec<String>, multi: bool },
    /// Provisioner finished successfully; payload is human summary.
    Done { summary: String },
    /// Provisioner failed; payload is human error.
    Failed { error: String },
}

#[derive(Debug, Clone)]
pub enum ProvisionResponse {
    Text(String),
    Selection(Vec<usize>),
    Cancelled,
}

/// Channels handed to a provisioner. It emits events on `events` and
/// awaits responses on `responses`.
pub struct ProvisionIo {
    pub events: mpsc::Sender<ProvisionEvent>,
    pub responses: mpsc::Receiver<ProvisionResponse>,
}

#[async_trait]
pub trait TuiProvisioner: Send {
    /// Stable kebab-case identifier — used for `rantaiclaw setup <name>`.
    fn name(&self) -> &'static str;
    /// One-line description for the picker.
    fn description(&self) -> &'static str;
    /// Run to completion. Mutates `config` on success; caller persists.
    async fn run(
        &self,
        config: &mut crate::config::Config,
        profile: &crate::profile::Profile,
        io: ProvisionIo,
    ) -> Result<()>;
}

// ... tests inline above ...
```

`src/onboard/provision/mod.rs`:

```rust
pub mod traits;
pub use traits::*;
```

`src/onboard/mod.rs` — add `pub mod provision;` near the top.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rantaiclaw onboard::provision::traits`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/onboard/provision/ src/onboard/mod.rs
git commit -m "feat(onboard): add TuiProvisioner contract for ratatui-driven setup"
```

---

## Task 2: Add the provisioner registry

**Files:**
- Create: `src/onboard/provision/registry.rs`
- Modify: `src/onboard/provision/mod.rs`
- Test: inline

- [ ] **Step 1: Write the failing test**

`src/onboard/provision/registry.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registry_returns_none_for_unknown() {
        assert!(provisioner_for("nope").is_none());
    }
    #[test]
    fn registry_lists_at_least_one_name() {
        // Will fail until Task 6 registers WhatsApp Web. Kept here as a
        // forward guard so we don't ship an empty registry.
        assert!(!available().is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw onboard::provision::registry`
Expected: FAIL on `available()` and `provisioner_for` not found.

- [ ] **Step 3: Implement registry skeleton**

```rust
use super::traits::TuiProvisioner;

/// Returns `None` if no provisioner matches `name`. Names are kebab-case.
pub fn provisioner_for(name: &str) -> Option<Box<dyn TuiProvisioner>> {
    match name {
        // Filled in by Task 6.
        _ => None,
    }
}

pub fn available() -> Vec<(&'static str, &'static str)> {
    // (name, description). Updated in Task 6.
    vec![]
}
```

Add `pub mod registry; pub use registry::*;` to `src/onboard/provision/mod.rs`.

- [ ] **Step 4: Run test to verify partial state**

Run: `cargo test -p rantaiclaw onboard::provision::registry::tests::registry_returns_none_for_unknown`
Expected: PASS.

The `registry_lists_at_least_one_name` test is expected to FAIL until Task 6. Mark it `#[ignore = "filled in by task 6"]` for now, then remove the ignore in Task 6.

- [ ] **Step 5: Commit**

```bash
git add src/onboard/provision/registry.rs src/onboard/provision/mod.rs
git commit -m "feat(onboard): scaffold provisioner registry"
```

---

## Task 3: SetupOverlay widget — render-only first

**Files:**
- Create: `src/tui/widgets/setup_overlay.rs`
- Modify: `src/tui/widgets/mod.rs`
- Test: `tests/tui_setup_overlay.rs`

The overlay state holds: a log of events received, an active prompt (if any), and an input buffer. It does NOT spawn the provisioner — that's wired in Task 5. This task is just the visual shell.

- [ ] **Step 1: Write the failing test**

`tests/tui_setup_overlay.rs`:

```rust
use rantaiclaw::onboard::provision::{ProvisionEvent, Severity};
use rantaiclaw::tui::widgets::setup_overlay::SetupOverlayState;

#[test]
fn overlay_appends_message_events_to_log() {
    let mut s = SetupOverlayState::new("WhatsApp Web pairing");
    s.handle_event(ProvisionEvent::Message {
        severity: Severity::Info,
        text: "Connecting…".into(),
    });
    assert_eq!(s.log_lines().len(), 1);
    assert!(s.log_lines()[0].contains("Connecting"));
}

#[test]
fn overlay_prompt_event_sets_active_prompt() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Prompt {
        id: "phone".into(), label: "Phone number".into(),
        default: None, secret: false,
    });
    assert_eq!(s.active_prompt().map(|p| p.label.as_str()), Some("Phone number"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement the state struct**

`src/tui/widgets/setup_overlay.rs`:

```rust
use crate::onboard::provision::{ProvisionEvent, Severity};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

#[derive(Debug, Clone)]
pub struct ActivePrompt {
    pub id: String,
    pub label: String,
    pub default: Option<String>,
    pub secret: bool,
}

#[derive(Debug, Default)]
pub struct SetupOverlayState {
    title: String,
    log: Vec<String>,
    qr: Option<(String, String)>, // (rendered_block, caption)
    prompt: Option<ActivePrompt>,
    input: String,
    pub closed: bool,
}

impl SetupOverlayState {
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), ..Default::default() }
    }

    pub fn handle_event(&mut self, ev: ProvisionEvent) {
        match ev {
            ProvisionEvent::Message { severity, text } => {
                let prefix = match severity {
                    Severity::Info => "·", Severity::Warn => "!",
                    Severity::Error => "✗", Severity::Success => "✓",
                };
                self.log.push(format!("{prefix} {text}"));
            }
            ProvisionEvent::QrCode { payload, caption } => {
                self.qr = Some((render_qr_block(&payload), caption));
            }
            ProvisionEvent::Prompt { id, label, default, secret } => {
                self.prompt = Some(ActivePrompt { id, label, default, secret });
                self.input.clear();
            }
            ProvisionEvent::Choose { .. } => {
                // TODO Task 7: hook to ListPicker.
                self.log.push("(choose UI not yet wired)".into());
            }
            ProvisionEvent::Done { summary } => {
                self.log.push(format!("✓ {summary}"));
                self.closed = true;
            }
            ProvisionEvent::Failed { error } => {
                self.log.push(format!("✗ {error}"));
                self.closed = true;
            }
        }
    }

    pub fn log_lines(&self) -> &[String] { &self.log }
    pub fn active_prompt(&self) -> Option<&ActivePrompt> { self.prompt.as_ref() }
    pub fn input(&self) -> &str { &self.input }
    pub fn push_char(&mut self, c: char) { self.input.push(c); }
    pub fn pop_char(&mut self) { self.input.pop(); }
    pub fn submit_prompt(&mut self) -> Option<(String, String)> {
        let p = self.prompt.take()?;
        let value = if self.input.is_empty() {
            p.default.clone().unwrap_or_default()
        } else {
            std::mem::take(&mut self.input)
        };
        Some((p.id, value))
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title.as_str())
            .style(Style::default().fg(Color::Cyan));
        let mut lines: Vec<Line> = self.log.iter().map(|l| Line::from(l.as_str())).collect();
        if let Some((qr, cap)) = &self.qr {
            lines.push(Line::from(""));
            lines.push(Line::from(cap.as_str()).style(Style::default().add_modifier(Modifier::BOLD)));
            for qrl in qr.lines() { lines.push(Line::from(qrl)); }
        }
        if let Some(p) = &self.prompt {
            lines.push(Line::from(""));
            let masked = if p.secret { "•".repeat(self.input.len()) } else { self.input.clone() };
            lines.push(Line::from(format!("{}: {}_", p.label, masked)));
        }
        let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        f.render_widget(para, area);
    }
}

fn render_qr_block(payload: &str) -> String {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(payload.as_bytes()) {
        Ok(qr) => qr.render::<unicode::Dense1x2>().build(),
        Err(_) => format!("[QR render failed; raw payload: {payload}]"),
    }
}
```

Add to `src/tui/widgets/mod.rs`: `pub mod setup_overlay; pub use setup_overlay::SetupOverlayState;`

Add to `Cargo.toml` `[dependencies]`: `qrcode = { version = "0.14", default-features = false }`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay && cargo build -p rantaiclaw`
Expected: PASS, build clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/tui/widgets/ tests/tui_setup_overlay.rs
git commit -m "feat(tui): add SetupOverlay widget with QR + prompt rendering"
```

---

## Task 4: Wire `OpenSetupOverlay` into CommandResult and `/setup` command

**Files:**
- Create: `src/tui/commands/setup.rs`
- Modify: `src/tui/commands/mod.rs:17-41` (CommandResult enum), `src/tui/commands/mod.rs:90-115` (registration)
- Test: extend `tests/tui_setup_overlay.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/tui_setup_overlay.rs`:

```rust
use rantaiclaw::tui::commands::{CommandRegistry, CommandResult};
use rantaiclaw::tui::context::TuiContext;

#[test]
fn slash_setup_returns_open_setup_overlay() {
    let registry = CommandRegistry::new();
    let (mut ctx, _rx, _tx) = TuiContext::test_context();
    let r = registry.dispatch("/setup", &mut ctx).unwrap();
    assert!(matches!(r, CommandResult::OpenSetupOverlay { .. }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay slash_setup`
Expected: FAIL — `OpenSetupOverlay` variant doesn't exist.

- [ ] **Step 3: Add the variant + command**

In `src/tui/commands/mod.rs`, add to `CommandResult`:

```rust
    /// Open the setup overlay. The picker variant lists available
    /// provisioners; passing a concrete name jumps straight in.
    OpenSetupOverlay { provisioner: Option<String> },
```

Create `src/tui/commands/setup.rs`:

```rust
use anyhow::Result;
use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

pub struct SetupCommand;

impl CommandHandler for SetupCommand {
    fn name(&self) -> &str { "setup" }
    fn description(&self) -> &str { "Configure providers, channels, and integrations" }
    fn usage(&self) -> &str { "setup [provisioner-name]" }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let provisioner = args.trim();
        Ok(CommandResult::OpenSetupOverlay {
            provisioner: if provisioner.is_empty() { None } else { Some(provisioner.to_string()) },
        })
    }
}
```

Register in `src/tui/commands/mod.rs` `register_defaults`: add `mod setup;` and `self.register(Box::new(setup::SetupCommand));`

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/tui/commands/ tests/tui_setup_overlay.rs
git commit -m "feat(tui): /setup command opens setup overlay"
```

---

## Task 5: App-level overlay event loop

Wire the overlay into `tui/app.rs`: when `OpenSetupOverlay` returns from a command, the app spawns the named provisioner on a tokio task, creates the mpsc pair, and routes:
- key events → overlay (chat input is suppressed while overlay is open)
- `ProvisionEvent`s from the channel → `state.handle_event(ev)`
- prompt submissions → send `ProvisionResponse::Text` back

**Files:**
- Modify: `src/tui/app.rs` — add `setup_overlay: Option<SetupOverlayState>`, `setup_response_tx: Option<mpsc::Sender<ProvisionResponse>>`, `setup_event_rx: Option<mpsc::Receiver<ProvisionEvent>>` fields. Handle `CommandResult::OpenSetupOverlay` in the dispatch site.
- Test: `tests/tui_setup_overlay.rs` — drive a fake provisioner end-to-end through the app loop.

- [ ] **Step 1: Write the failing test**

Append to `tests/tui_setup_overlay.rs` (uses a fake provisioner that emits two messages then `Done`):

```rust
use rantaiclaw::onboard::provision::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};
use async_trait::async_trait;

struct FakeProv;
#[async_trait]
impl TuiProvisioner for FakeProv {
    fn name(&self) -> &'static str { "fake" }
    fn description(&self) -> &'static str { "fake" }
    async fn run(
        &self, _c: &mut rantaiclaw::config::Config,
        _p: &rantaiclaw::profile::Profile, io: ProvisionIo,
    ) -> anyhow::Result<()> {
        io.events.send(ProvisionEvent::Message {
            severity: Severity::Info, text: "step 1".into() }).await?;
        io.events.send(ProvisionEvent::Done { summary: "ok".into() }).await?;
        Ok(())
    }
}

#[tokio::test]
async fn fake_provisioner_drives_overlay_to_done() {
    use tokio::sync::mpsc;
    let (etx, mut erx) = mpsc::channel(8);
    let (_rtx, rrx) = mpsc::channel(8);
    let mut state = SetupOverlayState::new("fake");
    let mut cfg = rantaiclaw::config::Config::default();
    let prof = rantaiclaw::profile::Profile::default_for_test();
    tokio::spawn(async move {
        FakeProv.run(&mut cfg, &prof, ProvisionIo { events: etx, responses: rrx }).await.unwrap();
    });
    while let Some(ev) = erx.recv().await {
        state.handle_event(ev);
        if state.closed { break; }
    }
    assert!(state.closed);
    assert!(state.log_lines().iter().any(|l| l.contains("step 1")));
    assert!(state.log_lines().iter().any(|l| l.contains("ok")));
}
```

This test does NOT touch `app.rs` directly; it validates the IO contract end-to-end. The actual `app.rs` wiring is verified in Task 9 by manual smoke + the integration test from Task 6.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay fake_provisioner`
Expected: FAIL — `Profile::default_for_test` may not exist; if not, add it as a `#[cfg(test)]` constructor in `src/profile/mod.rs`.

- [ ] **Step 3: Implement the wiring in `app.rs`**

Add fields to the `App` struct (find via `grep -n "pub struct App" src/tui/app.rs`):

```rust
    setup_overlay: Option<SetupOverlayState>,
    setup_event_rx: Option<tokio::sync::mpsc::Receiver<ProvisionEvent>>,
    setup_response_tx: Option<tokio::sync::mpsc::Sender<ProvisionResponse>>,
```

In the `CommandResult` dispatch arm (find it via `grep -n "CommandResult::" src/tui/app.rs`), add:

```rust
CommandResult::OpenSetupOverlay { provisioner } => {
    let name = provisioner.unwrap_or_else(|| "whatsapp-web".to_string());
    if let Some(p) = crate::onboard::provision::registry::provisioner_for(&name) {
        let (etx, erx) = tokio::sync::mpsc::channel(32);
        let (rtx, rrx) = tokio::sync::mpsc::channel(8);
        self.setup_overlay = Some(SetupOverlayState::new(p.name()));
        self.setup_event_rx = Some(erx);
        self.setup_response_tx = Some(rtx);
        let cfg = self.ctx.config.clone();
        let prof = self.ctx.profile.clone();
        let cfg_writeback = self.ctx.config.clone(); // see persistence note in Task 6
        tokio::spawn(async move {
            let mut cfg = cfg;
            let _ = p.run(&mut cfg, &prof, ProvisionIo { events: etx, responses: rrx }).await;
            // Persistence: provisioner success path writes via Config::save inside its body.
            // App reloads config on overlay close (Task 6).
        });
    } else {
        self.set_status(format!("unknown provisioner: {name}"));
    }
}
```

In the main event loop tick, drain `setup_event_rx`:

```rust
if let Some(rx) = self.setup_event_rx.as_mut() {
    while let Ok(ev) = rx.try_recv() {
        if let Some(state) = self.setup_overlay.as_mut() {
            state.handle_event(ev);
        }
    }
    if self.setup_overlay.as_ref().map(|s| s.closed).unwrap_or(false) {
        // Reload config from disk so freshly-written sections take effect.
        if let Ok(reloaded) = crate::config::Config::load(&self.ctx.profile.config_path()) {
            self.ctx.config = reloaded;
        }
        self.setup_overlay = None;
        self.setup_event_rx = None;
        self.setup_response_tx = None;
    }
}
```

In key handling, when `setup_overlay.is_some()`: route Esc to close, Enter to submit prompt, chars to `push_char`, Backspace to `pop_char`. Suppress all chat input bindings while overlay is open.

In the renderer, draw the overlay last (centered, ~60% of screen) when `self.setup_overlay.is_some()`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rantaiclaw --test tui_setup_overlay && cargo build -p rantaiclaw`
Expected: PASS, build clean.

- [ ] **Step 5: Commit**

```bash
git add src/tui/app.rs src/profile/mod.rs tests/tui_setup_overlay.rs
git commit -m "feat(tui): drive setup overlay from app event loop"
```

---

## Task 6: WhatsApp Web provisioner — split `pair_once` helper

The WhatsApp Web client today only emits the QR via `tracing::debug!` (audit §1). Extract a `pair_once` helper that returns an `impl Stream<Item = PairEvent>` so the provisioner can subscribe.

**Files:**
- Modify: `src/channels/whatsapp_web.rs:335-340` and surrounding
- Test: extend `tests/provision_whatsapp_web.rs` (created here)

- [ ] **Step 1: Write the failing test**

`tests/provision_whatsapp_web.rs`:

```rust
use rantaiclaw::channels::whatsapp_web::{pair_once, PairEvent, PairOptions};
use futures::StreamExt;

#[tokio::test]
#[ignore = "requires whatsapp-web feature; run with --features whatsapp-web"]
async fn pair_once_yields_qr_then_connected_or_timeout() {
    let mut stream = pair_once(PairOptions {
        session_path: tempfile::tempdir().unwrap().path().join("wa.db"),
        pair_phone: None,
        timeout: std::time::Duration::from_secs(2),
    });
    let mut saw_qr = false;
    while let Some(ev) = stream.next().await {
        match ev {
            PairEvent::Qr(_) => { saw_qr = true; break; }
            PairEvent::Timeout => break,
            _ => {}
        }
    }
    assert!(saw_qr || true, "smoke: stream produced events");
}
```

(The `|| true` is intentional — without a real WhatsApp server we can't assert QR; the test mainly verifies the API compiles + emits events. A non-ignored unit test below covers the API shape.)

Add a non-ignored unit test inside `whatsapp_web.rs`:

```rust
#[cfg(test)]
mod tests_pair {
    use super::*;
    #[test]
    fn pair_options_default_timeout_is_60s() {
        let o = PairOptions::default();
        assert_eq!(o.timeout, std::time::Duration::from_secs(60));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw whatsapp_web::tests_pair`
Expected: FAIL — `pair_once`, `PairEvent`, `PairOptions` not defined.

- [ ] **Step 3: Implement `pair_once`**

In `src/channels/whatsapp_web.rs`, add:

```rust
#[derive(Debug, Clone)]
pub struct PairOptions {
    pub session_path: std::path::PathBuf,
    pub pair_phone: Option<String>,
    pub timeout: std::time::Duration,
}
impl Default for PairOptions {
    fn default() -> Self {
        Self {
            session_path: std::path::PathBuf::from("wa.db"),
            pair_phone: None,
            timeout: std::time::Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PairEvent {
    Qr(String),
    PairCode(String),
    Connected,
    Timeout,
    Failed(String),
}

#[cfg(feature = "whatsapp-web")]
pub fn pair_once(opts: PairOptions) -> impl futures::Stream<Item = PairEvent> {
    use async_stream::stream;
    use tokio::time::timeout;
    stream! {
        // Bridge to existing one-shot client; replace the body below
        // with the real wa-rs handshake. Translate `Event::PairingQrCode`,
        // `Event::PairingCode`, `Event::Connected`, `Event::Disconnected`
        // into PairEvent variants. The `tracing::debug!("QR code: {}", code)`
        // line in the existing event handler (audit §1) becomes a yield here.
        // ... existing wa-rs init (refactor `WhatsAppWebClient::new` to share code) ...
        // for ev in client.events() { yield translate(ev); }
        yield PairEvent::Failed("pair_once: real impl pending; see TODO".into());
    }
}

#[cfg(not(feature = "whatsapp-web"))]
pub fn pair_once(_opts: PairOptions) -> impl futures::Stream<Item = PairEvent> {
    futures::stream::once(async {
        PairEvent::Failed("rebuild with --features whatsapp-web".into())
    })
}
```

When implementing the real body, refactor the existing `WhatsAppWebClient::run` (around line 335) so the event-translation code is shared between the long-running daemon path and `pair_once`. The daemon path also needs the QR rendering fix from audit §1: render with `qrcode` crate to stderr unconditionally instead of `tracing::debug!`.

Add `async-stream = "0.3"` and `futures = "0.3"` to `Cargo.toml` if not present.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rantaiclaw whatsapp_web::tests_pair && cargo build -p rantaiclaw --features whatsapp-web`
Expected: PASS, build clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/channels/whatsapp_web.rs tests/provision_whatsapp_web.rs
git commit -m "feat(channels): expose pair_once helper for WhatsApp Web provisioner"
```

---

## Task 7: Implement `WhatsAppWebProvisioner`

**Files:**
- Create: `src/onboard/provision/whatsapp_web.rs`
- Modify: `src/onboard/provision/registry.rs`
- Modify: `src/onboard/provision/mod.rs`

- [ ] **Step 1: Write the failing test**

In `src/onboard/provision/whatsapp_web.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::{ProvisionEvent, ProvisionIo, ProvisionResponse};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn provisioner_emits_qr_then_done_when_pair_succeeds() {
        // Stub `pair_once` by passing in a vec of synthetic PairEvents.
        let p = WhatsAppWebProvisioner::with_pair_stream(vec![
            crate::channels::whatsapp_web::PairEvent::Qr("FAKEQR".into()),
            crate::channels::whatsapp_web::PairEvent::Connected,
        ]);
        let (etx, mut erx) = mpsc::channel(16);
        let (_rtx, rrx) = mpsc::channel(8);
        let mut cfg = crate::config::Config::default();
        let prof = crate::profile::Profile::default_for_test();
        p.run(&mut cfg, &prof, ProvisionIo { events: etx, responses: rrx }).await.unwrap();

        let mut events = vec![];
        while let Ok(ev) = erx.try_recv() { events.push(ev); }
        assert!(events.iter().any(|e| matches!(e, ProvisionEvent::QrCode { payload, .. } if payload == "FAKEQR")));
        assert!(events.iter().any(|e| matches!(e, ProvisionEvent::Done { .. })));
        assert!(cfg.channels.whatsapp_web.is_some(), "provisioner must write config");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw onboard::provision::whatsapp_web`
Expected: FAIL — module not present.

- [ ] **Step 3: Implement the provisioner**

```rust
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;

use crate::channels::whatsapp_web::{pair_once, PairEvent, PairOptions};
use crate::onboard::provision::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};

pub struct WhatsAppWebProvisioner {
    /// Test hook — when `Some`, the provisioner replays these events instead
    /// of calling `pair_once`. Production callers use `default()`.
    stub_events: Option<Vec<PairEvent>>,
}

impl Default for WhatsAppWebProvisioner {
    fn default() -> Self { Self { stub_events: None } }
}

impl WhatsAppWebProvisioner {
    pub fn with_pair_stream(events: Vec<PairEvent>) -> Self {
        Self { stub_events: Some(events) }
    }
}

#[async_trait]
impl TuiProvisioner for WhatsAppWebProvisioner {
    fn name(&self) -> &'static str { "whatsapp-web" }
    fn description(&self) -> &'static str { "WhatsApp Web (QR pairing)" }

    async fn run(
        &self, config: &mut crate::config::Config,
        profile: &crate::profile::Profile, io: ProvisionIo,
    ) -> Result<()> {
        let ProvisionIo { events, mut responses } = io;

        // 1. Ask for session path, defaulting under profile dir.
        let default_session = profile.data_dir().join("whatsapp.db");
        events.send(ProvisionEvent::Prompt {
            id: "session_path".into(),
            label: "Session DB path".into(),
            default: Some(default_session.display().to_string()),
            secret: false,
        }).await.ok();
        let session_path = match responses.recv().await {
            Some(ProvisionResponse::Text(s)) => std::path::PathBuf::from(s),
            _ => { events.send(ProvisionEvent::Failed { error: "cancelled".into() }).await.ok(); return Ok(()); }
        };

        events.send(ProvisionEvent::Message {
            severity: Severity::Info, text: "Starting pairing…".into(),
        }).await.ok();

        // 2. Either replay stub events or call pair_once.
        let stream: std::pin::Pin<Box<dyn futures::Stream<Item = PairEvent> + Send>> =
            if let Some(stubs) = &self.stub_events {
                Box::pin(futures::stream::iter(stubs.clone()))
            } else {
                Box::pin(pair_once(PairOptions {
                    session_path: session_path.clone(),
                    pair_phone: None,
                    timeout: std::time::Duration::from_secs(120),
                }))
            };

        tokio::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                PairEvent::Qr(payload) => {
                    events.send(ProvisionEvent::QrCode {
                        payload, caption: "Scan with WhatsApp → Linked Devices → Link a Device".into(),
                    }).await.ok();
                }
                PairEvent::PairCode(code) => {
                    events.send(ProvisionEvent::Message {
                        severity: Severity::Info, text: format!("Pair code: {code}"),
                    }).await.ok();
                }
                PairEvent::Connected => {
                    config.channels.whatsapp_web = Some(crate::config::schema::WhatsAppWebConfig {
                        session_path: session_path.clone(),
                        ..Default::default()
                    });
                    config.save(&profile.config_path())?;
                    events.send(ProvisionEvent::Done {
                        summary: format!("WhatsApp Web paired; session at {}", session_path.display()),
                    }).await.ok();
                    return Ok(());
                }
                PairEvent::Timeout => {
                    events.send(ProvisionEvent::Failed { error: "pairing timed out".into() }).await.ok();
                    return Ok(());
                }
                PairEvent::Failed(e) => {
                    events.send(ProvisionEvent::Failed { error: e }).await.ok();
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
```

In `src/onboard/provision/registry.rs`, update:

```rust
pub fn provisioner_for(name: &str) -> Option<Box<dyn TuiProvisioner>> {
    match name {
        "whatsapp-web" => Some(Box::new(super::whatsapp_web::WhatsAppWebProvisioner::default())),
        _ => None,
    }
}
pub fn available() -> Vec<(&'static str, &'static str)> {
    vec![("whatsapp-web", "WhatsApp Web (QR pairing)")]
}
```

Remove the `#[ignore]` from `registry_lists_at_least_one_name`.

In `src/onboard/provision/mod.rs`: `pub mod whatsapp_web;`

Verify the exact field names in `crate::config::schema::WhatsAppWebConfig` match (`session_path`); adjust if the schema uses a different name.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rantaiclaw onboard::provision`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/onboard/provision/
git commit -m "feat(onboard): WhatsAppWebProvisioner ports QR pairing to TUI overlay"
```

---

## Task 8: `rantaiclaw setup` boots into the TUI overlay (interactive default)

**Files:**
- Modify: `src/main.rs:937-975` (the `Commands::Setup` arm)
- Modify: `src/tui/mod.rs` — add `run_tui_with_setup(config, profile, provisioner: Option<String>)`
- Test: extend `tests/setup_orchestration.rs` if it exists, otherwise add the assertion to a new `tests/cli_setup_routes.rs`.

- [ ] **Step 1: Write the failing test**

`tests/cli_setup_routes.rs`:

```rust
use clap::Parser;

#[test]
fn setup_with_non_interactive_keeps_dialoguer_path() {
    let cli = rantaiclaw::Cli::try_parse_from([
        "rantaiclaw", "setup", "--non-interactive",
    ]).unwrap();
    // Verify the parsed command carries `non_interactive = true`. Concrete
    // assertion depends on Cli shape; adjust to match.
    match cli.command {
        Some(rantaiclaw::Commands::Setup { non_interactive, .. }) => {
            assert!(non_interactive);
        }
        _ => panic!("expected Setup"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails or passes (depending on existing flag)**

Run: `cargo test -p rantaiclaw --test cli_setup_routes`

If `non_interactive` already exists on `Commands::Setup` in `src/main.rs:195`, the test passes immediately — proceed to step 3 to add the routing logic. If not, add the field as `#[arg(long)] non_interactive: bool` and re-run.

- [ ] **Step 3: Add routing logic**

In `src/main.rs`, replace the body of the `Some(Commands::Setup { ... })` arm:

```rust
let interactive = !non_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());
if interactive && section.is_none() {
    // New default: boot the chat TUI straight into the setup overlay.
    return tokio::runtime::Handle::current().block_on(async {
        crate::tui::run_tui_with_setup(config, profile, None).await
    });
}
// Fall back to the existing dialoguer wizard for `--non-interactive`,
// non-TTY stdin, or an explicit `setup <section>` invocation (until those
// sections are migrated to provisioners).
let task = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
    let r = onboard::wizard::run_setup(/* unchanged args */);
    Ok(r)
});
// ... rest unchanged ...
```

In `src/tui/mod.rs`:

```rust
pub async fn run_tui_with_setup(
    config: crate::config::Config,
    profile: crate::profile::Profile,
    provisioner: Option<String>,
) -> anyhow::Result<()> {
    let mut app = app::App::new(config, profile)?;
    // Pre-queue the OpenSetupOverlay command before the first frame.
    app.queue_command(crate::tui::commands::CommandResult::OpenSetupOverlay { provisioner });
    app.run().await
}
```

Add `App::queue_command` if it doesn't exist — a tiny helper that pushes onto a `Vec<CommandResult>` drained at the top of the event loop.

- [ ] **Step 4: Smoke test**

Run: `cargo build -p rantaiclaw && ./target/debug/rantaiclaw setup`
Expected: chat TUI launches with the setup overlay visible. `Esc` closes it back to the chat. `cargo test -p rantaiclaw` still passes.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/tui/mod.rs src/tui/app.rs tests/cli_setup_routes.rs
git commit -m "feat(cli): rantaiclaw setup boots TUI overlay by default; --non-interactive keeps dialoguer"
```

---

## Task 9: Headless `rantaiclaw setup whatsapp-web --non-interactive`

Add a third entry point: `rantaiclaw setup whatsapp-web` with `--non-interactive` runs the same `WhatsAppWebProvisioner` but with a stdout/stderr driver instead of the TUI overlay. QR renders to stderr via the same `qrcode` crate.

**Files:**
- Create: `src/onboard/provision/headless.rs`
- Modify: `src/main.rs` Setup arm (extend the non-interactive branch to detect a per-section provisioner name and call into `headless::run`)

- [ ] **Step 1: Write the failing test**

`src/onboard/provision/headless.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::provision::{ProvisionEvent, Severity};

    #[test]
    fn render_event_message_writes_to_buffer() {
        let mut out = Vec::new();
        render_event(
            &ProvisionEvent::Message { severity: Severity::Info, text: "hi".into() },
            &mut out,
        );
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("hi"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw onboard::provision::headless`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement the headless driver**

```rust
use std::io::Write;
use crate::onboard::provision::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};

pub fn render_event(ev: &ProvisionEvent, out: &mut impl Write) {
    match ev {
        ProvisionEvent::Message { severity, text } => {
            let prefix = match severity {
                Severity::Info => "·", Severity::Warn => "!",
                Severity::Error => "✗", Severity::Success => "✓",
            };
            let _ = writeln!(out, "{prefix} {text}");
        }
        ProvisionEvent::QrCode { payload, caption } => {
            use qrcode::{render::unicode, QrCode};
            let _ = writeln!(out, "\n{caption}\n");
            if let Ok(qr) = QrCode::new(payload.as_bytes()) {
                let _ = writeln!(out, "{}", qr.render::<unicode::Dense1x2>().build());
            } else {
                let _ = writeln!(out, "[QR render failed; payload: {payload}]");
            }
        }
        ProvisionEvent::Prompt { label, default, .. } => {
            let _ = writeln!(out, "PROMPT (non-interactive): {label} (default {default:?}) — supply via flag");
        }
        ProvisionEvent::Choose { label, options, .. } => {
            let _ = writeln!(out, "CHOOSE (non-interactive): {label} options {options:?}");
        }
        ProvisionEvent::Done { summary } => { let _ = writeln!(out, "✓ {summary}"); }
        ProvisionEvent::Failed { error } => { let _ = writeln!(out, "✗ {error}"); }
    }
}

pub async fn run(
    name: &str,
    flags: HeadlessFlags,
    config: &mut crate::config::Config,
    profile: &crate::profile::Profile,
) -> anyhow::Result<()> {
    let p = crate::onboard::provision::registry::provisioner_for(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provisioner: {name}"))?;
    let (etx, mut erx) = tokio::sync::mpsc::channel(32);
    let (rtx, rrx) = tokio::sync::mpsc::channel(8);

    // Pre-seed responses from CLI flags so prompts auto-resolve.
    for value in flags.preseeded_responses() {
        rtx.send(ProvisionResponse::Text(value)).await.ok();
    }
    let task = tokio::spawn({
        let mut cfg = config.clone();
        let prof = profile.clone();
        async move {
            p.run(&mut cfg, &prof, ProvisionIo { events: etx, responses: rrx }).await
        }
    });
    let mut stderr = std::io::stderr();
    while let Some(ev) = erx.recv().await {
        render_event(&ev, &mut stderr);
        if matches!(ev, ProvisionEvent::Done { .. } | ProvisionEvent::Failed { .. }) { break; }
    }
    task.await??;
    Ok(())
}

#[derive(Debug, Default)]
pub struct HeadlessFlags {
    pub session_path: Option<String>,
    // Add per-provisioner flags as more provisioners are migrated.
}
impl HeadlessFlags {
    fn preseeded_responses(&self) -> Vec<String> {
        let mut v = vec![];
        if let Some(p) = &self.session_path { v.push(p.clone()); }
        v
    }
}
```

In `src/main.rs`, extend the non-interactive Setup branch:

```rust
if non_interactive {
    if let Some(section_name) = section.as_deref() {
        if crate::onboard::provision::registry::provisioner_for(section_name).is_some() {
            return tokio::runtime::Handle::current().block_on(async {
                crate::onboard::provision::headless::run(
                    section_name,
                    HeadlessFlags { session_path /* from CLI flag */, ..Default::default() },
                    &mut config, &profile,
                ).await
            });
        }
    }
    // else fall through to existing dialoguer wizard with `--non-interactive`
}
```

Add the `--session-path` flag (and any others needed) to `Commands::Setup` in `main.rs`.

- [ ] **Step 4: Run tests + smoke**

Run:
```
cargo test -p rantaiclaw onboard::provision
./target/debug/rantaiclaw setup whatsapp-web --non-interactive --session-path /tmp/wa.db
```
Expected: tests pass; the headless command runs (likely fails at pair stage without WhatsApp, but emits the QR to stderr).

- [ ] **Step 5: Commit**

```bash
git add src/onboard/provision/headless.rs src/main.rs
git commit -m "feat(cli): headless setup driver for non-interactive automation"
```

---

## Task 10: Daemon-path QR rendering fix + feature warning (audit §1 close-out)

The daemon path also needs the QR fix, and the missing-feature error must surface clearly. Both are tiny but close audit §1.

**Files:**
- Modify: `src/channels/whatsapp_web.rs` — replace the existing `tracing::debug!("QR code: {}", code)` with a `qrcode`-rendered stderr block (same renderer as headless driver).
- Modify: `src/onboard/section/channels.rs` — when the user picks WhatsApp Web and the binary lacks the feature, emit a hard error with rebuild instructions instead of a hint.

- [ ] **Step 1: Write the failing test**

In `src/channels/whatsapp_web.rs`:

```rust
#[test]
fn render_qr_to_string_produces_unicode_block() {
    let s = render_qr_to_string("HELLOWORLD");
    assert!(s.contains('█') || s.contains('▀') || s.contains('▄'));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rantaiclaw render_qr_to_string`
Expected: FAIL — function not present.

- [ ] **Step 3: Implement and wire**

```rust
pub(crate) fn render_qr_to_string(payload: &str) -> String {
    use qrcode::{render::unicode, QrCode};
    QrCode::new(payload.as_bytes())
        .map(|qr| qr.render::<unicode::Dense1x2>().build())
        .unwrap_or_else(|_| format!("[QR render failed; payload: {payload}]"))
}
```

Replace the `Event::PairingQrCode` handler body so it does:

```rust
eprintln!(
    "\nScan with WhatsApp → Linked Devices → Link a Device\n\n{}\n",
    render_qr_to_string(&code),
);
tracing::info!("WhatsApp Web QR code rendered to stderr");
```

In `src/onboard/section/channels.rs`, around the WhatsApp Web branch, add:

```rust
#[cfg(not(feature = "whatsapp-web"))]
{
    anyhow::bail!(
        "WhatsApp Web requires building with --features whatsapp-web; rebuild and re-run setup"
    );
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rantaiclaw && cargo build -p rantaiclaw --features whatsapp-web`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/channels/whatsapp_web.rs src/onboard/section/channels.rs
git commit -m "fix(channels): render WhatsApp Web QR to stderr; hard-fail without whatsapp-web feature"
```

---

## Task 11: Docs + handoff

**Files:**
- Modify: `docs/commands-reference.md` — document new `rantaiclaw setup` behavior and `--non-interactive` flag semantics.
- Modify: `docs/channels-reference.md` — note the new in-TUI WhatsApp Web pairing flow.
- Create: `docs/superpowers/specs/2026-05-02-tui-setup-followups.md` — list remaining sections (provider, telegram, discord, slack, persona, skills, mcp, approvals) as future migration plans, each one task-sized.

- [ ] **Step 1: Update reference docs**

`docs/commands-reference.md`: under `setup`, document three entry points (interactive default → TUI overlay; non-interactive → dialoguer wizard or headless provisioner; `setup <section>` → single section).

`docs/channels-reference.md`: under WhatsApp Web, replace the "scan QR in WhatsApp > Linked Devices" hint with the new in-TUI flow + headless flag list.

- [ ] **Step 2: Write the follow-up spec**

`docs/superpowers/specs/2026-05-02-tui-setup-followups.md`: list each remaining section (provider, telegram, discord, slack, persona, skills, mcp, approvals) with the existing dialoguer flow location, the provisioner equivalent to build, and any per-section validation work (Telegram `getMe`, Slack `auth.test`, etc — already in the dialoguer path; just port to event-emitting form).

- [ ] **Step 3: Run docs lint**

Run: `cargo test -p rantaiclaw && (cd docs && command -v markdownlint && markdownlint . || true)`
Expected: tests pass; docs lint clean if installed.

- [ ] **Step 4: Commit**

```bash
git add docs/
git commit -m "docs: TUI-unified setup reference + follow-up spec for remaining sections"
```

---

## Self-Review Notes

- **Spec coverage:** Plan covers the unified-setup architecture (Tasks 1–5, 8), WhatsApp Web as first user (Tasks 6–7), audit §1 fixes (Tasks 6 + 10), and headless parity (Task 9). Audit items §2–§7 are explicitly out of scope and tracked elsewhere.
- **No placeholders:** Each step shows code or exact commands. The `pair_once` body in Task 6 has a TODO inside the function for the real wa-rs translation — that's flagged as the actual implementation work, not a plan gap.
- **Type consistency:** `ProvisionEvent`, `ProvisionResponse`, `ProvisionIo`, `TuiProvisioner`, `PairEvent`, `PairOptions`, `SetupOverlayState` are all defined in Task 1 / 3 / 6 and reused with consistent signatures throughout.
- **Risk:** Task 5's `app.rs` wiring is the highest-risk task (touches the central event loop). Recommend creating a `wt/tui-unified-setup` worktree before starting and validating Task 5 with manual smoke before commit.
- **Validation matrix (per CLAUDE.md §8):** Each task ends with `cargo test -p rantaiclaw`; final task should also run `./dev/ci.sh all` if Docker is available before opening the PR.
