use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::tasks::{self, CreateTask, TaskPriority};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskCreateSubtaskTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    agent_id: Option<String>,
}

impl TaskCreateSubtaskTool {
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
impl Tool for TaskCreateSubtaskTool {
    fn name(&self) -> &str {
        "create_subtask"
    }

    fn description(&self) -> &str {
        "Add a subtask to an existing task"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "parent_task_id": { "type": "string", "description": "Parent task ID" },
                "title": { "type": "string", "description": "Subtask title" },
                "description": { "type": "string" },
                "assignee_id": { "type": "string" },
                "priority": { "type": "string", "enum": ["LOW", "MEDIUM", "HIGH"] },
                "human_review": { "type": "boolean", "description": "Requires review when done" }
            },
            "required": ["parent_task_id", "title"]
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

        let parent_id = match args
            .get("parent_task_id")
            .and_then(serde_json::Value::as_str)
        {
            Some(id) => id,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'parent_task_id'".into()),
                })
            }
        };
        let title = match args.get("title").and_then(serde_json::Value::as_str) {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'title'".into()),
                })
            }
        };

        // Verify parent exists and is not itself a subtask (one level deep)
        let parent = match tasks::get_task(&self.config, parent_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                })
            }
        };
        if parent.parent_task_id.is_some() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Cannot nest subtasks more than one level deep".into()),
            });
        }

        let input = CreateTask {
            title: title.into(),
            description: args
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            priority: args
                .get("priority")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| TaskPriority::try_from(s).ok()),
            assignee_id: args
                .get("assignee_id")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            group_id: parent.group_id.clone(),
            reviewer_id: None,
            human_review: args
                .get("human_review")
                .and_then(serde_json::Value::as_bool),
            parent_task_id: Some(parent_id.into()),
            due_date: None,
            organization_id: parent.organization_id.clone(),
            created_by_employee_id: self.agent_id.clone(),
            created_by_user_id: None,
            metadata: None,
        };

        match tasks::create_task(&self.config, &input) {
            Ok(task) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "id": task.id,
                    "title": task.title,
                    "parent_task_id": parent_id
                }))?,
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
