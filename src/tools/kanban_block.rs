use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::block_task;
use crate::tools::kanban_common::{
    connect_active_board, err, is_orchestrator_active, is_worker_active, ok, resolve_task_id,
};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanBlockTool;

#[async_trait]
impl Tool for KanbanBlockTool {
    fn name(&self) -> &str {
        "kanban_block"
    }
    fn description(&self) -> &str {
        "Block the current kanban task awaiting human input."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["reason"]
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_worker_active() && !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_block unavailable: not running as kanban worker"),
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
        let Some(reason) = args.get("reason").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("reason is required"),
                error: Some("missing reason".into()),
            });
        };
        let conn = connect_active_board()?;
        let ok_flag = block_task(&conn, &id, Some(reason))?;
        Ok(ToolResult {
            success: ok_flag,
            output: if ok_flag {
                ok(json!({"task_id": id, "status": "blocked"}))
            } else {
                err("task not in running/ready")
            },
            error: None,
        })
    }
}
