use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::tasks::{self, ActorType};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskCommentTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    agent_id: Option<String>,
}

impl TaskCommentTool {
    pub fn new(
        config: Arc<Config>,
        security: Arc<SecurityPolicy>,
        agent_id: Option<String>,
    ) -> Self {
        Self {
            config,
            security,
            agent_id,
        }
    }
}

#[async_trait]
impl Tool for TaskCommentTool {
    fn name(&self) -> &str {
        "add_comment"
    }

    fn description(&self) -> &str {
        "Add a comment to a task"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "content": { "type": "string", "description": "Comment text" }
            },
            "required": ["task_id", "content"]
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
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Read-only mode".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded".into()),
            });
        }

        let task_id = match args.get("task_id").and_then(serde_json::Value::as_str) {
            Some(id) => id,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'task_id'".into()),
                })
            }
        };
        let content = match args.get("content").and_then(serde_json::Value::as_str) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'content'".into()),
                })
            }
        };

        match tasks::add_comment(
            &self.config,
            task_id,
            content,
            ActorType::Employee,
            self.agent_id.as_deref(),
            None,
        ) {
            Ok(comment) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&comment)?,
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
