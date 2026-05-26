use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::add_link;
use crate::tools::kanban_common::{connect_active_board, err, is_orchestrator_active, ok};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanLinkTool;

#[async_trait]
impl Tool for KanbanLinkTool {
    fn name(&self) -> &str {
        "kanban_link"
    }
    fn description(&self) -> &str {
        "Add a parent → child dependency between two existing kanban tasks (orchestrator only)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "parent_id": {"type": "string"},
                "child_id": {"type": "string"}
            },
            "required": ["parent_id", "child_id"]
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_link is orchestrator-only"),
                error: Some("orchestrator not active".into()),
            });
        }
        let Some(parent) = args.get("parent_id").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("parent_id required"),
                error: Some("missing parent_id".into()),
            });
        };
        let Some(child) = args.get("child_id").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("child_id required"),
                error: Some("missing child_id".into()),
            });
        };
        let conn = connect_active_board()?;
        let added = add_link(&conn, parent, child)?;
        Ok(ToolResult {
            success: added,
            output: if added {
                ok(json!({"parent_id": parent, "child_id": child}))
            } else {
                err("link rejected (cycle or already linked)")
            },
            error: None,
        })
    }
}
