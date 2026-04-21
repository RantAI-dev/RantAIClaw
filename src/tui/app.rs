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
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};

use super::async_bridge::TurnRequest;
use super::commands::{CommandRegistry, CommandResult as CmdResult};
use super::context::TuiContext;
use super::TuiConfig;
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
}

impl TuiApp {
    /// Create a new `TuiApp`, starting or resuming a session based on config.
    pub fn new(config: &TuiConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let db_path = config.data_dir.join("sessions.db");
        let store = SessionStore::open(&db_path)?;

        // TODO(task-15): wire these channel ends to a real `TuiAgentActor`
        // spawned from `run_tui`. `submit_input` already dispatches
        // `TurnRequest`s on `req_tx`; until the actor is spawned, those
        // requests will sit unread in the channel.
        let (req_tx, _req_rx) = tokio::sync::mpsc::channel(4);
        let (_events_tx, events_rx) = tokio::sync::mpsc::channel(32);

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
        })
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
            // Ctrl+C or Ctrl+D → quit
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state = AppState::Quitting;
                return Ok(EventResult::Quit);
            }
            // Ctrl+Enter → submit
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_input().await?;
            }
            // Plain Enter → newline in buffer
            KeyCode::Enter => {
                self.context.input_buffer.push('\n');
            }
            // Backspace
            KeyCode::Backspace => {
                self.context.input_buffer.pop();
            }
            // Regular character input
            KeyCode::Char(c) => {
                self.context.input_buffer.push(c);
            }
            // Scroll up
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

        // Slash-commands bypass the bridge entirely.
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let cmd = cmd.trim().to_string();
            self.handle_command(&cmd)?;
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

    /// Handle a slash command (text after the leading `/`).
    pub fn handle_command(&mut self, cmd: &str) -> Result<()> {
        match self.command_registry.dispatch(cmd, &mut self.context)? {
            CmdResult::Quit => {
                self.state = AppState::Quitting;
            }
            CmdResult::Message(msg) => {
                self.context.last_error = Some(msg);
            }
            CmdResult::Continue | CmdResult::ClearError => {
                self.context.last_error = None;
            }
        }
        Ok(())
    }

    /// Render the full UI into the terminal frame.
    pub fn render(&self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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

            self.render_header(frame, chunks[0]);
            self.render_chat(frame, chunks[1]);
            self.render_input(frame, chunks[2]);
            self.render_status(frame, chunks[3]);
        })?;
        Ok(())
    }

    /// Render the top header bar.
    fn render_header(&self, frame: &mut ratatui::Frame, area: Rect) {
        let title = format!(
            " RantaiClaw TUI  session: {}",
            &self.context.session_id[..8.min(self.context.session_id.len())]
        );
        let header = Paragraph::new(title).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(header, area);
    }

    /// Render the scrollable chat history.
    fn render_chat(&self, frame: &mut ratatui::Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .context
            .messages
            .iter()
            .map(|msg| {
                let (label, color) = match msg.role.as_str() {
                    "user" => ("You", Color::Green),
                    "assistant" => ("Assistant", Color::Blue),
                    _ => ("System", Color::Yellow),
                };
                let line = Line::from(vec![
                    Span::styled(
                        format!("{label}: "),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(msg.content.clone()),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .title(" Chat ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(list, area);
    }

    /// Render the multi-line input area.
    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) {
        let display = if self.context.input_buffer.is_empty() {
            Span::styled(
                "Type a message… (Ctrl+Enter to send, /help for commands)",
                Style::default().fg(Color::DarkGray),
            )
        } else {
            Span::raw(self.context.input_buffer.clone())
        };

        let input = Paragraph::new(Line::from(display))
            .block(
                Block::default()
                    .title(" Input ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(input, area);
    }

    /// Render the bottom status bar.
    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let status_text = if let Some(ref err) = self.context.last_error {
            format!(" {err}")
        } else {
            format!(
                " model: {}  msgs: {}  tokens: {}",
                self.context.model,
                self.context.messages.len(),
                self.context.token_usage.total_tokens,
            )
        };

        let status = Paragraph::new(status_text)
            .style(Style::default().fg(Color::White).bg(Color::DarkGray));
        frame.render_widget(status, area);
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

/// Entry point for the TUI: guards TTY, runs the event loop.
pub async fn run_tui(config: TuiConfig) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("TUI requires an interactive terminal (stdin is not a TTY)");
    }

    let mut app = TuiApp::new(&config)?;
    let mut terminal = setup_terminal()?;

    let result = run_loop(&mut app, &mut terminal).await;

    // Always restore terminal, even on error
    let restore_result = restore_terminal(&mut terminal);

    // Surface the loop error first, then the restore error
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
        let (req_tx, _req_rx) = tokio::sync::mpsc::channel(4);
        let (_events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let ctx = TuiContext::new(store, model, None, req_tx, events_rx).expect("context");
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
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

        assert!(app.context.messages.is_empty());
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
