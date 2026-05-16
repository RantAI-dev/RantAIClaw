//! Shared helpers for the `kanban_*` agent tools.
//!
//! Hermes ships nine tools with consistent shape: structured JSON args in,
//! structured JSON result out, and a `check_fn` that hides them from non-
//! orchestrator / non-worker schemas. We mirror that here — tools are always
//! constructable, but `is_available()` returns false unless `RANTAICLAW_KANBAN_TASK`
//! is set in the env OR the active config enables the orchestrator surface.

use serde_json::{json, Value};

use crate::kanban;

/// Env var that, when set, both pins the worker's task id and enables the
/// dispatcher-spawned worker tool surface.
pub const ENV_TASK: &str = "RANTAICLAW_KANBAN_TASK";
/// Env var that pins the active board for a dispatcher-spawned worker.
pub const ENV_BOARD: &str = "RANTAICLAW_KANBAN_BOARD";

#[derive(Debug, Clone, Copy)]
pub enum ToolSurface {
    /// Available to a dispatcher-spawned worker (env var set).
    Worker,
    /// Available to an orchestrator profile (env var or config flag).
    Orchestrator,
}

pub fn worker_task_id() -> Option<String> {
    std::env::var(ENV_TASK).ok().filter(|s| !s.is_empty())
}

pub fn active_board() -> Option<String> {
    std::env::var(ENV_BOARD).ok().filter(|s| !s.is_empty())
}

pub fn is_worker_active() -> bool {
    worker_task_id().is_some()
}

pub fn is_orchestrator_active() -> bool {
    // Orchestrator surface is enabled either when a task id is set (the
    // dispatcher also gives workers the orchestrator surface) or when the
    // host explicitly opted in via env (config wiring can flip this).
    is_worker_active()
        || std::env::var("RANTAICLAW_KANBAN_ORCHESTRATOR").map_or(false, |v| !v.is_empty())
}

pub fn ok(payload: Value) -> String {
    serde_json::to_string(&json!({"ok": true, "result": payload}))
        .unwrap_or_else(|_| "{\"ok\":true}".to_string())
}

pub fn err(message: impl Into<String>) -> String {
    serde_json::to_string(&json!({"ok": false, "error": message.into()}))
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialize failed\"}".to_string())
}

pub fn resolve_task_id(args: &Value) -> Option<String> {
    args.get("task_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(worker_task_id)
}

pub fn connect_active_board() -> kanban::KanbanResult<rusqlite::Connection> {
    kanban::connect(active_board().as_deref())
}
