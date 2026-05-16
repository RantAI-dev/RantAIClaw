use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::add_comment;
use crate::tools::kanban_common::{
    connect_active_board, err, is_orchestrator_active, is_worker_active, ok,
};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanCommentTool {
    pub default_author: String,
}

impl Default for KanbanCommentTool {
    fn default() -> Self {
        Self {
            default_author: "agent".to_string(),
        }
    }
}

#[async_trait]
impl Tool for KanbanCommentTool {
    fn name(&self) -> &str {
        "kanban_comment"
    }
    fn description(&self) -> &str {
        "Append a durable note to a kanban task's comment thread."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string"},
                "body": {"type": "string"},
                "author": {"type": "string"}
            },
            "required": ["task_id", "body"]
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_worker_active() && !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_comment unavailable: not running as kanban worker"),
                error: Some("kanban not active".into()),
            });
        }
        let Some(id) = args.get("task_id").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("task_id required"),
                error: Some("missing task_id".into()),
            });
        };
        let Some(body) = args.get("body").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("body required"),
                error: Some("missing body".into()),
            });
        };
        let author = args
            .get("author")
            .and_then(Value::as_str)
            .unwrap_or(&self.default_author);
        let conn = connect_active_board()?;
        let cid = add_comment(&conn, id, author, body)?;
        Ok(ToolResult {
            success: true,
            output: ok(json!({"comment_id": cid, "task_id": id})),
            error: None,
        })
    }
}
