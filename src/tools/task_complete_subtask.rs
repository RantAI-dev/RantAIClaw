use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::tasks::{self, state, ActorType, ReviewStatus, TaskEventType, TaskPatch, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskCompleteSubtaskTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    agent_id: Option<String>,
}

impl TaskCompleteSubtaskTool {
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
impl Tool for TaskCompleteSubtaskTool {
    fn name(&self) -> &str {
        "complete_subtask"
    }

    fn description(&self) -> &str {
        "Mark a subtask as done. If review is required, it enters IN_REVIEW instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "subtask_id": {
                    "type": "string",
                    "description": "Subtask ID to complete"
                }
            },
            "required": ["subtask_id"]
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

        let subtask_id = match args.get("subtask_id").and_then(serde_json::Value::as_str) {
            Some(id) => id,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'subtask_id'".into()),
                })
            }
        };

        let subtask = match tasks::get_task(&self.config, subtask_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                })
            }
        };

        let new_status = if subtask.human_review || subtask.reviewer_id.is_some() {
            TaskStatus::InReview
        } else {
            TaskStatus::Done
        };

        // Auto-advance from TODO through IN_PROGRESS if needed
        let current_status = subtask.status;
        if current_status == TaskStatus::Todo {
            // TODO -> IN_PROGRESS is always valid; advance first
            if let Err(e) = state::validate_transition(TaskStatus::Todo, TaskStatus::InProgress) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
            let advance_patch = TaskPatch {
                status: Some(TaskStatus::InProgress),
                ..TaskPatch::default()
            };
            if let Err(e) = tasks::update_task(&self.config, subtask_id, &advance_patch) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        } else if let Err(e) = state::validate_transition(current_status, new_status) {
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

        match tasks::update_task(&self.config, subtask_id, &patch) {
            Ok(updated) => {
                let _ = tasks::record_event(
                    &self.config,
                    subtask_id,
                    TaskEventType::SubtaskCompleted,
                    ActorType::Employee,
                    self.agent_id.as_deref(),
                    None,
                    json!({"status": new_status.as_str()}),
                );
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "id": updated.id,
                        "status": updated.status,
                        "needs_review": new_status == TaskStatus::InReview
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
