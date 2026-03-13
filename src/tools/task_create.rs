use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::tasks::{self, CreateTask, TaskPriority};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskCreateTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    agent_id: Option<String>,
}

impl TaskCreateTool {
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
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "create_task"
    }

    fn description(&self) -> &str {
        "Create a new task. Assign to yourself, another employee, or leave unassigned."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Task title (required)" },
                "description": { "type": "string", "description": "Task description" },
                "priority": { "type": "string", "enum": ["LOW", "MEDIUM", "HIGH"] },
                "assignee_id": { "type": "string", "description": "Employee ID to assign to" },
                "group_id": { "type": "string", "description": "Team/group ID" },
                "reviewer_id": { "type": "string", "description": "Employee ID for review" },
                "human_review": { "type": "boolean", "description": "Requires human review" },
                "due_date": { "type": "string", "description": "Due date in RFC3339 format" }
            },
            "required": ["title"]
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
                error: Some("Security policy: read-only mode".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded".into()),
            });
        }

        let title = match args.get("title").and_then(serde_json::Value::as_str) {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'title' parameter".into()),
                })
            }
        };

        let priority = args
            .get("priority")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| TaskPriority::try_from(s).ok());

        let due_date = args
            .get("due_date")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));

        let input = CreateTask {
            title,
            description: args
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            priority,
            assignee_id: args
                .get("assignee_id")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            group_id: args
                .get("group_id")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            reviewer_id: args
                .get("reviewer_id")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            human_review: args
                .get("human_review")
                .and_then(serde_json::Value::as_bool),
            parent_task_id: None,
            due_date,
            organization_id: None,
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
                    "status": task.status,
                    "assignee_id": task.assignee_id
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
