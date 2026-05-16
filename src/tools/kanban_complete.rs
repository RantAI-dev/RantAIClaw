use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::complete_task;
use crate::tools::kanban_common::{
    connect_active_board, err, is_orchestrator_active, is_worker_active, ok, resolve_task_id,
};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanCompleteTool;

#[async_trait]
impl Tool for KanbanCompleteTool {
    fn name(&self) -> &str {
        "kanban_complete"
    }
    fn description(&self) -> &str {
        "Close the current kanban task with summary + metadata. Either summary or result must be provided."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string"},
                "summary": {"type": "string", "description": "Human-readable handoff. Required when result omitted."},
                "result": {"type": "string", "description": "Legacy single-line result; appears on the task row."},
                "metadata": {"type": "object", "description": "Structured handoff facts (changed_files, verification, dependencies, …)."}
            }
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_worker_active() && !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_complete unavailable: not running as kanban worker"),
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
        let summary = args.get("summary").and_then(Value::as_str);
        let result = args.get("result").and_then(Value::as_str);
        let metadata = args.get("metadata").cloned();
        if summary.is_none() && result.is_none() {
            return Ok(ToolResult {
                success: false,
                output: err("at least one of summary / result required"),
                error: Some("missing handoff".into()),
            });
        }
        let conn = connect_active_board()?;
        let ok_flag = complete_task(&conn, &id, result, summary, metadata.as_ref())?;
        Ok(ToolResult {
            success: ok_flag,
            output: if ok_flag {
                ok(json!({"task_id": id, "status": "done"}))
            } else {
                err("task not in completable status")
            },
            error: None,
        })
    }
}
