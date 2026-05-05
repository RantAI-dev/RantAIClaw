use std::io::{self, IsTerminal, Stdout, Write};

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Widget, Wrap},
    Terminal, TerminalOptions, Viewport,
};

use tokio::sync::mpsc;

use super::async_bridge::{TuiAgentActor, TurnRequest};
use super::commands::{CommandRegistry, CommandResult as CmdResult};
use super::context::{TokenUsage, TuiContext};
use super::TuiConfig;
use crate::agent::agent::Agent;
use crate::agent::events::{AgentEvent, AgentEventSender};
use crate::sessions::SessionStore;

/// Per-tool-call accumulation state used while streaming.
#[derive(Debug, Clone)]
pub struct ToolBlockState {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub result: Option<(bool, String)>, // (ok, preview)
}

/// Current application state.
#[derive(Debug, Default)]
pub enum AppState {
    #[default]
    Ready,
    Streaming {
        partial: String,
        tool_blocks: Vec<ToolBlockState>,
        cancelling: bool,
    },
    Quitting,
}

/// Result of processing one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventResult {
    Continue,
    Quit,
}

/// Top-level TUI application.
pub struct TuiApp {
    pub state: AppState,
    pub context: TuiContext,
    pub command_registry: CommandRegistry,
    /// Active config, cloned for provisioner use.
    pub config: crate::config::Config,
    /// Active profile, cloned for provisioner use.
    pub profile: crate::profile::Profile,
    /// Slash-command dropdown — visible whenever the input buffer starts
    /// with `/`. Filtered by prefix on every keystroke.
    pub autocomplete: super::widgets::autocomplete::Autocomplete,
    /// Active modal overlay (e.g. /help). `None` = no overlay shown.
    /// Esc dismisses; left/right arrows cycle tabs.
    pub overlay: Option<super::commands::OverlayContent>,
    /// Active setup provisioner overlay. When `Some`, the chat input is
    /// suppressed and key events route to the overlay. The provisioner
    /// runs on a tokio task and emits `ProvisionEvent`s received here.
    pub setup_overlay: Option<super::SetupOverlayState>,
    /// Receiver for events emitted by the active provisioner.
    pub setup_event_rx:
        Option<tokio::sync::mpsc::Receiver<crate::onboard::provision::ProvisionEvent>>,
    /// Sender for responses (prompt answers) back to the provisioner.
    pub setup_response_tx:
        Option<tokio::sync::mpsc::Sender<crate::onboard::provision::ProvisionResponse>>,
    /// Active interactive list picker — Up/Down/Enter/Esc overlay used
    /// by `/model`, `/sessions`, `/resume`, `/personality`, etc. The
    /// `ListPicker.kind` tag tells the Enter handler what to do with the
    /// selected key. `None` when no picker is open.
    pub list_picker: Option<super::widgets::ListPicker>,
    /// Inline-mode scrollback queue. The event loop drains this list
    /// before each frame and emits each entry into the terminal's native
    /// scrollback above the viewport. Each entry is `(role, content)`.
    pub scrollback_queue: Vec<(String, String)>,
    /// Bytes of the current streaming `partial` already flushed to
    /// scrollback. Used to stream assistant output line-by-line into
    /// terminal scrollback while the turn is still in progress. Reset
    /// each time a new turn starts.
    pub stream_committed_chars: usize,
    /// Whether the `Assistant: ` header line for the current streaming
    /// turn has already been written to scrollback. Reset per turn.
    pub stream_header_committed: bool,
    /// `true` when Ctrl+G was pressed and the run loop should suspend
    /// the terminal, hand control to `$EDITOR`, and copy the resulting
    /// file contents back into the input buffer. The key handler can't
    /// run the editor itself because it doesn't own the `Terminal`.
    pub editor_request: bool,
    /// `true` when the run loop should wipe the terminal's screen and
    /// scrollback before the next render (e.g. after `/new`/`/clear`).
    /// Set by command handlers via `CommandResult::ClearTerminal` and
    /// consumed once the wipe is performed.
    pub clear_terminal_request: bool,
    /// First-run wizard. When `Some`, the app renders the wizard
    /// instead of the normal chat UI. Provisioner steps use the
    /// existing `setup_overlay` mechanism.
    pub first_run_wizard: Option<super::FirstRunWizard>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupTopicAction {
    TuiProvisioner(String),
    OpenChannelSubPicker,
    Unknown,
}

pub fn dispatch_setup_topic_key(key: &str) -> SetupTopicAction {
    if key == "channels" {
        return SetupTopicAction::OpenChannelSubPicker;
    }
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
    /// Create a new `TuiApp`, starting or resuming a session based on config.
    ///
    /// `req_tx` and `events_rx` are the TUI-side ends of the bridge to the
    /// `TuiAgentActor`. The actor owns the paired `req_rx`/`events_tx` and is
    /// spawned by `run_tui` before the app is constructed.
    pub fn new(
        tui_config: &TuiConfig,
        config: crate::config::Config,
        profile: crate::profile::Profile,
        req_tx: mpsc::Sender<TurnRequest>,
        events_rx: mpsc::Receiver<AgentEvent>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&tui_config.data_dir)?;
        let db_path = tui_config.data_dir.join("sessions.db");
        let store = SessionStore::open(&db_path)?;
        // Best-effort one-shot: derive titles for legacy sessions that
        // never went through the auto-titling path. Idempotent — a no-op
        // once every session has a title.
        let _ = store.backfill_titles();

        let mut context = TuiContext::new(
            store,
            &tui_config.model,
            tui_config.resume_session.as_deref(),
            req_tx,
            events_rx,
        )?;

        // Snapshot of every registered command so /help can build its
        // picker without reaching back into TuiApp from the command
        // handler (which only sees TuiContext).
        let command_registry = CommandRegistry::new();
        context.available_commands = command_registry
            .get_help()
            .into_iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();

        Ok(Self {
            state: AppState::Ready,
            context,
            command_registry,
            config,
            profile,
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
        })
    }

    /// Re-evaluate the slash-command dropdown against the current input
    /// buffer. Called after every keystroke that mutates `input_buffer`.
    fn refresh_autocomplete(&mut self) {
        let buf = &self.context.input_buffer;
        if buf.starts_with('/') && !buf.contains(' ') && !buf.contains('\n') {
            let suggestions = self.command_registry.autocomplete_with_descriptions(buf);
            self.autocomplete.update(suggestions);
        } else {
            self.autocomplete.hide();
        }
    }

    /// Replace the input buffer with the highlighted command name.
    fn complete_selected_command(&mut self) {
        if let Some(name) = self.autocomplete.selected() {
            self.context.input_buffer = format!("{name} ");
            self.autocomplete.hide();
        }
    }

    /// Process a single terminal event, returning whether to continue or quit.
    pub async fn handle_event(&mut self, event: Event) -> Result<EventResult> {
        if let Event::Key(key) = event {
            return self.handle_key(key).await;
        }
        Ok(EventResult::Continue)
    }

    /// Dispatch a key event.
    pub async fn handle_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        match key.code {
            // Ctrl+D → always quit
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state = AppState::Quitting;
                return Ok(EventResult::Quit);
            }
            // Ctrl+C → cancel streaming turn if active; otherwise quit
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match &mut self.state {
                    AppState::Streaming { cancelling, .. } => {
                        *cancelling = true;
                        if let Err(e) = self.context.req_tx.send(TurnRequest::Cancel).await {
                            // Bridge closed — fall through to quit.
                            self.context.last_error = Some(format!("cancel failed: {e}"));
                            self.state = AppState::Quitting;
                            return Ok(EventResult::Quit);
                        }
                        return Ok(EventResult::Continue);
                    }
                    AppState::Ready | AppState::Quitting => {
                        self.state = AppState::Quitting;
                        return Ok(EventResult::Quit);
                    }
                }
            }
            // List picker active — intercepts arrows/enter/esc and
            // routes printable chars to the search query. Up/Down move
            // within the *filtered* view; Enter dispatches the selected
            // item by kind; Esc dismisses; Backspace deletes from the
            // query. All other keys are swallowed so the user can't
            // type into the input buffer behind the picker.
            KeyCode::Up if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.move_up();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Down if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.move_down();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Left if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.prev_page();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Right if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.next_page();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Enter if self.list_picker.is_some() => {
                self.dispatch_list_picker_selection();
                return Ok(EventResult::Continue);
            }
            KeyCode::Esc if self.list_picker.is_some() => {
                self.list_picker = None;
                return Ok(EventResult::Continue);
            }
            KeyCode::Backspace if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.pop_query_char();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Char(c)
                if self.list_picker.is_some()
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(p) = self.list_picker.as_mut() {
                    p.push_query_char(c);
                }
                return Ok(EventResult::Continue);
            }
            _ if self.list_picker.is_some() => {
                // Picker open — swallow everything else.
                return Ok(EventResult::Continue);
            }
            // Tab — completes the highlighted command from the dropdown.
            // Always wins over the cycling-tabs handler when typing a
            // slash command (the autocomplete is visible only then).
            KeyCode::Tab if self.autocomplete.is_visible() => {
                self.complete_selected_command();
            }
            // Ctrl+Enter → submit (Kitty-protocol terminals).
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.autocomplete.hide();
                self.submit_input().await?;
            }
            // Ctrl+J → newline (alt for terminals that don't pass Shift+Enter).
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.context.input_buffer.push('\n');
            }
            // Ctrl+G → suspend the TUI and open the current input
            // buffer in $EDITOR. The actual swap happens in `run_loop`
            // (which owns the Terminal); we just raise a flag here.
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.editor_request = true;
            }
            // Plain Enter — Hermes / Claude Code convention:
            //   * If the autocomplete dropdown is visible and the highlighted
            //     command differs from what the user typed → complete first;
            //     a second Enter then fires it. If it already matches, submit.
            //   * Otherwise → submit. Period. Both prose and slash commands.
            //
            // Multi-line prompts work via Ctrl+J (handled above) or
            // Shift+Enter on terminals with the kitty keyboard protocol.
            KeyCode::Enter if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                if self.autocomplete.is_visible() {
                    let buf = self.context.input_buffer.trim_end_matches(' ').to_string();
                    let selected = self.autocomplete.selected().map(str::to_string);
                    if selected.as_deref() == Some(buf.as_str()) {
                        self.autocomplete.hide();
                        self.submit_input().await?;
                    } else {
                        self.complete_selected_command();
                    }
                } else {
                    self.submit_input().await?;
                }
            }
            // Escape — dismiss the dropdown without changing the buffer.
            KeyCode::Esc if self.autocomplete.is_visible() => {
                self.autocomplete.hide();
            }
            // Escape — close the modal overlay (e.g. /help).
            KeyCode::Esc if self.overlay.is_some() => {
                self.overlay = None;
            }
            // Tab cycles tabs in the overlay.
            KeyCode::Tab if self.overlay.is_some() => {
                if let Some(o) = self.overlay.as_mut() {
                    if !o.tabs.is_empty() {
                        o.active_tab = (o.active_tab + 1) % o.tabs.len();
                    }
                }
            }
            // Left/Right also cycle tabs in the overlay.
            KeyCode::Left if self.overlay.is_some() => {
                if let Some(o) = self.overlay.as_mut() {
                    if !o.tabs.is_empty() {
                        o.active_tab = if o.active_tab == 0 {
                            o.tabs.len() - 1
                        } else {
                            o.active_tab - 1
                        };
                    }
                }
            }
            KeyCode::Right if self.overlay.is_some() => {
                if let Some(o) = self.overlay.as_mut() {
                    if !o.tabs.is_empty() {
                        o.active_tab = (o.active_tab + 1) % o.tabs.len();
                    }
                }
            }
            // Backspace
            KeyCode::Backspace if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                self.context.input_buffer.pop();
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            // Regular character input
            KeyCode::Char(c) if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                self.context.input_buffer.push(c);
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            // Up/Down navigate the dropdown when visible; otherwise scroll
            // the chat history.
            KeyCode::Up if self.autocomplete.is_visible() => {
                self.autocomplete.previous();
            }
            KeyCode::Down if self.autocomplete.is_visible() => {
                self.autocomplete.next();
            }
            // Up/Down with no modal active recalls submitted prompts
            // from history. Native terminal scrollback (mouse wheel /
            // PgUp on the host terminal) handles chat scrolling.
            KeyCode::Up if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                if let Some(text) = self.context.history_recall_older() {
                    self.context.input_buffer = text;
                    self.refresh_autocomplete();
                }
            }
            KeyCode::Down if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                if let Some(text) = self.context.history_recall_newer() {
                    self.context.input_buffer = text;
                    self.refresh_autocomplete();
                }
            }
            // Esc — close the setup overlay or cancel the wizard.
            KeyCode::Esc if self.setup_overlay.is_some() || self.first_run_wizard.is_some() => {
                // If wizard is active, cancel the entire wizard.
                if self.first_run_wizard.is_some() {
                    let was_finished = self
                        .setup_overlay
                        .as_ref()
                        .map(|o| o.finished)
                        .unwrap_or(false);
                    if !was_finished {
                        if let Some(tx) = self.setup_response_tx.take() {
                            let _ = tx
                                .send(crate::onboard::provision::ProvisionResponse::Cancelled)
                                .await;
                        }
                    }
                    self.first_run_wizard = None;
                    self.setup_overlay = None;
                    self.setup_event_rx = None;
                    self.setup_response_tx = None;
                    if let Err(e) = self.reload_config() {
                        tracing::warn!("failed to reload config after wizard cancel: {}", e);
                    }
                    return Ok(EventResult::Continue);
                }
                let was_finished = self
                    .setup_overlay
                    .as_ref()
                    .map(|o| o.finished)
                    .unwrap_or(false);
                // Cancel only if the provisioner is still running. If
                // `finished` is set, the task has already exited and
                // sending Cancelled would be a no-op (and may panic
                // if the receiver has been dropped).
                if !was_finished {
                    if let Some(tx) = self.setup_response_tx.take() {
                        let _ = tx
                            .send(crate::onboard::provision::ProvisionResponse::Cancelled)
                            .await;
                    }
                }
                self.setup_overlay = None;
                self.setup_event_rx = None;
                self.setup_response_tx = None;
                // Reload config so freshly-written sections take effect.
                if let Err(e) = self.reload_config() {
                    tracing::warn!("failed to reload config after setup: {}", e);
                }
            }
            // Enter — submit the active prompt response.
            KeyCode::Enter
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_prompt().is_some()) =>
            {
                let (_id, value) = {
                    let overlay = self.setup_overlay.as_mut().unwrap();
                    overlay
                        .submit_prompt()
                        .unwrap_or_else(|| ("".into(), "".into()))
                };
                if let Some(tx) = &self.setup_response_tx {
                    let _ = tx
                        .send(crate::onboard::provision::ProvisionResponse::Text(value))
                        .await;
                }
                // Also drain the event that carries the prompt to the overlay.
                if let Some(rx) = &mut self.setup_event_rx {
                    while let Ok(ev) = rx.try_recv() {
                        if let Some(o) = &mut self.setup_overlay {
                            o.handle_event(ev);
                        }
                    }
                }
            }
            // Char input — route to overlay input when a prompt is active.
            KeyCode::Char(c)
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_prompt().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.push_char(c);
                }
            }
            // Backspace — delete from overlay input when a prompt is active.
            KeyCode::Backspace
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_prompt().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.pop_char();
                }
            }
            // Up — choose navigation.
            KeyCode::Up
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.choose_move_up();
                }
            }
            // Down — choose navigation.
            KeyCode::Down
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.choose_move_down();
                }
            }
            // Space — toggle in multi-select choose.
            KeyCode::Char(' ')
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.choose_toggle();
                }
            }
            // First-run wizard Enter handling — non-provisioner phases only.
            KeyCode::Enter if self.first_run_wizard.is_some() => {
                let wizard = self.first_run_wizard.as_mut().unwrap();
                match wizard.phase {
                    super::first_run_wizard::WizardPhase::Welcome => {
                        wizard.phase =
                            super::first_run_wizard::WizardPhase::RunningProvisioner { idx: 0 };
                        if let Some(name) = wizard.current_provisioner_name() {
                            if let Err(e) = self.open_setup_overlay(name.to_string()) {
                                let msg = format!("Failed to open provisioner: {}", e);
                                let _ = self.context.append_system_message(&msg);
                            }
                        }
                    }
                    super::first_run_wizard::WizardPhase::ProjectContext => {
                        wizard.finish_project_context();
                    }
                    super::first_run_wizard::WizardPhase::ScaffoldFiles => {
                        wizard.finish();
                    }
                    super::first_run_wizard::WizardPhase::Complete => {
                        self.first_run_wizard = None;
                    }
                    super::first_run_wizard::WizardPhase::RunningProvisioner { .. } => {
                        // Provisioner overlay handles Enter
                    }
                }
            }
            // Enter — submit choose or submit prompt. Skip if wizard is in Welcome
            // phase (the wizard Enter handler takes over in that case).
            KeyCode::Enter
                if self.setup_overlay.is_some()
                    && !self.first_run_wizard.as_ref().is_some_and(|w| {
                        matches!(w.phase, super::first_run_wizard::WizardPhase::Welcome)
                    })
                    && (self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some())
                        || self
                            .setup_overlay
                            .as_ref()
                            .is_some_and(|o| o.active_prompt().is_some())) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    if let Some((id, sel)) = o.submit_choose() {
                        if let Some(tx) = &self.setup_response_tx {
                            let _ = tx
                                .send(crate::onboard::provision::ProvisionResponse::Selection(sel));
                        }
                    } else if let Some((id, val)) = o.submit_prompt() {
                        if let Some(tx) = &self.setup_response_tx {
                            let _ =
                                tx.send(crate::onboard::provision::ProvisionResponse::Text(val));
                        }
                    }
                }
            }
            // Setup overlay open with no active prompt/choose — Up/Down/
            // PageUp/PageDown/Home/End scroll the overlay so the user can
            // see content (especially the QR + status log) that exceeds
            // the terminal height.
            KeyCode::Up if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_up();
                }
            }
            KeyCode::Down if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_down();
                }
            }
            KeyCode::PageUp if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_page_up();
                }
            }
            KeyCode::PageDown if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_page_down();
                }
            }
            KeyCode::Home if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_home();
                }
            }
            KeyCode::End if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_end();
                }
            }
            // Catch-all for any other key while overlay is open — swallow.
            _ if self.setup_overlay.is_some() => {
                return Ok(EventResult::Continue);
            }
            _ => {}
        }
        Ok(EventResult::Continue)
    }

    /// Submit the current input buffer as a message (or command).
    ///
    /// For slash-prefixed input, dispatches to the command registry.
    /// For message input in `Ready` state, records the user turn, sends a
    /// `TurnRequest::Submit` to the `TuiAgentActor` via the bridge, and
    /// transitions to `Streaming`. For message input in `Streaming` state,
    /// the request is still dispatched (the actor will queue it) and
    /// `queued_turns` is incremented so the status bar reflects backlog.
    pub async fn submit_input(&mut self) -> Result<()> {
        let raw = std::mem::take(&mut self.context.input_buffer);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        // Record this submission in input history so Up/Down can
        // recall it later. Also reset history navigation state so the
        // next Up press starts from the most recent entry.
        self.context.push_input_history(trimmed);
        self.context.exit_history_navigation();

        // Slash-commands bypass the bridge entirely (except `/retry`
        // which dispatches via `handle_command` → `dispatch_resubmit`).
        if let Some(cmd) = trimmed.strip_prefix('/') {
            // Echo the slash command into scrollback first, so the user
            // can see what they typed above the response (matches the
            // normal user/assistant exchange flow).
            self.scrollback_queue
                .push(("user".to_string(), format!("/{cmd}", cmd = cmd.trim())));
            let cmd = cmd.trim().to_string();
            self.handle_command(&cmd).await?;
            self.context.scroll_offset = 0;
            return Ok(());
        }

        let text = trimmed.to_string();
        self.context.append_user_message(&text)?;
        // Commit user message to scrollback so it appears inline like
        // Hermes/Claude-Code chat output.
        self.scrollback_queue
            .push(("user".to_string(), text.clone()));

        // Dispatch to the actor. If the bridge is closed (e.g. actor has
        // exited), surface a visible error but do not propagate — the TUI
        // should remain responsive so the user can /quit cleanly.
        if let Err(e) = self.context.req_tx.send(TurnRequest::Submit(text)).await {
            self.context.last_error = Some(format!("agent bridge closed: {e}"));
            self.context.scroll_offset = 0;
            return Ok(());
        }

        match self.state {
            AppState::Ready => {
                self.state = AppState::Streaming {
                    partial: String::new(),
                    tool_blocks: Vec::new(),
                    cancelling: false,
                };
                self.stream_committed_chars = 0;
                self.stream_header_committed = false;
            }
            AppState::Streaming { .. } => {
                self.context.queued_turns += 1;
            }
            AppState::Quitting => {}
        }

        self.context.scroll_offset = 0;
        Ok(())
    }

    /// Drain any queued `AgentEvent`s from the bridge without blocking.
    ///
    /// Called once per frame by the render loop, before rendering, so that
    /// state transitions (Chunk, ToolCall*, Usage, Done, Error) are reflected
    /// in the next paint. Uses `try_recv` to remain non-blocking on an empty
    /// channel; a closed channel is treated the same as empty here — the
    /// actor lifecycle is separately managed by `run_tui`.
    pub fn drain_events(&mut self) {
        while let Ok(ev) = self.context.events_rx.try_recv() {
            self.handle_agent_event(ev);
        }
        if let Some(rx) = &mut self.setup_event_rx {
            while let Ok(ev) = rx.try_recv() {
                if let Some(overlay) = &mut self.setup_overlay {
                    overlay.handle_event(ev.clone());
                }
                if let Some(wizard) = &mut self.first_run_wizard {
                    wizard.handle_provisioner_event(ev);
                }
            }
        }
        // Wizard auto-advance: when the current provisioner finishes,
        // spawn the next one or advance to the next wizard phase.
        if let Some(wizard) = &mut self.first_run_wizard {
            if matches!(
                wizard.phase,
                super::first_run_wizard::WizardPhase::RunningProvisioner { .. }
            ) {
                if self.setup_overlay.as_ref().is_some_and(|o| o.finished) {
                    self.setup_overlay = None;
                    self.setup_event_rx = None;
                    self.setup_response_tx = None;
                    wizard.advance();
                    if matches!(
                        wizard.phase,
                        super::first_run_wizard::WizardPhase::RunningProvisioner { .. }
                    ) {
                        if let Some(name) = wizard.current_provisioner_name() {
                            if let Err(e) = self.open_setup_overlay(name.to_string()) {
                                let msg = format!("Failed to open provisioner: {}", e);
                                let _ = self.context.append_system_message(&msg);
                            }
                        }
                    }
                }
            }
        }
        // Note: we no longer auto-clear the overlay when the provisioner
        // sets `finished = true`. The user dismisses via Esc so they can
        // read the success/failure summary at their own pace. The Esc
        // handler does the cleanup (clear overlay state + reload config).
    }

    fn reload_config(&mut self) -> anyhow::Result<()> {
        let path = self.profile.config_toml();
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let config: crate::config::Config =
            toml::from_str(&contents).context("failed to parse config file")?;
        self.config = config;
        Ok(())
    }

    /// Apply a single `AgentEvent` to the app state.
    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Chunk(s) => {
                if let AppState::Streaming { partial, .. } = &mut self.state {
                    partial.push_str(&s);
                }
            }
            AgentEvent::ToolCallStart { id, name, args } => {
                if let AppState::Streaming { tool_blocks, .. } = &mut self.state {
                    tool_blocks.push(ToolBlockState {
                        id,
                        name,
                        args,
                        result: None,
                    });
                }
            }
            AgentEvent::ToolCallEnd {
                id,
                ok,
                output_preview,
            } => {
                if let AppState::Streaming { tool_blocks, .. } = &mut self.state {
                    if let Some(b) = tool_blocks.iter_mut().find(|b| b.id == id) {
                        b.result = Some((ok, output_preview));
                    }
                }
            }
            AgentEvent::Usage(u) => {
                // Map the agent's cost::TokenUsage onto the TUI's tally shape.
                self.context.token_usage = TokenUsage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u.total_tokens,
                };
            }
            AgentEvent::Done {
                final_text,
                cancelled,
            } => {
                self.finalize_turn(final_text, cancelled);
            }
            AgentEvent::Error(msg) => {
                self.finalize_error(msg);
            }
        }
    }

    /// Finalize a turn on `AgentEvent::Done`.
    ///
    /// On cancel, the inline `Agent::turn_streaming` loop emits an empty
    /// `final_text` (it cannot easily salvage buffered partial text), so the
    /// TUI must reconstruct the visible output from the local `partial`
    /// accumulator built up from `Chunk` events. A `[cancelled]` marker is
    /// appended in that case so the user sees the interruption clearly.
    ///
    /// If more turns are queued, transitions back to a fresh `Streaming`
    /// state; otherwise returns to `Ready`.
    fn finalize_turn(&mut self, final_text: String, cancelled: bool) {
        let mut body = if cancelled && final_text.is_empty() {
            if let AppState::Streaming { partial, .. } = &self.state {
                partial.clone()
            } else {
                String::new()
            }
        } else {
            final_text
        };

        if cancelled {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("[cancelled]");
        }

        // Snapshot tool blocks from streaming state before we transition
        // away — they're discarded otherwise.
        let tool_calls_json = if let AppState::Streaming { tool_blocks, .. } = &self.state {
            super::render::serialize_tool_calls(tool_blocks)
        } else {
            None
        };

        // Persist and display the assistant reply. A store failure should not
        // crash the loop — surface it as a visible error and keep running.
        if let Err(e) = self
            .context
            .append_assistant_message_with_tools(&body, tool_calls_json)
        {
            self.context.last_error = Some(format!("failed to persist reply: {e}"));
        }
        // Commit assistant message to scrollback (inline mode).
        // If we've already been streaming this turn line-by-line into
        // scrollback, only emit the trailing partial-line tail (if any)
        // plus a blank separator. Otherwise emit the full message.
        if self.stream_header_committed {
            let committed = self.stream_committed_chars.min(body.len());
            let tail = body[committed..].to_string();
            if !tail.is_empty() {
                self.scrollback_queue
                    .push(("_continuation".to_string(), tail));
            } else {
                self.scrollback_queue
                    .push(("_continuation".to_string(), String::new()));
            }
        } else {
            self.scrollback_queue
                .push(("assistant".to_string(), body.clone()));
        }

        if self.context.queued_turns > 0 {
            self.context.queued_turns -= 1;
            self.state = AppState::Streaming {
                partial: String::new(),
                tool_blocks: Vec::new(),
                cancelling: false,
            };
            self.stream_committed_chars = 0;
            self.stream_header_committed = false;
        } else {
            self.state = AppState::Ready;
        }
    }

    /// Finalize a turn on `AgentEvent::Error`. Surfaces the error as a
    /// visible assistant message (so it shows up in chat history) AND sets
    /// `last_error` so the status bar reflects it until cleared.
    ///
    /// Recognizes a small list of common error shapes (API key not set,
    /// rate-limited, model not available) and rewrites them into a
    /// short, actionable line so the chat doesn't get a wall of stack
    /// trace. Unknown errors fall through verbatim.
    fn finalize_error(&mut self, msg: String) {
        let (chat_body, status_line) = format_agent_error(&msg);
        if let Err(e) = self.context.append_assistant_message(&chat_body) {
            self.context.last_error = Some(format!("failed to persist error: {e}"));
        } else {
            self.context.last_error = Some(status_line);
        }
        // Commit error message to scrollback so the user sees it inline.
        self.scrollback_queue
            .push(("system".to_string(), chat_body));
        self.state = AppState::Ready;
    }

    /// Handle a slash command (text after the leading `/`).
    pub async fn handle_command(&mut self, cmd: &str) -> Result<()> {
        match self.command_registry.dispatch(cmd, &mut self.context)? {
            CmdResult::Quit => {
                self.state = AppState::Quitting;
            }
            CmdResult::Message(msg) => {
                // Append as a system chat message so multi-line content
                // renders properly. The status bar's `last_error` slot is
                // reserved for errors only.
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            CmdResult::Overlay(content) => {
                // Inline-mode: render the overlay's body straight into
                // terminal scrollback (Hermes / Claude Code feel) so the
                // user can scroll back through long output natively. We
                // flatten the active tab plus a header line.
                let mut out = String::new();
                out.push_str(&format!("{}\n", content.title));
                for (i, tab) in content.tabs.iter().enumerate() {
                    if content.tabs.len() > 1 {
                        let marker = if i == content.active_tab {
                            "▸ "
                        } else {
                            "  "
                        };
                        out.push_str(&format!("\n{}{}\n", marker, tab.label));
                    }
                    for line in &tab.body {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                let _ = self.context.append_system_message(&out);
                self.scrollback_queue.push(("system".to_string(), out));
            }
            CmdResult::Continue | CmdResult::ClearError => {
                self.context.last_error = None;
            }
            CmdResult::Resubmit(text) => {
                self.dispatch_resubmit(text).await;
            }
            CmdResult::OpenListPicker(picker) => {
                self.list_picker = Some(picker);
            }
            CmdResult::OpenSetupOverlay { provisioner } => {
                if let Some(name) = provisioner {
                    if let Err(e) = self.open_setup_overlay(name) {
                        let msg = format!("Failed to open setup: {}", e);
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".to_string(), msg));
                    }
                }
            }
            CmdResult::OpenFirstRunWizard => {
                self.first_run_wizard = Some(super::FirstRunWizard::new(self.profile.clone()));
            }
            CmdResult::ClearTerminal(announce) => {
                // The actual screen+scrollback wipe runs in `run_loop`
                // (which owns the Terminal). We just raise the flag and
                // queue the announcement to be emitted on the fresh
                // screen. Drop any pending scrollback so we don't echo
                // the user's `/new` line into the cleared terminal.
                self.scrollback_queue.clear();
                self.clear_terminal_request = true;
                let _ = self.context.append_system_message(&announce);
                self.scrollback_queue.push(("system".to_string(), announce));
            }
        }
        Ok(())
    }

    /// Apply the user's selection from the active list picker. Matches
    /// on `ListPickerKind` so each picker type runs its own side effect
    /// (switch model, resume session, set personality…). Always closes
    /// the picker afterward.
    fn dispatch_list_picker_selection(&mut self) {
        use super::widgets::ListPickerKind;

        let (kind, key) = match self
            .list_picker
            .as_ref()
            .and_then(|p| p.current().map(|item| (p.kind, item.key.clone())))
        {
            Some(v) => v,
            None => {
                self.list_picker = None;
                return;
            }
        };
        self.list_picker = None;

        match kind {
            ListPickerKind::Model => {
                self.context.model = key.clone();
                let msg = format!("Model set to: {key}");
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            ListPickerKind::Session => {
                let session = match self.context.session_store.get_session(&key) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        let msg = format!("Session {key} not found.");
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".to_string(), msg));
                        return;
                    }
                    Err(e) => {
                        self.context.last_error = Some(format!("resume failed: {e}"));
                        return;
                    }
                };
                self.context.session_id = session.id.clone();
                self.context.model = session.model.clone();
                self.context.messages.clear();
                if let Err(e) = self.context.load_session_messages() {
                    self.context.last_error = Some(format!("load_session failed: {e}"));
                    return;
                }
                let short = &session.id[..session.id.len().min(8)];
                let msg = format!(
                    "Resumed session {short} ({} messages)",
                    self.context.messages.len()
                );
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            ListPickerKind::Personality => {
                let msg = format!(
                    "Personality set to: {key}\n(Full integration with system prompt pending)"
                );
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            ListPickerKind::Skill => {
                // Pre-fill an invocation prompt into the input buffer.
                // The user can edit, append context, and Enter to send.
                self.context.input_buffer = format!("Use the {key} skill: ");
                self.refresh_autocomplete();
            }
            ListPickerKind::Help => {
                // Pre-fill `/<command> ` into the input buffer so the
                // user can add args and submit (or just press Enter for
                // no-arg commands like /usage or /status).
                self.context.input_buffer = format!("/{key} ");
                self.refresh_autocomplete();
            }
            ListPickerKind::SetupTopic => match dispatch_setup_topic_key(&key) {
                SetupTopicAction::TuiProvisioner(ref name) => {
                    let name = name.clone();
                    match self.open_setup_overlay(name.clone()) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = format!("Failed to open {name} setup: {e}");
                            let _ = self.context.append_system_message(&msg);
                            self.scrollback_queue.push(("system".into(), msg));
                        }
                    }
                }
                SetupTopicAction::OpenChannelSubPicker => self.open_channel_sub_picker(),
                SetupTopicAction::Unknown => {
                    let msg = format!("Unknown setup topic: {key}");
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                }
            },
            ListPickerKind::SetupChannel => match dispatch_setup_channel_key(&key) {
                SetupChannelAction::TuiProvisioner(ref name) => {
                    let name = name.clone();
                    match self.open_setup_overlay(name.clone()) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = format!("Failed to open {name} setup: {e}");
                            let _ = self.context.append_system_message(&msg);
                            self.scrollback_queue.push(("system".into(), msg));
                        }
                    }
                }
                SetupChannelAction::Unknown => {
                    let msg = format!(
                        "Channel {key} is not yet available in-TUI. Run `rantaiclaw setup channels --non-interactive` from a shell to use the legacy CLI flow."
                    );
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                }
            },
        }
    }

    fn open_setup_overlay(&mut self, name: String) -> anyhow::Result<()> {
        use crate::onboard::provision::provisioner_for;

        let prov =
            provisioner_for(&name).ok_or_else(|| anyhow::anyhow!("unknown provisioner: {name}"))?;

        let (events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(8);

        let mut config = self.config.clone();
        let profile = self.profile.clone();

        let prov_name = prov.name().to_string();
        let overlay_state = crate::tui::SetupOverlayState::new(format!("Setup — {prov_name}"));

        self.setup_overlay = Some(overlay_state);
        self.setup_event_rx = Some(events_rx);
        self.setup_response_tx = Some(response_tx);

        let events_tx = events_tx;
        tokio::spawn(async move {
            // Clone events_tx so we can still report save failures to the
            // overlay after `prov.run` consumes the original via ProvisionIo.
            let save_failure_tx = events_tx.clone();
            let io = crate::onboard::provision::ProvisionIo {
                events: events_tx,
                responses: response_rx,
            };
            match prov.run(&mut config, &profile, io).await {
                Ok(()) => {
                    // Persist the mutated config to disk. Without this,
                    // every provisioner mutation is lost when the spawned
                    // task drops `config`. Config::save() also handles
                    // secret encryption (see config/schema.rs:save) so
                    // plaintext API keys captured during the flow get
                    // encrypted before hitting disk.
                    if let Err(e) = config.save().await {
                        tracing::error!(
                            provisioner = prov_name,
                            "failed to save config after provisioner: {e}"
                        );
                        // Best-effort surface to the overlay log so the
                        // user sees the failure instead of a phantom
                        // success.
                        let _ = save_failure_tx
                            .send(crate::onboard::provision::ProvisionEvent::Failed {
                                error: format!("Config save failed: {e}"),
                            })
                            .await;
                    }
                }
                Err(e) => {
                    tracing::error!(provisioner = prov_name, "provisioner error: {e}");
                }
            }
        });

        Ok(())
    }

    /// Dispatch a `/retry`-style resubmit: re-runs an existing user message
    /// without appending it to history. Refuses while a turn is already
    /// streaming — the user should cancel first.
    async fn dispatch_resubmit(&mut self, text: String) {
        match self.state {
            AppState::Streaming { .. } => {
                self.context.last_error = Some(
                    "Cannot retry while a response is streaming. Press Ctrl+C to cancel first."
                        .to_string(),
                );
            }
            AppState::Ready => {
                if let Err(e) = self.context.req_tx.send(TurnRequest::Submit(text)).await {
                    self.context.last_error = Some(format!("agent bridge closed: {e}"));
                    return;
                }
                self.state = AppState::Streaming {
                    partial: String::new(),
                    tool_blocks: Vec::new(),
                    cancelling: false,
                };
                self.context.last_error = None;
            }
            AppState::Quitting => {}
        }
    }

    fn open_channel_sub_picker(&mut self) {
        use crate::onboard::provision::{available, provisioner_for, ProvisionerCategory};
        use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

        let items: Vec<ListPickerItem> = available()
            .into_iter()
            .filter_map(|(name, desc)| {
                let p = provisioner_for(name)?;
                if p.category() == ProvisionerCategory::Channel {
                    Some(ListPickerItem {
                        key: name.to_string(),
                        primary: name.to_string(),
                        secondary: desc.to_string(),
                    })
                } else {
                    None
                }
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

    /// Render the inline viewport — only the bottom `INLINE_VIEWPORT_LINES`
    /// rows of the terminal. Chat history is NOT rendered here; messages
    /// commit to scrollback via `commit_message_to_scrollback` as they
    /// arrive. The viewport just hosts: status bar, input box, and any
    /// transient overlays (autocomplete dropdown, /help modal, streaming
    /// spinner).
    pub fn render(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let TuiApp {
            state,
            context,
            autocomplete,
            overlay,
            list_picker,
            stream_committed_chars,
            ..
        } = self;

        terminal.draw(|frame| {
            let area = frame.area();

            // Original tight 6-row layout: preview + input + status.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(STREAM_PREVIEW_LINES),
                    Constraint::Length(4),
                    Constraint::Length(1),
                ])
                .split(area);

            render_stream_preview_pane(state, *stream_committed_chars, frame, chunks[0]);
            render_input_pane(context, frame, chunks[1]);
            render_status_pane(context, frame, chunks[2]);

            // Modal overlay (e.g. /help) takes over the entire viewport.
            if let Some(content) = overlay.as_ref() {
                render_overlay_pane(content, frame, area);
            }

            // List picker overlay — covers the entire 6-row viewport.
            if let Some(picker) = list_picker.as_mut() {
                picker.render(frame, area);
            }

            // Setup overlay — full terminal coverage while active.
            if let Some(overlay_state) = self.setup_overlay.as_mut() {
                overlay_state.render(frame, area);
            }

            // First-run wizard — full terminal coverage, renders over everything.
            if let Some(wizard) = &mut self.first_run_wizard {
                wizard.render_fullscreen(frame, area);
            }

            // Slash-command dropdown — anchored just above the input box,
            // clamped to stay strictly inside the inline viewport.
            if autocomplete.is_visible() {
                let input_area = chunks[1];
                let space_above = input_area.y.saturating_sub(area.y);
                let max_rows: u16 = 8;
                let desired = (max_rows + 2).min(area.height.saturating_sub(1));
                let popup_height = desired.min(space_above.max(input_area.height));
                if popup_height >= 3 {
                    let popup_y = if space_above >= popup_height {
                        input_area.y - popup_height
                    } else {
                        input_area.y
                    };
                    let popup_area = Rect {
                        x: input_area.x,
                        y: popup_y,
                        width: input_area.width,
                        height: popup_height,
                    };
                    autocomplete.render(frame, popup_area);
                }
            }
        })?;
        Ok(())
    }

    /// Render path while the list picker is open. Uses the full
    /// terminal area (alt-screen mode) so the picker can show a search
    /// bar, many list rows, and a hotkey footer — Hermes / Claude-Code
    /// style. The status bar and input box are intentionally hidden
    /// here; the user is in modal selection mode.
    pub fn render_fullscreen_picker(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let TuiApp { list_picker, .. } = self;
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some(picker) = list_picker.as_mut() {
                picker.render_fullscreen(frame, area);
            }
        })?;
        Ok(())
    }

    /// Render path while the slash-command autocomplete dropdown is
    /// visible (alt-screen mode). Layout: input box at top, dropdown
    /// below (taking the bulk of the screen so many commands are
    /// visible at once), status bar at bottom — matches the Claude-Code
    /// reference image. The user keeps typing into the input; the
    /// dropdown re-filters live as they go.
    pub fn render_fullscreen_autocomplete(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let TuiApp {
            context,
            autocomplete,
            ..
        } = self;
        terminal.draw(|frame| {
            let area = frame.area();
            // Layout: 1 row top margin · 4 rows input · 1 row spacer ·
            // remaining rows for dropdown · 1 row status. Input pinned
            // near the top so typing position stays consistent with
            // inline mode; dropdown gets all the leftover height.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top margin
                    Constraint::Length(4), // input box
                    Constraint::Length(1), // spacer
                    Constraint::Min(3),    // dropdown
                    Constraint::Length(1), // status bar
                ])
                .split(area);

            render_input_pane(context, frame, chunks[1]);
            autocomplete.render(frame, chunks[3]);
            render_status_pane(context, frame, chunks[4]);
        })?;
        Ok(())
    }

    /// Commit a finalized message to the terminal's scrollback (above the
    /// inline viewport). This is the inline-mode equivalent of "append to
    /// chat history" — once committed the line is permanent and scrolls
    /// naturally with the terminal, exactly like Hermes / Claude Code.
    pub fn commit_message_to_scrollback(
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let size = terminal.size()?;
        let theme = super::render::RenderTheme::default();
        let mut lines = if role == "_continuation" {
            // Bare content lines (no role header) — used to flush the
            // trailing partial line after a streaming turn.
            content
                .split('\n')
                .map(|l| super::render::render_block_line(l, &theme))
                .collect::<Vec<_>>()
        } else {
            super::render::render_message_lines(role, content, &[], &[], &theme)
        };
        lines.push(Line::from(""));
        commit_lines_to_scrollback(terminal, lines, size.width, size.height)
    }

    /// Flush newly-completed lines from the active streaming `partial`
    /// into the terminal scrollback, splitting on `\n`. Each call commits
    /// only the bytes that have a trailing newline since the last flush;
    /// the still-incomplete tail stays in `partial` until either more
    /// data arrives or the turn finalizes. Idempotent on a finalized
    /// state.
    pub fn flush_stream_to_scrollback(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let partial = match &self.state {
            AppState::Streaming { partial, .. } => partial.clone(),
            _ => return Ok(()),
        };
        if self.stream_committed_chars > partial.len() {
            self.stream_committed_chars = 0;
        }
        let remaining = &partial[self.stream_committed_chars..];
        let last_nl = match remaining.rfind('\n') {
            Some(i) => i,
            None => return Ok(()),
        };
        let chunk = remaining[..=last_nl].to_string();
        let abs_end = self.stream_committed_chars + last_nl + 1;

        let theme = super::render::RenderTheme::default();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut iter = chunk.split_inclusive('\n');
        if !self.stream_header_committed {
            if let Some(first_line) = iter.next() {
                let trimmed = first_line.trim_end_matches('\n');
                let body = super::render::render_block_line(trimmed, &theme);
                let mut spans = vec![Span::styled(
                    "Assistant: ",
                    Style::default()
                        .fg(theme.assistant_label)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(body.spans);
                lines.push(Line::from(spans));
            }
            self.stream_header_committed = true;
        }
        for rest in iter {
            let trimmed = rest.trim_end_matches('\n');
            lines.push(super::render::render_block_line(trimmed, &theme));
        }

        if !lines.is_empty() {
            let size = terminal.size()?;
            commit_lines_to_scrollback(terminal, lines, size.width, size.height)?;
        }
        self.stream_committed_chars = abs_end;
        Ok(())
    }

    /// Print the splash banner + welcome line once, into scrollback,
    /// before the inline viewport takes over the bottom of the terminal.
    pub fn commit_splash_to_scrollback(
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        ctx: &TuiContext,
    ) -> Result<()> {
        let size = terminal.size()?;
        let lines = render_splash_lines();
        let session_short = &ctx.session_id[..8.min(ctx.session_id.len())];
        let mut all_lines = lines;
        all_lines.push(Line::from(""));
        all_lines.push(Line::from(vec![
            Span::styled(
                format!("Rantaiclaw v{}", env!("CARGO_PKG_VERSION")),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  · session {} ", session_short),
                Style::default().fg(Color::Rgb(107, 114, 128)),
            ),
        ]));
        all_lines.push(Line::from(""));
        commit_lines_to_scrollback(terminal, all_lines, size.width, size.height)
    }

    /// Original `render_header` shape, kept for backward callers.
    #[allow(dead_code)]
    fn render_header(&self, frame: &mut ratatui::Frame, area: Rect) {
        let session_short = &self.context.session_id[..8.min(self.context.session_id.len())];
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                "  Rantaiclaw  ",
                Style::default()
                    .fg(Color::Rgb(4, 11, 46))
                    .bg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" v{}  ", env!("CARGO_PKG_VERSION")),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· session {} ", session_short),
                Style::default().fg(Color::Rgb(107, 114, 128)),
            ),
        ]));
        frame.render_widget(header, area);
    }

    /// Render the scrollable chat history. Shows a brand splash when the
    /// session is empty (mirrors Hermes' opening screen).
    fn render_chat(&self, frame: &mut ratatui::Frame, area: Rect) {
        let theme = super::render::RenderTheme::default();
        let mut items: Vec<ListItem> = Vec::with_capacity(self.context.messages.len() + 1);

        // Empty-state splash — figlet wordmark + welcome line.
        if self.context.messages.is_empty() && !matches!(self.state, AppState::Streaming { .. }) {
            for line in render_splash_lines() {
                items.push(ListItem::new(line));
            }
        }

        for msg in &self.context.messages {
            let persisted = msg
                .tool_calls
                .as_deref()
                .map(super::render::parse_persisted_tool_calls)
                .unwrap_or_default();
            let lines = super::render::render_message_lines(
                &msg.role,
                &msg.content,
                &persisted,
                &[],
                &theme,
            );
            items.push(ListItem::new(lines));
        }

        // While a turn is streaming, render the in-progress assistant
        // message + tool blocks so the user sees live progress.
        if let AppState::Streaming {
            partial,
            tool_blocks,
            ..
        } = &self.state
        {
            // Spinner glyph cycles based on a millisecond clock so the user
            // sees motion during the inevitable LLM round-trip.
            let frame_idx = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as usize / 100)
                .unwrap_or(0))
                % SPINNER_FRAMES.len();
            let spinner = SPINNER_FRAMES[frame_idx];
            let header = Line::from(vec![
                Span::styled(
                    format!("{spinner} "),
                    Style::default()
                        .fg(Color::Rgb(94, 184, 255))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("thinking…", Style::default().fg(Color::Rgb(107, 114, 128))),
            ]);
            items.push(ListItem::new(header));
            let lines =
                super::render::render_message_lines("assistant", partial, &[], tool_blocks, &theme);
            items.push(ListItem::new(lines));
        }

        let list = List::new(items).block(
            Block::default()
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        "Chat",
                        Style::default()
                            .fg(Color::Rgb(94, 184, 255))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(40, 70, 140))),
        );
        frame.render_widget(list, area);
    }

    /// Render the multi-line input area with the brand cyan accent.
    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) {
        let prefix = Span::styled(
            "▎ ",
            Style::default()
                .fg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        );
        let body = if self.context.input_buffer.is_empty() {
            Span::styled(
                "Type a message…  (Ctrl+Enter send · /help for commands · Ctrl+C exit)",
                Style::default().fg(Color::Rgb(107, 114, 128)),
            )
        } else {
            Span::raw(self.context.input_buffer.clone())
        };

        let input = Paragraph::new(Line::from(vec![prefix, body]))
            .block(
                Block::default()
                    .title(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(
                            "$ you",
                            Style::default()
                                .fg(Color::Rgb(94, 184, 255))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                    ]))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Rgb(94, 184, 255))),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(input, area);
    }

    /// Render the Hermes-style bottom status bar with model · usage · session age.
    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let muted = Style::default().fg(Color::Rgb(107, 114, 128));
        let sky = Style::default().fg(Color::Rgb(94, 184, 255));
        let coral = Style::default()
            .fg(Color::Rgb(255, 123, 123))
            .add_modifier(Modifier::BOLD);

        let line = if let Some(ref err) = self.context.last_error {
            Line::from(vec![
                Span::styled(" ✗ ", coral),
                Span::styled(err.clone(), Style::default().fg(Color::Rgb(255, 123, 123))),
            ])
        } else {
            // Compact context-window meter — pretty-prints big numbers.
            let used = self.context.token_usage.total_tokens;
            let used_label = format_tokens(used);
            // Approximate context window from configured value if available.
            let window = self.context.context_window.unwrap_or(0);
            let window_label = if window > 0 {
                format!("/{}", format_tokens(window))
            } else {
                String::new()
            };
            let pct = if window > 0 {
                ((used as f64 / window as f64) * 100.0).round() as u32
            } else {
                0
            };

            // Session age in human time.
            let age_secs = self.context.started_at.elapsed().as_secs();
            let age_label = format_duration_short(age_secs);

            Line::from(vec![
                Span::styled(" $ ", sky),
                Span::styled(
                    self.context.model.clone(),
                    Style::default()
                        .fg(Color::Rgb(94, 184, 255))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  │  ", muted),
                Span::styled(
                    format!("{used_label}{window_label}"),
                    Style::default().fg(Color::Rgb(126, 226, 179)),
                ),
                Span::styled(
                    if window > 0 {
                        format!("  {pct}%")
                    } else {
                        String::new()
                    },
                    muted,
                ),
                Span::styled("  │  ", muted),
                Span::styled(format!("{} msgs", self.context.messages.len()), muted),
                Span::styled("  │  ", muted),
                Span::styled(age_label, muted),
            ])
        };

        let status = Paragraph::new(line);
        frame.render_widget(status, area);
    }
}

/// Spinner cycle used during streaming — Braille pattern matches the rest
/// of the brand's Unicode-forward look.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Recognise a handful of common agent error shapes and rewrite them as a
/// short, actionable chat message + a one-liner for the status bar.
/// Unknown errors fall through verbatim so we never lose information.
///
/// Returns `(chat_message_body, status_line)`.
fn format_agent_error(raw: &str) -> (String, String) {
    let lower = raw.to_lowercase();

    if lower.contains("api key not set") || lower.contains("api_key not set") {
        let body = "✗ Missing API key.\n\
\n\
Set one of the following before sending a message:\n\
  • Export `OPENROUTER_API_KEY=…` (or the equivalent for your provider)\n\
  • Run `rantaiclaw onboard` outside the TUI to save it to config\n\
  • Type `/quit`, then `rantaiclaw setup provider` for the guided flow"
            .to_string();
        return (
            body,
            "missing API key — set OPENROUTER_API_KEY or run setup".into(),
        );
    }

    if lower.contains("429") || lower.contains("rate limit") || lower.contains("rate-limit") {
        let body = "⚠ Rate-limited by the provider.\n\n\
            Wait a few seconds and try again, or switch models with `/model`."
            .to_string();
        return (body, "provider rate limit hit".into());
    }

    if lower.contains("not a valid model id") || lower.contains("model not found") {
        let body = format!(
            "✗ Model unavailable.\n\n\
            The configured model isn't accepted by your provider. \
            Pick a different one with `/model <name>` or run \
            `rantaiclaw setup provider --force`.\n\n\
            Provider response: {}",
            first_meaningful_line(raw)
        );
        return (body, "model unavailable — see chat for details".into());
    }

    // Default: trim the verbose "Attempts: provider= ... attempt 1/3" tail
    // so the chat shows the human-readable cause first, then the rest as
    // dim context.
    let trimmed = compact_provider_error(raw);
    let status = first_meaningful_line(&trimmed);
    (format!("✗ {trimmed}"), status)
}

/// Pull the first non-trivial line out of a multi-line error blob.
fn first_meaningful_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("Attempts:"))
        .unwrap_or("")
        .to_string()
}

/// Reduce a "All providers/models failed. Attempts: …" blob to the most
/// useful line plus a one-liner attempts summary. Keeps the user oriented
/// without dumping the whole retry transcript.
fn compact_provider_error(s: &str) -> String {
    if !s.contains("All providers/models failed") {
        return s.to_string();
    }
    let attempts: Vec<&str> = s.lines().filter(|l| l.contains("attempt")).collect();
    let primary = attempts
        .first()
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| s.to_string());
    if attempts.len() > 1 {
        format!("{primary} (+{} more attempts)", attempts.len() - 1)
    } else {
        primary
    }
}

/// Route tracing output to a per-day log file under
/// `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log` so warnings from the agent
/// path don't corrupt the TUI's alt-screen render. Best-effort: any
/// failure (no HOME, can't create directory, etc.) silently falls back
/// to whatever subscriber is already installed (which in TUI mode is
/// usually nothing — i.e. tracing becomes a no-op).
///
/// Idempotent: if a global subscriber is already set, this is a no-op.
/// That makes the function safe to call from multiple entry points.
fn install_tui_tracing() {
    use tracing_subscriber::EnvFilter;

    // Resolve the log path. Use the rantaiclaw root so it lives next to
    // the user's other state, not buried under XDG cache.
    let log_dir = crate::profile::paths::rantaiclaw_root().join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let log_path = log_dir.join(format!("tui-{date}.log"));

    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .finish();

    // `set_global_default` returns Err if a subscriber is already set; we
    // happily swallow that — main()'s subscriber would have been less
    // appropriate for TUI mode anyway, but if it somehow ran first we
    // just leave it and accept the visual artifacts.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

// ---------------------------------------------------------------------------
// Free-standing render helpers used by `TuiApp::render`. They take parameter
// references so the closure can call them while `render` holds a disjoint
// `&mut self.autocomplete` borrow.
// ---------------------------------------------------------------------------

fn render_header_pane(ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let session_short = &ctx.session_id[..8.min(ctx.session_id.len())];
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "  Rantaiclaw  ",
            Style::default()
                .fg(Color::Rgb(4, 11, 46))
                .bg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}  ", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· session {} ", session_short),
            Style::default().fg(Color::Rgb(107, 114, 128)),
        ),
    ]));
    frame.render_widget(header, area);
}

fn render_chat_pane(state: &AppState, ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let theme = super::render::RenderTheme::default();
    let mut items: Vec<ListItem> = Vec::with_capacity(ctx.messages.len() + 1);

    if ctx.messages.is_empty() && !matches!(state, AppState::Streaming { .. }) {
        for line in render_splash_lines() {
            items.push(ListItem::new(line));
        }
    }

    for msg in &ctx.messages {
        let persisted = msg
            .tool_calls
            .as_deref()
            .map(super::render::parse_persisted_tool_calls)
            .unwrap_or_default();
        let lines =
            super::render::render_message_lines(&msg.role, &msg.content, &persisted, &[], &theme);
        items.push(ListItem::new(lines));
    }

    if let AppState::Streaming {
        partial,
        tool_blocks,
        ..
    } = state
    {
        let frame_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as usize / 100)
            .unwrap_or(0))
            % SPINNER_FRAMES.len();
        let spinner = SPINNER_FRAMES[frame_idx];
        let header = Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("thinking…", Style::default().fg(Color::Rgb(107, 114, 128))),
        ]);
        items.push(ListItem::new(header));
        let lines =
            super::render::render_message_lines("assistant", partial, &[], tool_blocks, &theme);
        items.push(ListItem::new(lines));
    }

    let list = List::new(items).block(
        Block::default()
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Chat",
                    Style::default()
                        .fg(Color::Rgb(94, 184, 255))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Rgb(40, 70, 140))),
    );
    frame.render_widget(list, area);
}

/// Commit a block of `Line`s to the terminal scrollback above the inline
/// viewport. Splits across multiple `insert_before` calls so we never
/// exceed ratatui's max-insert-height (terminal height − viewport height),
/// which otherwise panics with `index outside of buffer`.
fn commit_lines_to_scrollback(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    lines: Vec<Line<'static>>,
    width: u16,
    term_height: u16,
) -> Result<()> {
    if lines.is_empty() || width == 0 {
        return Ok(());
    }
    // Max rows we can safely reserve in one insert_before call. ratatui
    // requires this to fit between the top of the screen and the top of
    // the inline viewport; leave a 1-row buffer for safety.
    let max_chunk: u16 = term_height
        .saturating_sub(INLINE_VIEWPORT_LINES)
        .saturating_sub(1)
        .max(1);

    let mut buf: Vec<Line<'static>> = Vec::new();
    let mut buf_rows: u16 = 0;

    let flush = |terminal: &mut Terminal<CrosstermBackend<Stdout>>,
                 chunk: &mut Vec<Line<'static>>,
                 rows: &mut u16|
     -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let p = Paragraph::new(std::mem::take(chunk)).wrap(Wrap { trim: false });
        let height = (*rows).max(1);
        *rows = 0;
        terminal.insert_before(height, |b: &mut Buffer| {
            p.render(b.area, b);
        })?;
        Ok(())
    };

    for line in lines {
        // Estimate how many wrapped rows this line takes at `width`.
        // Use Paragraph::line_count for accuracy.
        let single = Paragraph::new(vec![line.clone()]).wrap(Wrap { trim: false });
        let row_count = single.line_count(width).max(1) as u16;

        // If a single line is taller than the chunk limit (extreme cases
        // like a 300-col line on a tall narrow terminal), cap it — we'd
        // rather lose tail rows than panic.
        let row_count = row_count.min(max_chunk);

        if buf_rows + row_count > max_chunk {
            flush(terminal, &mut buf, &mut buf_rows)?;
        }
        buf.push(line);
        buf_rows += row_count;
    }
    flush(terminal, &mut buf, &mut buf_rows)?;
    Ok(())
}

/// Render the live "stream preview" pane — sits above the input box and
/// shows the still-uncommitted tail of the assistant's reply as it
/// arrives. Empty when not streaming. The first row also shows a Braille
/// spinner so the user has motion to look at while bytes accumulate.
fn render_stream_preview_pane(
    state: &AppState,
    committed_chars: usize,
    frame: &mut ratatui::Frame,
    area: Rect,
) {
    let (partial, cancelling) = match state {
        AppState::Streaming {
            partial,
            cancelling,
            ..
        } => (partial.clone(), *cancelling),
        _ => return,
    };

    let frame_idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as usize / 100)
        .unwrap_or(0))
        % SPINNER_FRAMES.len();
    let spinner = SPINNER_FRAMES[frame_idx];
    let label = if cancelling {
        "cancelling…"
    } else if partial.len() == committed_chars {
        "thinking…"
    } else {
        "streaming…"
    };
    let muted = Style::default().fg(Color::Rgb(107, 114, 128));
    let sky = Style::default()
        .fg(Color::Rgb(94, 184, 255))
        .add_modifier(Modifier::BOLD);

    // Single-row layout: `[spinner] streaming…   <last few words of the
    // in-progress line>`. Completed lines flow into scrollback so the
    // preview never grows beyond one row — keeps the viewport tight and
    // matches Hermes / Claude Code feel.
    let safe_committed = committed_chars.min(partial.len());
    let tail = &partial[safe_committed..];
    let last_line = tail.rsplit('\n').next().unwrap_or("");
    let snippet = if last_line.chars().count() > 60 {
        let total = last_line.chars().count();
        let skip = total.saturating_sub(58);
        let suffix: String = last_line.chars().skip(skip).collect();
        format!("…{suffix}")
    } else {
        last_line.to_string()
    };

    let mut spans = vec![
        Span::styled(format!("  {spinner} "), sky),
        Span::styled(label.to_string(), muted),
    ];
    if !snippet.trim().is_empty() {
        spans.push(Span::styled("    ".to_string(), muted));
        spans.push(Span::styled(
            snippet,
            Style::default().fg(Color::Rgb(180, 200, 220)),
        ));
    } else {
        spans.push(Span::styled("    Ctrl+C to cancel".to_string(), muted));
    }
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

fn render_input_pane(ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let prefix = Span::styled(
        "▎ ",
        Style::default()
            .fg(Color::Rgb(94, 184, 255))
            .add_modifier(Modifier::BOLD),
    );
    let body = if ctx.input_buffer.is_empty() {
        Span::styled(
            "Type a message…  (Enter sends · /help for commands · Ctrl+J newline · Ctrl+C exit)",
            Style::default().fg(Color::Rgb(107, 114, 128)),
        )
    } else {
        Span::raw(ctx.input_buffer.clone())
    };

    let input = Paragraph::new(Line::from(vec![prefix, body]))
        .block(
            Block::default()
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        "$ you",
                        Style::default()
                            .fg(Color::Rgb(94, 184, 255))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(94, 184, 255))),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
}

fn render_status_pane(ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let muted = Style::default().fg(Color::Rgb(107, 114, 128));
    let sky = Style::default().fg(Color::Rgb(94, 184, 255));
    let coral = Style::default()
        .fg(Color::Rgb(255, 123, 123))
        .add_modifier(Modifier::BOLD);

    let line = if let Some(ref err) = ctx.last_error {
        Line::from(vec![
            Span::styled(" ✗ ", coral),
            Span::styled(err.clone(), Style::default().fg(Color::Rgb(255, 123, 123))),
        ])
    } else {
        let used = ctx.token_usage.total_tokens;
        let used_label = format_tokens(used);
        let window = ctx.context_window.unwrap_or(0);
        let window_label = if window > 0 {
            format!("/{}", format_tokens(window))
        } else {
            String::new()
        };
        let pct = if window > 0 {
            ((used as f64 / window as f64) * 100.0).round() as u32
        } else {
            0
        };
        let age_secs = ctx.started_at.elapsed().as_secs();
        let age_label = format_duration_short(age_secs);

        Line::from(vec![
            Span::styled(" $ ", sky),
            Span::styled(
                ctx.model.clone(),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  │  ", muted),
            Span::styled(
                format!("{used_label}{window_label}"),
                Style::default().fg(Color::Rgb(126, 226, 179)),
            ),
            Span::styled(
                if window > 0 {
                    format!("  {pct}%")
                } else {
                    String::new()
                },
                muted,
            ),
            Span::styled("  │  ", muted),
            Span::styled(format!("{} msgs", ctx.messages.len()), muted),
            Span::styled("  │  ", muted),
            Span::styled(age_label, muted),
        ])
    };

    let status = Paragraph::new(line);
    frame.render_widget(status, area);
}

/// Render the splash banner + welcome lines as ratatui `Line`s for the
/// empty-chat state. Pulls the same assets the CLI splash uses, colored
/// by the brand gradient.
fn render_splash_lines() -> Vec<Line<'static>> {
    let banner = include_str!("../onboard/assets/banner_full.txt");
    let mut out: Vec<Line<'static>> = Vec::new();
    let palette = [
        Color::Rgb(94, 184, 255),  // sky
        Color::Rgb(94, 184, 255),  // sky
        Color::Rgb(59, 140, 255),  // blue
        Color::Rgb(59, 140, 255),  // blue
        Color::Rgb(40, 70, 140),   // navy bright
        Color::Rgb(107, 114, 128), // muted
    ];
    for (i, line) in banner.lines().enumerate() {
        let color = palette[i.min(palette.len() - 1)];
        out.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }
    out.push(Line::from(""));
    out.push(Line::from(vec![
        Span::styled(
            "  Welcome to Rantaiclaw. ",
            Style::default()
                .fg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Type a message or /help for commands.",
            Style::default().fg(Color::Rgb(107, 114, 128)),
        ),
    ]));
    out.push(Line::from(""));
    out
}

/// Format token counts with K / M suffixes so the status bar stays compact.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Render the modal `/help`-style overlay over the chat area. Layout
/// mirrors Claude Code's:
///   ┌ Rantaiclaw v0.5.0  [general]  commands ───────────┐
///   │                                                   │
///   │ <body of active tab>                              │
///   │                                                   │
///   │                                Esc to close       │
///   └───────────────────────────────────────────────────┘
fn render_overlay_pane(
    content: &super::commands::OverlayContent,
    frame: &mut ratatui::Frame,
    area: Rect,
) {
    use ratatui::widgets::{Clear, Paragraph};

    if area.height < 5 || area.width < 30 {
        // Terminal too small for the overlay; fall back to silently
        // skipping the panel — the user can still see the chat behind it
        // and dismiss with Esc.
        return;
    }

    // Center the panel — leave a 2-col gutter on each side.
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    let panel_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: inner_w,
        height: inner_h,
    };

    // Draw a clean opaque background so chat content underneath doesn't
    // bleed through.
    frame.render_widget(Clear, panel_area);

    let sky = Color::Rgb(94, 184, 255);
    let blue = Color::Rgb(59, 140, 255);
    let muted = Color::Rgb(107, 114, 128);
    let active_bg = Color::Rgb(94, 184, 255);
    let frame_color = Color::Rgb(40, 70, 140);

    // Title spans the full width. Active tab gets a sky-blue chip; inactive
    // tabs are muted.
    let mut title_spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(
            content.title.clone(),
            Style::default().fg(sky).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    for (i, tab) in content.tabs.iter().enumerate() {
        if i == content.active_tab {
            title_spans.push(Span::styled(
                format!(" {} ", tab.label),
                Style::default()
                    .fg(Color::Rgb(4, 11, 46))
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            title_spans.push(Span::styled(
                format!(" {} ", tab.label),
                Style::default().fg(muted),
            ));
        }
        title_spans.push(Span::raw(" "));
    }

    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color));

    let body_lines: Vec<Line> = content
        .tabs
        .get(content.active_tab)
        .map(|t| {
            let mut lines: Vec<Line> = Vec::with_capacity(t.body.len() + 2);
            for raw in &t.body {
                if raw.is_empty() {
                    lines.push(Line::from(""));
                    continue;
                }
                // Section header heuristic: line that doesn't start with
                // whitespace and ends without colon (or is short) — bolden
                // it as a category label.
                let is_section = !raw.starts_with(' ')
                    && !raw.contains("://")
                    && raw.split_whitespace().count() <= 4;
                if is_section {
                    lines.push(Line::from(Span::styled(
                        raw.clone(),
                        Style::default().fg(blue).add_modifier(Modifier::BOLD),
                    )));
                } else {
                    // Inside the body lines, bullet rows are coloured to make
                    // command names + key bindings pop without overdoing it.
                    lines.push(highlight_help_line(raw, sky, muted));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "  Esc",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to close · ", Style::default().fg(muted)),
                Span::styled(
                    "Tab / ← →",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to switch tabs", Style::default().fg(muted)),
            ]));
            lines
        })
        .unwrap_or_default();

    let body = Paragraph::new(body_lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, panel_area);
}

/// Color the leading token of a help-body line — command names (`/foo`)
/// or shortcut keys (`Ctrl+C`) get the sky accent, the rest stays muted.
fn highlight_help_line(raw: &str, sky: Color, muted: Color) -> Line<'static> {
    // Line shape we expect: `  /command   description...` or
    // `  Ctrl+X    description...` or just `  • text...`. We split on the
    // first run of >=2 spaces.
    let trimmed = raw.trim_start();
    let leading = raw.len() - trimmed.len();
    // Find the first "double-space" gap that separates the keyword from
    // the description.
    if let Some(gap) = trimmed.find("  ") {
        let keyword = &trimmed[..gap];
        let rest = trimmed[gap..].trim_start();
        Line::from(vec![
            Span::raw(" ".repeat(leading)),
            Span::styled(
                keyword.to_string(),
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(rest.to_string(), Style::default().fg(muted)),
        ])
    } else {
        Line::from(Span::styled(raw.to_string(), Style::default().fg(muted)))
    }
}

/// Format a duration in seconds as a compact `1h2m` / `34m` / `12s` label.
fn format_duration_short(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// Lines reserved at the bottom of the terminal for the live inline
/// viewport (status bar + input box + spinner room). Everything else flows
/// into the terminal's native scrollback.
/// Inline viewport height. Reverted to a tight 6-row layout because
/// any static "leave room for the dropdown below input" approach
/// in ratatui's inline mode leaves visible blank rows in scrollback.
/// Dropdown renders ABOVE the input within these 6 rows.
pub const INLINE_VIEWPORT_LINES: u16 = 6;
/// Rows reserved at the top of the inline viewport for the live streaming
/// preview (the still-uncommitted tail of the assistant's reply). Always
/// present so the viewport size doesn't need to resize between idle and
/// streaming states. Just one row — the spinner and current in-progress
/// snippet share it. Completed lines flow up into permanent scrollback.
pub const STREAM_PREVIEW_LINES: u16 = 1;

/// Set up the terminal in **inline mode** — no alternate screen takeover.
///
/// The bottom `INLINE_VIEWPORT_LINES` rows of the terminal are reserved
/// for the TUI's live region (status bar + input box). Everything emitted
/// via `terminal.insert_before(...)` or plain `println!` lands in the
/// terminal's normal scrollback above that region. On exit the viewport
/// is consumed and the terminal returns to its prompt.
///
/// This is the Hermes / Claude-Code-style flow: chat history is the
/// terminal's own scrollback, not a ratatui List widget that fights it.
pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(INLINE_VIEWPORT_LINES),
        },
    )?;
    Ok(terminal)
}

/// Suspend the TUI, hand control to `$EDITOR` (or `EDITOR`/`VISUAL`,
/// falling back to `nano`/`vi`/`notepad`), and copy the resulting
/// file contents back into `app.context.input_buffer` on success.
///
/// Best-effort: on any error (no editor on PATH, editor exited
/// non-zero, file IO failure) the original buffer is preserved and a
/// status-bar message surfaces the cause. Always restores raw mode
/// before returning so the caller can resume drawing.
fn run_external_editor(
    app: &mut TuiApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    use std::io::Read as _;
    use std::process::Command;

    // Resolve editor command: $EDITOR > $VISUAL > nano > vi > notepad.
    let editor_cmd = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "notepad".to_string()
            } else if which_program("nano") {
                "nano".to_string()
            } else {
                "vi".to_string()
            }
        });

    // Write current buffer to a temp file; the editor edits in place.
    // Use a unique filename in the OS temp dir (no extra dep needed).
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = std::env::temp_dir().join(format!("rantaiclaw-prompt-{pid}-{nonce}.md"));
    std::fs::write(&tmp_path, &app.context.input_buffer)?;

    // Suspend the TUI: flush, leave alt-screen if needed, drop raw mode.
    let was_fullscreen = app.list_picker.is_some();
    if was_fullscreen {
        execute!(io::stdout(), LeaveAlternateScreen)?;
    }
    let _ = terminal.flush();
    disable_raw_mode()?;
    let _ = io::stdout().flush();

    // Run the editor. Inherit stdio so the user sees / interacts with it.
    let mut parts = editor_cmd.split_whitespace();
    let bin = parts.next().unwrap_or("vi");
    let args: Vec<&str> = parts.collect();
    let status = Command::new(bin).args(&args).arg(&tmp_path).status();

    // Always restore raw mode + alt-screen (if we were in it).
    enable_raw_mode()?;
    if was_fullscreen {
        execute!(io::stdout(), EnterAlternateScreen)?;
        terminal.clear()?;
    } else {
        // Inline mode: re-claim a fresh terminal so the inline viewport
        // is re-laid-out cleanly after the editor printed to the tty.
        *terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(INLINE_VIEWPORT_LINES),
            },
        )?;
    }

    let result = match status {
        Ok(s) if s.success() => {
            let mut buf = String::new();
            std::fs::File::open(&tmp_path).and_then(|mut f| f.read_to_string(&mut buf))?;
            if buf.ends_with('\n') {
                buf.pop();
            }
            app.context.input_buffer = buf;
            app.context.exit_history_navigation();
            Ok(())
        }
        Ok(s) => {
            app.context.last_error = Some(format!(
                "editor exited with status {} — buffer unchanged",
                s.code().unwrap_or(-1)
            ));
            Ok(())
        }
        Err(e) => {
            app.context.last_error = Some(format!("editor '{bin}' failed to launch: {e}"));
            Ok(())
        }
    };
    // Best-effort cleanup; file is in $TMPDIR so leftovers are harmless.
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Cheap PATH check so we can prefer `nano` over `vi` when available.
fn which_program(name: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

/// Swap the terminal into alt-screen / fullscreen mode. Used while a
/// list picker is open so it can claim the entire terminal height
/// instead of fighting for space inside the 6-row inline viewport.
/// The original scrollback is preserved by the terminal emulator and
/// restored automatically on `swap_to_inline`.
pub fn swap_to_fullscreen(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let _ = terminal.flush();
    execute!(io::stdout(), EnterAlternateScreen)?;
    *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    Ok(())
}

/// Swap the terminal back to the inline 6-row viewport after leaving the
/// alt-screen picker. The inline viewport is re-created fresh; existing
/// terminal scrollback (committed via `insert_before` before the picker
/// opened) is automatically restored by the terminal when alt-screen is
/// left.
pub fn swap_to_inline(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let _ = terminal.flush();
    execute!(io::stdout(), LeaveAlternateScreen)?;
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(INLINE_VIEWPORT_LINES),
        },
    )?;
    Ok(())
}

/// Restore the terminal to its original state. Inline mode means no
/// alt-screen to leave; we just flush the viewport (so the cursor lands
/// below it cleanly) and disable raw mode.
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    // Move cursor to a fresh line below the viewport so the user's shell
    // prompt doesn't print on top of our last frame.
    terminal.clear()?;
    let _ = terminal.show_cursor();
    disable_raw_mode()?;
    let _ = io::stdout().flush();
    println!();
    Ok(())
}

/// Entry point for the TUI: guards TTY, builds the `Agent` + actor bridge,
/// runs the event loop, then shuts the actor down cleanly.
///
/// The TUI talks to the `Agent` exclusively through an mpsc bridge:
///   * `req_tx`/`req_rx` — TUI -> actor (`TurnRequest::Submit`/`Cancel`)
///   * `events_tx`/`events_rx` — actor -> TUI (`AgentEvent` stream)
///
/// Config is loaded here (rather than passed in) to avoid the binary/library
/// `Config` duplication that results from the bin+lib sharing `src/config/`.
/// `Agent::from_config` lives in the library crate, so it must receive the
/// library-side `Config`, which we obtain via the library's own loader.
///
/// On exit we drop the `TuiApp` (which releases `req_tx`), giving the actor
/// `None` from `req_rx.recv()` so it can finish its current turn and return.
/// A bounded timeout avoids hanging shutdown if the actor is stuck.
pub async fn run_tui(tui_config: TuiConfig) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("TUI requires an interactive terminal (stdin is not a TTY)");
    }

    // Install a file-writing tracing subscriber BEFORE we touch any code
    // that might emit logs. Without this, tracing falls through to the
    // default-stderr writer, and `tracing::warn!` calls from the agent's
    // provider/retry path bleed straight into the TUI's alt-screen frame.
    // Best-effort: failure to set it up just falls back to stderr (which
    // is rare in practice and worth knowing about).
    install_tui_tracing();

    let mut app_config = crate::config::Config::load_or_init().await?;
    app_config.apply_env_overrides();

    // If TuiConfig still has its compile-time default (no /model flag was
    // passed), surface the active config's provider:model so the status
    // bar reflects what the agent will actually use.
    let mut tui_config = tui_config;
    if tui_config.model == TuiConfig::default().model {
        let provider = app_config.default_provider.clone().unwrap_or_default();
        let model = app_config.default_model.clone().unwrap_or_default();
        if !provider.is_empty() && !model.is_empty() {
            tui_config.model = format!("{}:{}", provider, model);
        } else if !model.is_empty() {
            tui_config.model = model;
        }
    }

    let agent = Agent::from_config(&app_config)?;

    let profile =
        crate::profile::ProfileManager::active().unwrap_or_else(|_| crate::profile::Profile {
            name: "default".to_string(),
            root: crate::profile::paths::profile_dir("default"),
        });

    // Channel capacities are intentionally small on the request side (user
    // input is human-paced) and larger on the event side (streaming chunks
    // burst quickly per turn).
    let (req_tx, req_rx) = mpsc::channel::<TurnRequest>(16);
    let (events_tx, events_rx): (AgentEventSender, mpsc::Receiver<AgentEvent>) = mpsc::channel(128);

    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let actor_handle = tokio::spawn(actor.run());

    // Compute the list of providers visible to /model. Starts with the
    // primary `default_provider`, then any unique providers used by
    // `model_routes`. This is the "enabled" set surfaced in the picker.
    let mut available_providers: Vec<String> = Vec::new();
    if let Some(p) = app_config.default_provider.clone() {
        available_providers.push(p);
    }
    for route in &app_config.model_routes {
        if !available_providers.iter().any(|p| p == &route.provider) {
            available_providers.push(route.provider.clone());
        }
    }

    let mut app = TuiApp::new(
        &tui_config,
        app_config.clone(),
        profile.clone(),
        req_tx,
        events_rx,
    )?;
    app.context.available_providers = available_providers;

    if let Some(topic) = tui_config.setup_provisioner.take() {
        // `rantaiclaw setup` (no topic) and `rantaiclaw setup full` both
        // boot the first-run wizard — the canonical "set everything up"
        // entry point. Named topics route to the overlay for that one
        // provisioner; unknown names surface the existing error.
        if topic.is_empty() || topic.eq_ignore_ascii_case("full") {
            app.first_run_wizard = Some(crate::tui::FirstRunWizard::new(profile.clone()));
        } else if let Err(e) = app.open_setup_overlay(topic) {
            let msg = format!("Failed to open setup: {}", e);
            let _ = app.context.append_system_message(&msg);
            app.scrollback_queue.push(("system".to_string(), msg));
        }
    } else if app_config.api_key.is_none() && app_config.default_provider.is_none() {
        app.first_run_wizard = Some(crate::tui::FirstRunWizard::new(profile.clone()));
    }

    // Idempotent first-run skill seeding: if the workspace skills dir
    // is empty, drop the 5-skill starter pack (web-search, summarizer,
    // research-assistant, scheduler-reminders, meeting-notes) so the
    // /skills picker has real content out of the box. Mirrors Hermes'
    // bundled-skills-on-install UX. Best-effort — failure is logged but
    // doesn't block startup.
    let _ = crate::skills::bundled::install_starter_pack(&profile);
    // Load skills from the active workspace so /skills can browse them.
    // load_skills_with_config falls back to an empty vec on any error
    // (missing dir, malformed manifest, etc.) — never blocks startup.
    app.context.available_skills =
        crate::skills::load_skills_with_config(&app_config.workspace_dir, &app_config);
    let mut terminal = setup_terminal()?;

    // Splash banner — committed once to the terminal's scrollback before
    // the inline viewport takes over. Becomes permanent history above the
    // status bar / input region.
    let _ = TuiApp::commit_splash_to_scrollback(&mut terminal, &app.context);

    let result = run_loop(&mut app, &mut terminal).await;

    // Always restore terminal, even on error.
    let restore_result = restore_terminal(&mut terminal);

    // Drop the app so the actor's req_rx sees all senders gone and exits.
    drop(app);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), actor_handle).await;

    // Surface the loop error first, then the restore error.
    result?;
    restore_result?;

    Ok(())
}

/// Inner event loop separated from terminal lifecycle for easier testing.
async fn run_loop(
    app: &mut TuiApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    // Temporary fullscreen Terminal instantiated only while a list
    // picker is open. The inline `terminal` (parameter) is NEVER
    // dropped/recreated during alt-screen swaps — that's what caused
    // the scrollback whitespace bug. Instead we send the alt-screen
    // escape and use a separate Fullscreen Terminal sharing stdout for
    // the duration. Returning to inline just drops `alt` and emits
    // LeaveAlternateScreen — the original screen state (including the
    // inline viewport rows) is restored by the terminal emulator.
    let mut alt: Option<Terminal<CrosstermBackend<Stdout>>> = None;
    loop {
        // Drain any buffered agent events before rendering so the frame
        // reflects the latest streaming state.
        app.drain_events();

        // Alt-screen entry/exit covers TWO triggers — list picker open
        // OR slash-autocomplete dropdown visible. Edge-triggered via
        // option presence so we don't churn buffers on every keystroke.
        let want_alt = app.list_picker.is_some()
            || app.autocomplete.is_visible()
            || app.setup_overlay.is_some()
            || app.first_run_wizard.is_some();
        let in_alt = alt.is_some();
        if want_alt && !in_alt {
            execute!(io::stdout(), EnterAlternateScreen)?;
            let mut t = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            t.clear()?;
            alt = Some(t);
        } else if !want_alt && in_alt {
            // Drop the temp fullscreen terminal first so its final flush
            // happens INSIDE alt-screen, then leave alt-screen, then
            // force the inline terminal to repaint cleanly on top of
            // the restored screen.
            drop(alt.take());
            execute!(io::stdout(), LeaveAlternateScreen)?;
            terminal.clear()?;
        }

        // /new and /clear request a full screen+scrollback wipe so
        // the next session starts on a clean terminal — same intent
        // as running `clear` at the shell. ESC[3J clears the xterm
        // scrollback buffer; ESC[2J clears the visible screen; ESC[H
        // homes the cursor. Then re-claim a fresh inline viewport and
        // re-print the splash banner so the user lands on the same
        // welcome screen as a cold launch (`./rantaiclaw`).
        if app.clear_terminal_request && alt.is_none() {
            app.clear_terminal_request = false;
            let _ = terminal.flush();
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[3J\x1b[2J\x1b[H");
            let _ = out.flush();
            *terminal = Terminal::with_options(
                CrosstermBackend::new(io::stdout()),
                TerminalOptions {
                    viewport: Viewport::Inline(INLINE_VIEWPORT_LINES),
                },
            )?;
            let _ = TuiApp::commit_splash_to_scrollback(terminal, &app.context);
        }

        // Inline-only: stream completed lines into scrollback and flush
        // any queued message commits ABOVE the viewport. Skipped while
        // the picker has us in alt-screen — those commits would write
        // into the alt buffer and be lost when we return.
        if alt.is_none() {
            app.flush_stream_to_scrollback(terminal)?;
            let pending: Vec<(String, String)> = std::mem::take(&mut app.scrollback_queue);
            for (role, content) in pending {
                TuiApp::commit_message_to_scrollback(terminal, &role, &content)?;
            }
        }

        if let Some(ref mut alt_term) = alt {
            // While in alt-screen, render whichever overlay drove us in.
            // Setup overlay takes precedence (it's the most modal — owns
            // a provisioner task), then picker, then autocomplete.
            if app.first_run_wizard.is_some() {
                alt_term.draw(|frame| {
                    let area = frame.area();
                    if let Some(w) = app.first_run_wizard.as_ref() {
                        w.render_fullscreen(frame, area);
                    }
                })?;
            } else if app.setup_overlay.is_some() {
                alt_term.draw(|frame| {
                    let area = frame.area();
                    if let Some(o) = app.setup_overlay.as_mut() {
                        o.render(frame, area);
                    }
                })?;
            } else if app.list_picker.is_some() {
                app.render_fullscreen_picker(alt_term)?;
            } else {
                app.render_fullscreen_autocomplete(alt_term)?;
            }
        } else {
            app.render(terminal)?;
        }

        // Tighten the poll interval during streaming so the live preview
        // updates fast enough to feel like word-by-word streaming. When
        // idle, poll less aggressively to keep CPU near zero.
        let poll_ms = if matches!(app.state, AppState::Streaming { .. }) {
            16
        } else {
            100
        };
        if event::poll(std::time::Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            // Inline mode pins a 6-row viewport to the bottom of the
            // terminal. When the terminal is resized, ratatui repaints
            // the viewport at the new bottom row, but the previous
            // viewport's rows remain in the terminal buffer above as
            // ghost copies. Drain any coalesced Resize events, then
            // wipe the screen+scrollback and replay splash + messages
            // so the terminal looks like a fresh launch at the new
            // size. While in alt-screen the picker handles its own
            // sizing; just trigger a repaint there.
            if matches!(ev, Event::Resize(_, _)) {
                while event::poll(std::time::Duration::from_millis(0))? {
                    let next = event::read()?;
                    if !matches!(next, Event::Resize(_, _)) {
                        match app.handle_event(next).await? {
                            EventResult::Quit => break,
                            EventResult::Continue => {}
                        }
                        break;
                    }
                }
                if alt.is_none() {
                    let _ = terminal.flush();
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x1b[3J\x1b[2J\x1b[H");
                    let _ = out.flush();
                    *terminal = Terminal::with_options(
                        CrosstermBackend::new(io::stdout()),
                        TerminalOptions {
                            viewport: Viewport::Inline(INLINE_VIEWPORT_LINES),
                        },
                    )?;
                    let _ = TuiApp::commit_splash_to_scrollback(terminal, &app.context);
                    let messages = app.context.messages.clone();
                    for msg in messages {
                        let _ =
                            TuiApp::commit_message_to_scrollback(terminal, &msg.role, &msg.content);
                    }
                } else {
                    let _ = terminal.clear();
                }
            } else {
                match app.handle_event(ev).await? {
                    EventResult::Quit => break,
                    EventResult::Continue => {}
                }
            }
        }

        // Handle deferred editor request raised by Ctrl+G. Done after
        // event handling so the buffer reflects the user's latest
        // keystrokes, and before the next render so the new contents
        // appear on the next frame.
        if app.editor_request {
            app.editor_request = false;
            if let Err(e) = run_external_editor(app, terminal) {
                app.context.last_error = Some(format!("editor flow error: {e}"));
            }
        }

        if matches!(app.state, AppState::Quitting) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod error_format_tests {
    use super::format_agent_error;

    #[test]
    fn api_key_missing_gets_friendly_chat_body() {
        let raw = "All providers/models failed. Attempts:\nprovider=openrouter model=anthropic/claude-sonnet-4.6 attempt 1/3: non_retryable; error=OpenRouter API key not set. Run `rantaiclaw onboard` or set OPENROUTER_API_KEY env var.";
        let (chat, status) = format_agent_error(raw);
        assert!(chat.contains("Missing API key"));
        assert!(chat.contains("OPENROUTER_API_KEY"));
        assert!(!chat.contains("attempt 1/3"));
        assert!(status.contains("missing API key"));
    }

    #[test]
    fn rate_limit_gets_short_message() {
        let raw = "All providers/models failed. Attempts:\nprovider=openrouter model=x attempt 1/3: rate-limited; HTTP 429";
        let (chat, status) = format_agent_error(raw);
        assert!(chat.contains("Rate-limited"));
        assert!(status.contains("rate limit"));
    }

    #[test]
    fn unknown_error_compacts_attempts_tail() {
        let raw = "All providers/models failed. Attempts:\nprovider=p1 model=m attempt 1/3: foo\nprovider=p2 model=m attempt 1/3: bar";
        let (chat, _status) = format_agent_error(raw);
        // Compacted to first attempt plus +N more rather than full transcript.
        assert!(chat.contains("(+1 more attempts)"), "got {chat:?}");
    }

    #[test]
    fn non_provider_error_passes_through_with_prefix() {
        let (chat, _) = format_agent_error("something else broke");
        assert!(chat.starts_with("✗ something else broke"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionStore;
    use crate::tui::context::TuiContext;

    fn make_app_from_store(store: SessionStore, model: &str) -> TuiApp {
        // Tests that hit this helper do not exercise the bridge; the request
        // receiver and events sender are held locally so the TUI-side ends
        // stay valid for the duration of the test.
        let (req_tx, _req_rx) = tokio::sync::mpsc::channel(4);
        let (_events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let ctx = TuiContext::new(store, model, None, req_tx, events_rx).expect("context");
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
        }
    }

    #[test]
    fn app_creates_new_session_on_start() {
        let store = SessionStore::in_memory().expect("store");
        let app = make_app_from_store(store, "test-model");

        assert!(!app.context.session_id.is_empty());
        assert!(matches!(app.state, AppState::Ready));
        assert!(app.context.messages.is_empty());
    }

    #[tokio::test]
    async fn app_handles_quit_command() {
        let store = SessionStore::in_memory().expect("store");
        let mut app = make_app_from_store(store, "test-model");

        app.context.input_buffer = "/quit".to_string();
        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Quitting));
    }

    #[tokio::test]
    async fn app_handles_new_command() {
        let store = SessionStore::in_memory().expect("store");
        let mut app = make_app_from_store(store, "test-model");

        let first_session_id = app.context.session_id.clone();

        // A non-command submit now dispatches via the bridge and transitions
        // the app to Streaming (no real actor in this test — the request
        // simply sits in the channel). The user message is still appended
        // locally, which is what this test originally covered.
        app.context.input_buffer = "hello".to_string();
        app.submit_input().await.unwrap();
        assert!(!app.context.messages.is_empty());

        app.context.input_buffer = "/new".to_string();
        app.submit_input().await.unwrap();

        // /new clears the chat, then appends a system "Started new session"
        // confirmation line — so messages == 1, not 0. The session id flips.
        assert_eq!(app.context.messages.len(), 1);
        assert_eq!(app.context.messages[0].role, "system");
        assert_ne!(app.context.session_id, first_session_id);
    }
}

#[cfg(test)]
mod submit_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
        }
    }

    #[tokio::test]
    async fn submit_input_ready_state_sends_request_and_transitions_to_streaming() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;
        app.context.input_buffer = "hello".into();

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.context.input_buffer, "");
        let req = req_rx.recv().await.expect("request should have been sent");
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "hello"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
        }
    }

    #[tokio::test]
    async fn submit_input_streaming_state_queues_and_increments_counter() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
        };
        app.context.input_buffer = "queued".into();

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.context.queued_turns, 1);
        let req = req_rx.recv().await.expect("request should have been sent");
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "queued"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
        }
    }

    #[tokio::test]
    async fn submit_input_empty_buffer_is_noop() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;
        app.context.input_buffer = "   ".into(); // whitespace only

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Ready));
        assert!(
            req_rx.try_recv().is_err(),
            "no request should have been sent for whitespace-only buffer"
        );
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
        }
    }

    #[tokio::test]
    async fn ctrl_c_in_streaming_sends_cancel_and_sets_cancelling_flag() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
        };

        let result = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(result, EventResult::Continue);
        let got = req_rx.recv().await.unwrap();
        assert!(matches!(got, TurnRequest::Cancel));
        if let AppState::Streaming { cancelling, .. } = &app.state {
            assert!(*cancelling);
        } else {
            panic!("state should remain Streaming during cancel");
        }
    }

    #[tokio::test]
    async fn ctrl_c_in_ready_state_transitions_to_quitting() {
        let (ctx, mut _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;

        let result = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(result, EventResult::Quit);
        assert!(matches!(app.state, AppState::Quitting));
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use crate::agent::events::AgentEvent;
    use crate::tui::context::TuiContext;

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
        }
    }

    #[tokio::test]
    async fn drain_events_chunk_appends_to_partial() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::from("prev "),
            tool_blocks: vec![],
            cancelling: false,
        };
        events_tx
            .send(AgentEvent::Chunk("more".into()))
            .await
            .unwrap();

        app.drain_events();

        if let AppState::Streaming { partial, .. } = &app.state {
            assert_eq!(partial, "prev more");
        } else {
            panic!("expected Streaming");
        }
    }

    #[tokio::test]
    async fn drain_events_done_finalizes_turn_to_ready() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::from("answer"),
            tool_blocks: vec![],
            cancelling: false,
        };
        events_tx
            .send(AgentEvent::Done {
                final_text: "answer".into(),
                cancelled: false,
            })
            .await
            .unwrap();

        app.drain_events();

        assert!(matches!(app.state, AppState::Ready));
        assert!(!app.context.messages.is_empty());
        assert_eq!(app.context.messages.last().unwrap().content, "answer");
    }

    #[tokio::test]
    async fn drain_events_done_cancelled_appends_marker_using_local_partial() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::from("partial text from chunks"),
            tool_blocks: vec![],
            cancelling: true,
        };
        // Agent emits Done with empty final_text on cancel — TUI must use local partial.
        events_tx
            .send(AgentEvent::Done {
                final_text: String::new(),
                cancelled: true,
            })
            .await
            .unwrap();

        app.drain_events();

        assert!(matches!(app.state, AppState::Ready));
        let last = app.context.messages.last().unwrap();
        assert!(last.content.contains("partial text"));
        assert!(last.content.contains("[cancelled]"));
    }

    #[tokio::test]
    async fn drain_events_tool_call_start_end_updates_blocks() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
        };
        events_tx
            .send(AgentEvent::ToolCallStart {
                id: "call-1".into(),
                name: "shell".into(),
                args: serde_json::json!({"cmd":"ls"}),
            })
            .await
            .unwrap();
        events_tx
            .send(AgentEvent::ToolCallEnd {
                id: "call-1".into(),
                ok: true,
                output_preview: "files".into(),
            })
            .await
            .unwrap();

        app.drain_events();

        if let AppState::Streaming { tool_blocks, .. } = &app.state {
            assert_eq!(tool_blocks.len(), 1);
            assert_eq!(tool_blocks[0].name, "shell");
            assert_eq!(tool_blocks[0].result, Some((true, "files".into())));
        } else {
            panic!("expected Streaming");
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
        }
    }

    #[tokio::test]
    async fn retry_in_ready_resubmits_last_user_message_and_streams() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("previous prompt").unwrap();
        app.context.append_assistant_message("old reply").unwrap();
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        // The user message is retained; the old assistant reply is gone.
        assert_eq!(app.context.messages.len(), 1);
        assert_eq!(app.context.messages[0].content, "previous prompt");
        let req = req_rx.recv().await.expect("resubmit should dispatch");
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "previous prompt"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
        }
        assert!(app.context.last_error.is_none());
    }

    #[tokio::test]
    async fn retry_in_streaming_refuses_and_sets_last_error() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("prompt").unwrap();
        app.context.append_assistant_message("reply").unwrap();
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
        };

        app.handle_command("retry").await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert!(
            req_rx.try_recv().is_err(),
            "no request should be dispatched while streaming"
        );
        let err = app.context.last_error.as_deref().unwrap_or("");
        assert!(err.contains("Cannot retry"));
    }

    #[tokio::test]
    async fn retry_with_no_history_sets_message_and_stays_ready() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        assert!(matches!(app.state, AppState::Ready));
        assert!(req_rx.try_recv().is_err());
        // CommandResult::Message now appends as a system chat message
        // (instead of the single-line error slot) so multi-line content
        // renders properly.
        let last_msg = app.context.messages.last().expect("system message");
        assert_eq!(last_msg.role, "system");
        assert!(
            last_msg.content.contains("No previous response"),
            "got {:?}",
            last_msg.content
        );
    }
}
