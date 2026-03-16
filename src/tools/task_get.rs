use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::tasks;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskGetTool {
    config: Arc<Config>,
}

impl TaskGetTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "get_task"
    }

    fn description(&self) -> &str {
        "Get full task detail including subtasks, comments, review status, and activity timeline"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Task ID to fetch"
                }
            },
            "required": ["task_id"]
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

        let task_id = match args.get("task_id").and_then(serde_json::Value::as_str) {
            Some(id) => id,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'task_id' parameter".into()),
                })
            }
        };

        match tasks::get_task_detail(&self.config, task_id) {
            Ok(detail) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&detail)?,
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
