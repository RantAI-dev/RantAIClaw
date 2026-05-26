use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::{build_worker_context, get_task, list_comments, list_runs};
use crate::tools::kanban_common::{
    connect_active_board, err, is_orchestrator_active, is_worker_active, ok, resolve_task_id,
};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanShowTool;

#[async_trait]
impl Tool for KanbanShowTool {
    fn name(&self) -> &str {
        "kanban_show"
    }
    fn description(&self) -> &str {
        "Read the current kanban task (title, body, prior runs, comments, full pre-formatted worker_context). Defaults to the worker's own task id."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task id; defaults to $RANTAICLAW_KANBAN_TASK"}
            }
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_worker_active() && !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_show unavailable: not running as kanban worker"),
                error: Some("kanban not active".into()),
            });
        }
        let Some(id) = resolve_task_id(&args) else {
            return Ok(ToolResult {
                success: false,
                output: err("task_id is required"),
                error: Some("missing task_id".into()),
            });
        };
        let conn = connect_active_board()?;
        let Some(task) = get_task(&conn, &id)? else {
            return Ok(ToolResult {
                success: false,
                output: err(format!("task {id} not found")),
                error: Some("not found".into()),
            });
        };
        let comments = list_comments(&conn, &id)?;
        let runs = list_runs(&conn, &id)?;
        let context = build_worker_context(&conn, &id)?;
        let payload = json!({
            "task": task,
            "comments": comments,
            "runs": runs,
            "worker_context": context,
        });
        Ok(ToolResult {
            success: true,
            output: ok(payload),
            error: None,
        })
    }
}
