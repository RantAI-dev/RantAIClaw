use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::{list_tasks, ListFilter};
use crate::tools::kanban_common::{connect_active_board, err, is_orchestrator_active, ok};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanListTool;

#[async_trait]
impl Tool for KanbanListTool {
    fn name(&self) -> &str {
        "kanban_list"
    }
    fn description(&self) -> &str {
        "List kanban tasks with optional filters (assignee, status, tenant, include_archived, limit)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "assignee": {"type": "string"},
                "status": {"type": "string"},
                "tenant": {"type": "string"},
                "include_archived": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
            }
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_list is an orchestrator-only tool"),
                error: Some("orchestrator not active".into()),
            });
        }
        let filter = ListFilter {
            assignee: args
                .get("assignee")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: args
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            tenant: args
                .get("tenant")
                .and_then(Value::as_str)
                .map(str::to_string),
            include_archived: args
                .get("include_archived")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            limit: args.get("limit").and_then(Value::as_i64),
        };
        let conn = connect_active_board()?;
        let tasks = list_tasks(&conn, &filter)?;
        Ok(ToolResult {
            success: true,
            output: ok(json!({"tasks": tasks})),
            error: None,
        })
    }
}
