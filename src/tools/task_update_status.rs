use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::tasks::{self, state, ActorType, ReviewStatus, TaskEventType, TaskPatch, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskUpdateStatusTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    agent_id: Option<String>,
}

impl TaskUpdateStatusTool {
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
impl Tool for TaskUpdateStatusTool {
    fn name(&self) -> &str {
        "update_task_status"
    }

    fn description(&self) -> &str {
        "Move a task to the next status: TODO -> IN_PROGRESS -> IN_REVIEW -> DONE"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["TODO", "IN_PROGRESS", "IN_REVIEW", "DONE", "CANCELLED"]
                }
            },
            "required": ["task_id", "status"]
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
        let new_status_str = match args.get("status").and_then(serde_json::Value::as_str) {
            Some(s) => s,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'status'".into()),
                })
            }
        };
        let new_status = match TaskStatus::try_from(new_status_str) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                })
            }
        };

        let existing = match tasks::get_task(&self.config, task_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                })
            }
        };

        if let Err(e) = state::validate_transition(existing.status, new_status) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            });
        }

        let patch = TaskPatch {
            status: Some(new_status),
            review_status: if new_status == TaskStatus::InReview {
                Some(Some(ReviewStatus::Pending))
            } else {
                None
            },
            ..TaskPatch::default()
        };

        match tasks::update_task(&self.config, task_id, &patch) {
            Ok(task) => {
                let _ = tasks::record_event(
                    &self.config,
                    task_id,
                    TaskEventType::StatusChanged,
                    ActorType::Employee,
                    self.agent_id.as_deref(),
                    None,
                    json!({"from": existing.status.as_str(), "to": new_status.as_str()}),
                );
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "id": task.id,
                        "status": task.status,
                        "review_status": task.review_status
                    }))?,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}
