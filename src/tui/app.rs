use std::io::{self, IsTerminal, Stdout};

use anyhow::{bail, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
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
    /// Slash-command dropdown — visible whenever the input buffer starts
    /// with `/`. Filtered by prefix on every keystroke.
    pub autocomplete: super::widgets::autocomplete::Autocomplete,
    /// Active modal overlay (e.g. /help). `None` = no overlay shown.
    /// Esc dismisses; left/right arrows cycle tabs.
    pub overlay: Option<super::commands::OverlayContent>,
}

impl TuiApp {
    /// Create a new `TuiApp`, starting or resuming a session based on config.
    ///
    /// `req_tx` and `events_rx` are the TUI-side ends of the bridge to the
    /// `TuiAgentActor`. The actor owns the paired `req_rx`/`events_tx` and is
    /// spawned by `run_tui` before the app is constructed.
    pub fn new(
        config: &TuiConfig,
        req_tx: mpsc::Sender<TurnRequest>,
        events_rx: mpsc::Receiver<AgentEvent>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let db_path = config.data_dir.join("sessions.db");
        let store = SessionStore::open(&db_path)?;

        let context = TuiContext::new(
            store,
            &config.model,
            config.resume_session.as_deref(),
            req_tx,
            events_rx,
        )?;

        Ok(Self {
            state: AppState::Ready,
            context,
            command_registry: CommandRegistry::new(),
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
        })
    }

    /// Re-evaluate the slash-command dropdown against the current input
    /// buffer. Called after every keystroke that mutates `input_buffer`.
    fn refresh_autocomplete(&mut self) {
        let buf = &self.context.input_buffer;
        if buf.starts_with('/') && !buf.contains(' ') && !buf.contains('\n') {
            let suggestions = self
                .command_registry
                .autocomplete_with_descriptions(buf);
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
            // Plain Enter:
            //   * On a slash-command buffer (`/foo …` on a single line) →
            //     submit, so users don't need Ctrl+Enter for `/help`.
            //   * Otherwise → newline (multi-line prompts still work).
            //   * If the autocomplete dropdown is open and the highlighted
            //     command differs from what the user typed, complete first
            //     and submit on the *next* Enter; if the highlight already
            //     matches, submit immediately.
            KeyCode::Enter => {
                let buf = self.context.input_buffer.trim_end_matches(' ');
                let is_single_line_slash = buf.starts_with('/') && !buf.contains('\n');
                if is_single_line_slash {
                    if self.autocomplete.is_visible() {
                        // If the user's typed text already matches the
                        // highlighted suggestion, just submit — they
                        // intended to fire it. Otherwise complete to the
                        // selection and let them confirm or add args.
                        let typed_cmd = buf.trim_end_matches(' ').to_string();
                        let selected = self
                            .autocomplete
                            .selected()
                            .map(|s| s.to_string());
                        if selected.as_deref() == Some(typed_cmd.as_str()) {
                            self.autocomplete.hide();
                            self.submit_input().await?;
                        } else {
                            self.complete_selected_command();
                        }
                    } else {
                        self.submit_input().await?;
                    }
                } else if self.autocomplete.is_visible() {
                    // Defensive: dropdown shouldn't be visible here, but if
                    // it is, treat Enter as completion to match user
                    // muscle memory.
                    self.complete_selected_command();
                } else {
                    self.context.input_buffer.push('\n');
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
            KeyCode::Backspace => {
                self.context.input_buffer.pop();
                self.refresh_autocomplete();
            }
            // Regular character input
            KeyCode::Char(c) => {
                self.context.input_buffer.push(c);
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
            KeyCode::Up => {
                self.context.scroll_offset = self.context.scroll_offset.saturating_add(1);
            }
            // Scroll down
            KeyCode::Down => {
                self.context.scroll_offset = self.context.scroll_offset.saturating_sub(1);
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

        // Slash-commands bypass the bridge entirely (except `/retry`
        // which dispatches via `handle_command` → `dispatch_resubmit`).
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let cmd = cmd.trim().to_string();
            self.handle_command(&cmd).await?;
            self.context.scroll_offset = 0;
            return Ok(());
        }

        let text = trimmed.to_string();
        self.context.append_user_message(&text)?;

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

        if self.context.queued_turns > 0 {
            self.context.queued_turns -= 1;
            self.state = AppState::Streaming {
                partial: String::new(),
                tool_blocks: Vec::new(),
                cancelling: false,
            };
        } else {
            self.state = AppState::Ready;
        }
    }

    /// Finalize a turn on `AgentEvent::Error`. Surfaces the error as a
    /// visible assistant message (so it shows up in chat history) AND sets
    /// `last_error` so the status bar reflects it until cleared.
    fn finalize_error(&mut self, msg: String) {
        let body = format!("[error] {msg}");
        if let Err(e) = self.context.append_assistant_message(&body) {
            // If persistence fails, prefer reporting the persistence error —
            // the original error is already in `last_error` below.
            self.context.last_error = Some(format!("failed to persist error: {e}"));
        } else {
            self.context.last_error = Some(msg);
        }
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
            }
            CmdResult::Overlay(content) => {
                self.overlay = Some(content);
            }
            CmdResult::Continue | CmdResult::ClearError => {
                self.context.last_error = None;
            }
            CmdResult::Resubmit(text) => {
                self.dispatch_resubmit(text).await;
            }
        }
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

    /// Render the full UI into the terminal frame. Uses a disjoint-field
    /// borrow so `&mut self.autocomplete` (needed for `ListState`) coexists
    /// with `&self.context` / `&self.state` borrows for the other panes.
    pub fn render(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let TuiApp {
            state,
            context,
            autocomplete,
            overlay,
            ..
        } = self;

        terminal.draw(|frame| {
            let area = frame.area();

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // header
                    Constraint::Min(3),    // chat
                    Constraint::Length(5), // input
                    Constraint::Length(1), // status
                ])
                .split(area);

            render_header_pane(context, frame, chunks[0]);
            render_chat_pane(state, context, frame, chunks[1]);
            render_input_pane(context, frame, chunks[2]);
            render_status_pane(context, frame, chunks[3]);

            // Modal overlay — when open, occupies most of the chat area.
            // Drawn before the dropdown so the dropdown wins focus visually
            // if both are open (rare; the user closes one before toggling
            // the other).
            if let Some(content) = overlay.as_ref() {
                render_overlay_pane(content, frame, chunks[1]);
            }

            // Slash-command dropdown — anchored just above the input box,
            // grows upward into the chat area like a Hermes / Claude-Code
            // popup.
            if autocomplete.is_visible() {
                let input_area = chunks[2];
                let chat_area = chunks[1];
                // 8 rows max + 2 frame chars; clamp to available chat space.
                let max_rows = chat_area.height.saturating_sub(1).max(3) as usize;
                let popup_height =
                    ((max_rows + 2).min(10)) as u16;
                let popup_y = input_area.y.saturating_sub(popup_height);
                let popup_area = Rect {
                    x: input_area.x,
                    y: popup_y,
                    width: input_area.width,
                    height: popup_height,
                };
                autocomplete.render(frame, popup_area);
            }
        })?;
        Ok(())
    }

    /// Original `render_header` shape, kept for backward callers.
    #[allow(dead_code)]
    fn render_header(&self, frame: &mut ratatui::Frame, area: Rect) {
        let session_short = &self.context.session_id
            [..8.min(self.context.session_id.len())];
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
        if self.context.messages.is_empty()
            && !matches!(self.state, AppState::Streaming { .. })
        {
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
                Span::styled(
                    "thinking…",
                    Style::default().fg(Color::Rgb(107, 114, 128)),
                ),
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
        let lines = super::render::render_message_lines(
            &msg.role,
            &msg.content,
            &persisted,
            &[],
            &theme,
        );
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
            Span::styled(
                "thinking…",
                Style::default().fg(Color::Rgb(107, 114, 128)),
            ),
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

    let body = Paragraph::new(body_lines).block(block).wrap(Wrap { trim: false });
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

/// Set up the terminal for raw/alternate-screen mode.
pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
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

    let mut app_config = crate::config::Config::load_or_init().await?;
    app_config.apply_env_overrides();

    let agent = Agent::from_config(&app_config)?;

    // Channel capacities are intentionally small on the request side (user
    // input is human-paced) and larger on the event side (streaming chunks
    // burst quickly per turn).
    let (req_tx, req_rx) = mpsc::channel::<TurnRequest>(16);
    let (events_tx, events_rx): (AgentEventSender, mpsc::Receiver<AgentEvent>) = mpsc::channel(128);

    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let actor_handle = tokio::spawn(actor.run());

    let mut app = TuiApp::new(&tui_config, req_tx, events_rx)?;
    let mut terminal = setup_terminal()?;

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
    loop {
        // Drain any buffered agent events before rendering so the frame
        // reflects the latest streaming state.
        app.drain_events();

        app.render(terminal)?;

        if event::poll(std::time::Duration::from_millis(100))? {
            let ev = event::read()?;
            match app.handle_event(ev).await? {
                EventResult::Quit => break,
                EventResult::Continue => {}
            }
        }

        if matches!(app.state, AppState::Quitting) {
            break;
        }
    }
    Ok(())
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
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
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
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
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
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
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
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
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
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
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
