use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::unblock_task;
use crate::tools::kanban_common::{connect_active_board, err, is_orchestrator_active, ok};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanUnblockTool;

#[async_trait]
impl Tool for KanbanUnblockTool {
    fn name(&self) -> &str {
        "kanban_unblock"
    }
    fn description(&self) -> &str {
        "Move a blocked kanban task back to ready (orchestrator only)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"task_id": {"type": "string"}},
            "required": ["task_id"]
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_unblock is orchestrator-only"),
                error: Some("orchestrator not active".into()),
            });
        }
        let Some(id) = args.get("task_id").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("task_id required"),
                error: Some("missing task_id".into()),
            });
        };
        let conn = connect_active_board()?;
        let ok_flag = unblock_task(&conn, id)?;
        Ok(ToolResult {
            success: ok_flag,
            output: if ok_flag {
                ok(json!({"task_id": id}))
            } else {
                err("task not in blocked")
            },
            error: None,
        })
    }
}
