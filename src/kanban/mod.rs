//! SQLite-backed Kanban board for multi-profile, multi-project collaboration.
//!
//! Port of the Hermes Agent kanban subsystem at v1 schema parity. Every Hermes
//! verb (CLI / `/kanban` slash command / `kanban_*` agent tool) maps 1:1 to a
//! Rust equivalent so existing playbooks transfer without rewrites.
//!
//! See `docs/superpowers/specs/2026-05-16-kanban-port-design.md` for the
//! design rationale and the Hermes reference path.

pub mod boards;
pub mod cli;
pub mod context;
pub mod dispatcher;
pub mod errors;
pub mod events;
pub mod notifier;
pub mod notify;
pub mod paths;
pub mod runs;
pub mod schema;
pub mod slash;
pub mod specify;
pub mod store;

#[cfg(test)]
pub(crate) mod test_env;

#[cfg(test)]
mod tests;

pub use boards::{
    board_exists, clear_current_board, create_board, get_current_board, list_boards,
    normalize_board_slug, set_current_board, Board, DEFAULT_BOARD,
};
pub use cli::{handle_command, BoardsCommand, KanbanCommand};
pub use context::build_worker_context;
pub use dispatcher::{Dispatcher, DispatcherHandle, DispatcherOptions};
pub use errors::{KanbanError, Result as KanbanResult};
pub use events::{EventKind, TASK_TERMINAL_EVENT_KINDS};
pub use notify::{list_subscriptions, subscribe, unsubscribe, NotifySubscription, SubscribeInput};
#[allow(unused_imports)]
pub use paths::kanban_db_path;
pub use runs::{list_runs, Run};
pub use schema::{apply_schema, KANBAN_SCHEMA_VERSION, VALID_STATUSES, VALID_WORKSPACE_KINDS};
pub use slash::run_slash;
pub use store::{
    add_comment, add_link, archive_task, assign_task, block_task, claim_task, complete_task,
    connect, create_task, get_task, heartbeat_claim, init_db, list_comments, list_events,
    list_tasks, recompute_ready, release_stale_claims, remove_link, unblock_task, Comment,
    CreateTaskInput, Event, ListFilter, Task,
};
