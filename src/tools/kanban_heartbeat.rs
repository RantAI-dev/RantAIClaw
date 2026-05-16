use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::heartbeat_claim;
use crate::tools::kanban_common::{
    connect_active_board, err, is_worker_active, ok, resolve_task_id,
};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanHeartbeatTool;

#[async_trait]
impl Tool for KanbanHeartbeatTool {
    fn name(&self) -> &str {
        "kanban_heartbeat"
    }
    fn description(&self) -> &str {
        "Signal liveness during a long-running kanban task. Pure side-effect."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string"},
                "note": {"type": "string"}
            }
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_worker_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_heartbeat is worker-only"),
                error: Some("worker not active".into()),
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
        let ok_flag = heartbeat_claim(&conn, &id, None, None)?;
        Ok(ToolResult {
            success: ok_flag,
            output: if ok_flag {
                ok(json!({"task_id": id, "extended": true}))
            } else {
                err("claim lost — task is no longer running on this worker")
            },
            error: None,
        })
    }
}
