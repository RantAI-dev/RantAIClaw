use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::{create_task, CreateTaskInput};
use crate::tools::kanban_common::{connect_active_board, err, is_orchestrator_active, ok};
use crate::tools::traits::{Tool, ToolResult};

pub struct KanbanCreateTool;

#[async_trait]
impl Tool for KanbanCreateTool {
    fn name(&self) -> &str {
        "kanban_create"
    }
    fn description(&self) -> &str {
        "Create a new kanban task and optionally link it under parent tasks (orchestrator fan-out)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "body": {"type": "string"},
                "assignee": {"type": "string"},
                "parents": {"type": "array", "items": {"type": "string"}},
                "tenant": {"type": "string"},
                "workspace_kind": {"type": "string", "enum": ["scratch", "worktree", "dir"]},
                "workspace_path": {"type": "string"},
                "priority": {"type": "integer"},
                "triage": {"type": "boolean"},
                "idempotency_key": {"type": "string"},
                "max_runtime_seconds": {"type": "integer"},
                "skills": {"type": "array", "items": {"type": "string"}},
                "max_retries": {"type": "integer"}
            },
            "required": ["title", "assignee"]
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if !is_orchestrator_active() {
            return Ok(ToolResult {
                success: false,
                output: err("kanban_create is an orchestrator-only tool"),
                error: Some("orchestrator not active".into()),
            });
        }
        let Some(title) = args.get("title").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("title required"),
                error: Some("missing title".into()),
            });
        };
        let Some(assignee) = args.get("assignee").and_then(Value::as_str) else {
            return Ok(ToolResult {
                success: false,
                output: err("assignee required"),
                error: Some("missing assignee".into()),
            });
        };
        let parents: Vec<String> = args
            .get("parents")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let skills: Option<Vec<String>> = args.get("skills").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
        let input = CreateTaskInput {
            title: title.to_string(),
            body: args.get("body").and_then(Value::as_str).map(str::to_string),
            assignee: Some(assignee.to_string()),
            created_by: Some("orchestrator".to_string()),
            workspace_kind: args
                .get("workspace_kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            workspace_path: args
                .get("workspace_path")
                .and_then(Value::as_str)
                .map(str::to_string),
            tenant: args
                .get("tenant")
                .and_then(Value::as_str)
                .map(str::to_string),
            priority: args.get("priority").and_then(Value::as_i64),
            parents,
            triage: args.get("triage").and_then(Value::as_bool).unwrap_or(false),
            idempotency_key: args
                .get("idempotency_key")
                .and_then(Value::as_str)
                .map(str::to_string),
            max_runtime_seconds: args.get("max_runtime_seconds").and_then(Value::as_i64),
            skills,
            max_retries: args.get("max_retries").and_then(Value::as_i64),
        };
        let conn = connect_active_board()?;
        let id = create_task(&conn, &input)?;
        Ok(ToolResult {
            success: true,
            output: ok(json!({"task_id": id})),
            error: None,
        })
    }
}
