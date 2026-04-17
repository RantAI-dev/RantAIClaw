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

use super::commands::{CommandRegistry, CommandResult as CmdResult};
use super::context::TuiContext;
use super::TuiConfig;
use crate::sessions::SessionStore;

/// Current application state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppState {
    Chatting,
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

        let context = TuiContext::new(store, &config.model, config.resume_session.as_deref())?;

        Ok(Self {
            state: AppState::Chatting,
            context,
            command_registry: CommandRegistry::new(),
        })
    }

    /// Process a single terminal event, returning whether to continue or quit.
    pub fn handle_event(&mut self, event: Event) -> Result<EventResult> {
        if let Event::Key(key) = event {
            return self.handle_key(key);
        }
        Ok(EventResult::Continue)
    }

    /// Dispatch a key event.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        match key.code {
            // Ctrl+C or Ctrl+D → quit
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state = AppState::Quitting;
                return Ok(EventResult::Quit);
            }
            // Ctrl+Enter → submit
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_input()?;
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
    pub fn submit_input(&mut self) -> Result<()> {
        let input = self.context.input_buffer.trim().to_string();
        self.context.input_buffer.clear();

        if input.is_empty() {
            return Ok(());
        }

        if let Some(cmd) = input.strip_prefix('/') {
            self.handle_command(cmd.trim())?;
        } else {
            self.context.append_user_message(&input)?;
            // NOTE: actual provider call will be wired in a later task.
            // For now, emit a placeholder acknowledgement.
            self.context
                .append_assistant_message("[provider not yet wired]")?;
        }

        // Reset scroll to bottom after new content
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

    let result = run_loop(&mut app, &mut terminal);

    // Always restore terminal, even on error
    let restore_result = restore_terminal(&mut terminal);

    // Surface the loop error first, then the restore error
    result?;
    restore_result?;

    Ok(())
}

/// Inner event loop separated from terminal lifecycle for easier testing.
fn run_loop(app: &mut TuiApp, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    loop {
        app.render(terminal)?;

        if event::poll(std::time::Duration::from_millis(100))? {
            let ev = event::read()?;
            match app.handle_event(ev)? {
                EventResult::Quit => break,
                EventResult::Continue => {}
            }
        }

        if app.state == AppState::Quitting {
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
        let ctx = TuiContext::new(store, model, None).expect("context");
        TuiApp {
            state: AppState::Chatting,
            context: ctx,
            command_registry: CommandRegistry::new(),
        }
    }

    #[test]
    fn app_creates_new_session_on_start() {
        let store = SessionStore::in_memory().expect("store");
        let app = make_app_from_store(store, "test-model");

        assert!(!app.context.session_id.is_empty());
        assert_eq!(app.state, AppState::Chatting);
        assert!(app.context.messages.is_empty());
    }

    #[test]
    fn app_handles_quit_command() {
        let store = SessionStore::in_memory().expect("store");
        let mut app = make_app_from_store(store, "test-model");

        app.context.input_buffer = "/quit".to_string();
        app.submit_input().unwrap();

        assert_eq!(app.state, AppState::Quitting);
    }

    #[test]
    fn app_handles_new_command() {
        let store = SessionStore::in_memory().expect("store");
        let mut app = make_app_from_store(store, "test-model");

        let first_session_id = app.context.session_id.clone();

        // Add a message then clear
        app.context.input_buffer = "hello".to_string();
        app.submit_input().unwrap();
        assert!(!app.context.messages.is_empty());

        app.context.input_buffer = "/new".to_string();
        app.submit_input().unwrap();

        assert!(app.context.messages.is_empty());
        assert_ne!(app.context.session_id, first_session_id);
        assert_eq!(app.state, AppState::Chatting);
    }
}
