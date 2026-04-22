use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::security::SecurityPolicy;
use crate::tasks::{self, state, ActorType, ReviewAction, TaskEventType, TaskPatch};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct TaskReviewTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    agent_id: Option<String>,
}

impl TaskReviewTool {
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
impl Tool for TaskReviewTool {
    fn name(&self) -> &str {
        "review_task"
    }

    fn description(&self) -> &str {
        "Review a task that is IN_REVIEW status. You can approve, request changes, or reject it. You cannot review your own tasks."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "ID of the task to review (must be in IN_REVIEW status)"
                },
                "action": {
                    "type": "string",
                    "enum": ["approve", "changes", "reject"],
                    "description": "Review action: approve (mark done), changes (send back for rework), reject (cancel task)"
                },
                "comment": {
                    "type": "string",
                    "description": "Review feedback explaining the decision"
                }
            },
            "required": ["task_id", "action"]
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

        let action_str = match args.get("action").and_then(serde_json::Value::as_str) {
            Some(a) => a,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'action'".into()),
                })
            }
        };

        let action = match action_str {
            "approve" => ReviewAction::Approve,
            "changes" => ReviewAction::Changes,
            "reject" => ReviewAction::Reject,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Invalid action '{}'. Must be: approve, changes, reject",
                        action_str
                    )),
                })
            }
        };

        let comment = args
            .get("comment")
            .and_then(serde_json::Value::as_str)
            .map(String::from);

        // Load the task
        let task = match tasks::get_task(&self.config, task_id) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                })
            }
        };

        // Self-review check: prevent reviewing tasks assigned to yourself
        if let Some(ref agent_id) = self.agent_id {
            if task.assignee_id.as_deref() == Some(agent_id) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Cannot review your own task. Ask another employee or a human to review it."
                            .into(),
                    ),
                });
            }
        }

        // Apply the review state machine
        let (new_status, review_status) = match state::apply_review(task.status, action) {
            Ok(result) => result,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                })
            }
        };

        let patch = TaskPatch {
            status: Some(new_status),
            review_status: Some(Some(review_status)),
            review_comment: comment.as_ref().map(|c| Some(c.clone())),
            ..TaskPatch::default()
        };

        match tasks::update_task(&self.config, task_id, &patch) {
            Ok(updated) => {
                let _ = tasks::record_event(
                    &self.config,
                    task_id,
                    TaskEventType::ReviewResponded,
                    ActorType::Employee,
                    self.agent_id.as_deref(),
                    None,
                    json!({
                        "action": action_str,
                        "new_status": new_status.as_str(),
                        "comment": comment,
                    }),
                );
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&json!({
                        "id": updated.id,
                        "title": updated.title,
                        "status": updated.status,
                        "review_status": updated.review_status,
                        "review_comment": updated.review_comment,
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
