use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::tasks::{self, TaskFilter, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskListTool {
    config: Arc<Config>,
    agent_id: Option<String>,
}

impl TaskListTool {
    pub fn new(config: Arc<Config>, agent_id: Option<String>) -> Self {
        Self { config, agent_id }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "list_tasks"
    }

    fn description(&self) -> &str {
        "List tasks. By default returns tasks assigned to you. Use status filter to narrow results."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["TODO", "IN_PROGRESS", "IN_REVIEW", "DONE", "CANCELLED"],
                    "description": "Filter by status"
                },
                "all": {
                    "type": "boolean",
                    "description": "If true, list all tasks (not just assigned to you)",
                    "default": false
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.config.tasks.enabled {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Task engine is disabled".into()),
            });
        }

        let status = args
            .get("status")
            .and_then(serde_json::Value::as_str)
            .map(TaskStatus::try_from)
            .transpose()
            .map_err(|e| anyhow::anyhow!(e))?;

        let all = args
            .get("all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let filter = TaskFilter {
            status,
            assignee_id: if all { None } else { self.agent_id.clone() },
            top_level_only: Some(true),
            ..TaskFilter::default()
        };

        match tasks::list_tasks(&self.config, &filter) {
            Ok(tasks) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&tasks)?,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}
