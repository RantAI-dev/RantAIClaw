# RantaiClaw TUI Design Specification

**Date:** 2026-04-16  
**Status:** Approved  
**Scope:** Interactive Terminal User Interface for RantaiClaw

## 1. Overview

This specification defines the design for a rich Terminal User Interface (TUI) for RantaiClaw, targeting competitive parity with Hermes Agent. The TUI will be the default entry point for interactive use, providing multiline editing, slash command autocomplete, streaming visualization, and persistent session management.

### 1.1 Goals

- Match Hermes Agent TUI feature set (competitive parity)
- Maintain RantaiClaw's performance characteristics (~12MB binary, <200ms startup)
- Integrate cleanly with existing agent loop, providers, tools, and memory
- Provide dedicated session persistence with FTS5 search

### 1.2 Non-Goals

- Voice mode (STT/TTS) — deferred to future release
- GUI/desktop application
- Web-based interface

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         User Terminal                           │
└─────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌─────────────────────────────────────────────────────────────────┐
│                           TUI Layer                             │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────┐    │
│  │  Input   │  │  Render  │  │ Commands │  │   Widgets    │    │
│  │ (editor, │  │ (chat,   │  │ (/model, │  │  (spinner,   │    │
│  │ history) │  │ stream)  │  │ /new...) │  │   picker)    │    │
│  └──────────┘  └──────────┘  └──────────┘  └──────────────┘    │
│                         │                                       │
│                    TuiContext (shared state)                    │
└─────────────────────────────────────────────────────────────────┘
                                │
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                 ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│   Sessions DB    │  │   Agent Loop     │  │  Config/Memory   │
│  (SQLite+FTS5)   │  │  (existing)      │  │   (existing)     │
└──────────────────┘  └──────────────────┘  └──────────────────┘
```

**Key principles:**
- TUI is a standalone frontend, not a channel
- Orchestrates existing agent loop directly
- Own persistence layer (sessions) separate from memory
- Reuses existing config, providers, tools infrastructure

## 3. Module Structure

```
src/
├── tui/
│   ├── mod.rs              # Public API: run_tui(), TuiConfig
│   ├── app.rs              # TuiApp state machine, main loop
│   ├── context.rs          # TuiContext: shared mutable state
│   │
│   ├── input/
│   │   ├── mod.rs          # Input subsystem exports
│   │   ├── editor.rs       # Multiline TextArea (Enter=newline, Ctrl+Enter=submit)
│   │   ├── history.rs      # Persistent command history (SQLite-backed)
│   │   └── keybindings.rs  # Key event handling, shortcuts
│   │
│   ├── render/
│   │   ├── mod.rs          # Render subsystem exports
│   │   ├── layout.rs       # Screen layout (header, chat, input, status)
│   │   ├── chat.rs         # Conversation message rendering
│   │   ├── stream.rs       # Streaming response with spinner
│   │   ├── tools.rs        # Tool call blocks (collapsible)
│   │   └── markdown.rs     # Basic markdown rendering
│   │
│   ├── commands/
│   │   ├── mod.rs          # Command dispatcher, registry
│   │   ├── core.rs         # /help, /quit, /new, /clear
│   │   ├── model.rs        # /model picker
│   │   ├── session.rs      # /sessions, /resume, /insights
│   │   ├── memory.rs       # /memory, /compress
│   │   ├── agent.rs        # /retry, /undo, /stop
│   │   ├── cron.rs         # /cron list, add, remove
│   │   ├── config.rs       # /usage, /status, /platforms
│   │   └── skills.rs       # /skills, /personality
│   │
│   └── widgets/
│       ├── mod.rs          # Widget exports
│       ├── spinner.rs      # Animated spinner (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏)
│       ├── picker.rs       # Modal picker (models, personalities)
│       ├── autocomplete.rs # Slash command autocomplete popup
│       └── progress.rs     # Token/context usage bar
│
├── sessions/
│   ├── mod.rs              # SessionManager public API
│   ├── store.rs            # SessionStore: SQLite + FTS5
│   ├── types.rs            # Session, Message, SearchResult
│   └── migrations.rs       # Schema versioning
│
└── main.rs                 # Updated: TUI as default entry point
```

**Estimated scope:** ~25 new files, ~4000-5000 lines

## 4. Core Components

### 4.1 TuiApp State Machine

```rust
pub enum AppState {
    Chatting,
    Streaming { cancel_token: CancellationToken },
    PickerOpen { picker: PickerKind },
    Autocomplete { suggestions: Vec<String>, selected: usize },
    Quitting,
}

pub struct TuiApp {
    state: AppState,
    context: TuiContext,
    terminal: Terminal<CrosstermBackend<Stdout>>,
}
```

### 4.2 TuiContext (Shared State)

```rust
pub struct TuiContext {
    // Session
    pub session_id: String,
    pub session_manager: SessionManager,
    
    // Conversation
    pub messages: Vec<ConversationMessage>,
    pub current_response: Option<StreamingResponse>,
    
    // Agent integration
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<ToolDefinition>,
    pub config: Config,
    pub memory: Arc<dyn Memory>,
    
    // UI state
    pub input_buffer: TextArea,
    pub command_history: CommandHistory,
    pub scroll_offset: usize,
    
    // Status
    pub model: String,
    pub token_usage: TokenUsage,
    pub last_error: Option<String>,
}
```

### 4.3 Main Loop

```rust
pub async fn run_tui(config: TuiConfig) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("TUI requires an interactive terminal");
    }
    
    let terminal = setup_terminal()?;
    let mut app = TuiApp::new(config).await?;
    
    loop {
        app.render()?;
        
        if let Some(event) = poll_event(Duration::from_millis(50))? {
            match app.handle_event(event).await? {
                EventResult::Continue => {}
                EventResult::Quit => break,
            }
        }
        
        app.tick_streaming().await?;
    }
    
    restore_terminal(terminal)?;
    Ok(())
}
```

## 5. Session Management

### 5.1 Database Schema

```sql
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    parent_session_id TEXT,
    model TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    message_count INTEGER DEFAULT 0,
    token_count INTEGER DEFAULT 0,
    source TEXT DEFAULT 'tui',
    FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    tool_calls TEXT,
    timestamp INTEGER NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    content,
    content=messages,
    content_rowid=id
);

CREATE INDEX idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX idx_messages_session ON messages(session_id, timestamp);
```

### 5.2 SessionManager API

```rust
pub struct SessionManager {
    store: SessionStore,
}

impl SessionManager {
    pub async fn new_session(&self, model: &str) -> Result<Session>;
    pub async fn resume_session(&self, id: &str) -> Result<Session>;
    pub async fn end_session(&self, id: &str) -> Result<()>;
    pub async fn append_message(&self, session_id: &str, msg: &Message) -> Result<()>;
    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<Message>>;
    pub async fn split_session(&self, session_id: &str, summary: &str) -> Result<Session>;
    pub async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>>;
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>>;
    pub async fn set_title(&self, session_id: &str, title: &str) -> Result<()>;
    pub async fn get_session(&self, id: &str) -> Result<Option<Session>>;
}
```

## 6. Slash Commands

### 6.1 Full Command List (30 commands)

| Command | Args | Description |
|---------|------|-------------|
| `/help` | `[command]` | Show help |
| `/quit`, `/exit` | | Exit TUI |
| `/new`, `/clear` | | Start fresh session |
| `/model` | `[provider:model]` | Pick or set model |
| `/usage` | | Show token/cost stats |
| `/compress` | | Compress context |
| `/sessions` | `[--days N]` | List past sessions |
| `/resume` | `<id\|title>` | Resume a session |
| `/title` | `<name>` | Set session title |
| `/search` | `<query>` | FTS5 search history |
| `/insights` | `[--days N]` | Session analytics |
| `/retry` | | Retry last response |
| `/undo` | | Remove last exchange |
| `/stop` | | Cancel streaming |
| `/memory` | `[add\|list\|remove]` | Manage memory |
| `/forget` | `<key>` | Remove memory entry |
| `/cron` | | List cron jobs |
| `/cron add` | `<schedule> <task>` | Add cron job |
| `/cron remove` | `<id>` | Remove cron job |
| `/cron pause` | `<id>` | Pause cron job |
| `/cron resume` | `<id>` | Resume cron job |
| `/skills` | | List available skills |
| `/skill` | `<name>` | Run a skill |
| `/personality` | `[name]` | Set personality |
| `/status` | | Show all component status |
| `/platforms` | | Show connected channels |
| `/doctor` | | Run diagnostics |
| `/config` | `[key] [value]` | View/set config |
| `/debug` | | Toggle debug mode |

### 6.2 Command Handler Trait

```rust
pub trait CommandHandler: Send + Sync {
    fn name(&self) -> &str;
    fn aliases(&self) -> Vec<&str> { vec![] }
    fn description(&self) -> &str;
    fn usage(&self) -> &str { self.name() }
    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult>;
    fn autocomplete(&self, partial: &str, ctx: &TuiContext) -> Vec<String> { vec![] }
}
```

## 7. Entry Point Architecture

```
rantaiclaw           → TUI (if TTY) or error
rantaiclaw chat      → Same as above
rantaiclaw chat -m   → Single message mode (non-interactive)
rantaiclaw daemon    → Background service
rantaiclaw gateway   → HTTP gateway
```

TTY guard prevents running interactive TUI in pipes/non-interactive contexts.

## 8. Streaming Output

Hermes-style inline streaming:
- Text chunks append to current response in real-time
- Spinner animation during LLM thinking
- Tool calls rendered as collapsible blocks
- Ctrl+C cancels streaming cleanly

## 9. Dependencies

### 9.1 New Dependencies

```toml
[dependencies]
ratatui = "0.29"      # TUI framework (~400KB)
crossterm = "0.28"    # Terminal control (~150KB)
```

**Binary size impact:** +1-2MB

### 9.2 Feature Flag

```toml
[features]
default = ["tui"]
tui = ["dep:ratatui", "dep:crossterm"]
minimal = []  # For daemon-only builds
```

## 10. Testing Strategy

### 10.1 Unit Tests

- Command parsing and execution
- Session store CRUD operations
- FTS5 search functionality
- Input buffer manipulation
- Widget rendering

### 10.2 Integration Tests

- Session persistence roundtrip
- Command dispatch for all registered commands
- Agent loop integration

### 10.3 Render Snapshot Tests

- Chat message rendering
- Spinner frame cycling
- Layout at various terminal sizes

### 10.4 Manual Testing Checklist

- [ ] TUI launches on TTY, errors on non-TTY
- [ ] Multiline input (Shift+Enter, paste)
- [ ] Command history persistence
- [ ] `/model` picker functionality
- [ ] Streaming with spinner
- [ ] Ctrl+C cancellation
- [ ] `/sessions` and `/resume`
- [ ] `/search` cross-session
- [ ] All 30 slash commands
- [ ] Window resize handling

## 11. Implementation Phases

1. **Phase 1: Core TUI** — Basic app loop, input, rendering, `/help`, `/quit`, `/new`
2. **Phase 2: Sessions** — SQLite store, `/sessions`, `/resume`, `/search`
3. **Phase 3: Commands** — All 30 slash commands with autocomplete
4. **Phase 4: Polish** — Streaming UX, widgets, error handling

## 12. Success Criteria

- User can run `rantaiclaw` and get interactive TUI
- Feature parity with Hermes TUI core experience
- Session history persists and is searchable
- Binary size remains under 15MB
- Startup time remains under 500ms
