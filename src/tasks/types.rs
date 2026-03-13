// src/tasks/types.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Status enum ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    #[default]
    Todo,
    InProgress,
    InReview,
    Done,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Todo => "TODO",
            Self::InProgress => "IN_PROGRESS",
            Self::InReview => "IN_REVIEW",
            Self::Done => "DONE",
            Self::Cancelled => "CANCELLED",
        }
    }
}

impl TryFrom<&str> for TaskStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "TODO" => Ok(Self::Todo),
            "IN_PROGRESS" => Ok(Self::InProgress),
            "IN_REVIEW" => Ok(Self::InReview),
            "DONE" => Ok(Self::Done),
            "CANCELLED" => Ok(Self::Cancelled),
            _ => Err(format!(
                "Invalid task status '{}'. Expected: TODO, IN_PROGRESS, IN_REVIEW, DONE, CANCELLED",
                value
            )),
        }
    }
}

// ── Priority enum ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskPriority {
    Low,
    #[default]
    Medium,
    High,
}

impl TaskPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::Medium => "MEDIUM",
            Self::High => "HIGH",
        }
    }
}

impl TryFrom<&str> for TaskPriority {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "LOW" => Ok(Self::Low),
            "MEDIUM" => Ok(Self::Medium),
            "HIGH" => Ok(Self::High),
            _ => Err(format!(
                "Invalid priority '{}'. Expected: LOW, MEDIUM, HIGH",
                value
            )),
        }
    }
}

// ── Review status ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewStatus {
    Pending,
    Approved,
    ChangesRequested,
    Rejected,
}

impl ReviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Approved => "APPROVED",
            Self::ChangesRequested => "CHANGES_REQUESTED",
            Self::Rejected => "REJECTED",
        }
    }
}

impl TryFrom<&str> for ReviewStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "PENDING" => Ok(Self::Pending),
            "APPROVED" => Ok(Self::Approved),
            "CHANGES_REQUESTED" => Ok(Self::ChangesRequested),
            "REJECTED" => Ok(Self::Rejected),
            _ => Err(format!(
                "Invalid review status '{}'. Expected: PENDING, APPROVED, CHANGES_REQUESTED, REJECTED",
                value
            )),
        }
    }
}

// ── Actor type ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActorType {
    Human,
    Employee,
}

impl ActorType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Human => "HUMAN",
            Self::Employee => "EMPLOYEE",
        }
    }
}

impl TryFrom<&str> for ActorType {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "HUMAN" => Ok(Self::Human),
            "EMPLOYEE" => Ok(Self::Employee),
            _ => Err(format!(
                "Invalid actor type '{}'. Expected: HUMAN, EMPLOYEE",
                value
            )),
        }
    }
}

// ── Event type ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskEventType {
    Created,
    StatusChanged,
    Assigned,
    ReviewSubmitted,
    ReviewResponded,
    Comment,
    SubtaskCompleted,
}

impl TaskEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::StatusChanged => "STATUS_CHANGED",
            Self::Assigned => "ASSIGNED",
            Self::ReviewSubmitted => "REVIEW_SUBMITTED",
            Self::ReviewResponded => "REVIEW_RESPONDED",
            Self::Comment => "COMMENT",
            Self::SubtaskCompleted => "SUBTASK_COMPLETED",
        }
    }
}

impl TryFrom<&str> for TaskEventType {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "CREATED" => Ok(Self::Created),
            "STATUS_CHANGED" => Ok(Self::StatusChanged),
            "ASSIGNED" => Ok(Self::Assigned),
            "REVIEW_SUBMITTED" => Ok(Self::ReviewSubmitted),
            "REVIEW_RESPONDED" => Ok(Self::ReviewResponded),
            "COMMENT" => Ok(Self::Comment),
            "SUBTASK_COMPLETED" => Ok(Self::SubtaskCompleted),
            _ => Err(format!("Invalid event type '{}'", value)),
        }
    }
}

// ── Core structs ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub organization_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    pub priority: TaskPriority,
    pub assignee_id: Option<String>,
    pub group_id: Option<String>,
    pub reviewer_id: Option<String>,
    pub human_review: bool,
    pub review_status: Option<ReviewStatus>,
    pub review_comment: Option<String>,
    pub parent_task_id: Option<String>,
    pub created_by_employee_id: Option<String>,
    pub created_by_user_id: Option<String>,
    pub due_date: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub order_in_status: i32,
    pub order_in_parent: i32,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskComment {
    pub id: String,
    pub task_id: String,
    pub content: String,
    pub author_type: ActorType,
    pub author_employee_id: Option<String>,
    pub author_user_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub id: String,
    pub task_id: String,
    pub event_type: TaskEventType,
    pub actor_type: ActorType,
    pub actor_employee_id: Option<String>,
    pub actor_user_id: Option<String>,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Full task detail including subtasks, comments, and events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub subtasks: Vec<Task>,
    pub comments: Vec<TaskComment>,
    pub events: Vec<TaskEvent>,
}

// ── Create / Update DTOs ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTask {
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<TaskPriority>,
    pub assignee_id: Option<String>,
    pub group_id: Option<String>,
    pub reviewer_id: Option<String>,
    pub human_review: Option<bool>,
    pub parent_task_id: Option<String>,
    pub due_date: Option<DateTime<Utc>>,
    pub organization_id: Option<String>,
    pub created_by_employee_id: Option<String>,
    pub created_by_user_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<TaskStatus>,
    pub priority: Option<TaskPriority>,
    pub assignee_id: Option<Option<String>>,
    pub group_id: Option<Option<String>>,
    pub reviewer_id: Option<Option<String>>,
    pub human_review: Option<bool>,
    pub review_status: Option<Option<ReviewStatus>>,
    pub review_comment: Option<Option<String>>,
    pub due_date: Option<Option<DateTime<Utc>>>,
    pub order_in_status: Option<i32>,
    pub order_in_parent: Option<i32>,
    pub metadata: Option<serde_json::Value>,
}

// ── Review action ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewAction {
    Approve,
    Changes,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub action: ReviewAction,
    pub comment: Option<String>,
    pub actor_type: Option<ActorType>,
    pub actor_employee_id: Option<String>,
    pub actor_user_id: Option<String>,
}

// ── List filters ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskFilter {
    pub status: Option<TaskStatus>,
    pub assignee_id: Option<String>,
    pub group_id: Option<String>,
    pub priority: Option<TaskPriority>,
    pub parent_task_id: Option<String>,
    /// If true, only return top-level tasks (parent_task_id IS NULL)
    pub top_level_only: Option<bool>,
    pub organization_id: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_roundtrip() {
        for status in [
            TaskStatus::Todo,
            TaskStatus::InProgress,
            TaskStatus::InReview,
            TaskStatus::Done,
            TaskStatus::Cancelled,
        ] {
            let s = status.as_str();
            let parsed = TaskStatus::try_from(s).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn task_status_case_insensitive() {
        assert_eq!(TaskStatus::try_from("todo").unwrap(), TaskStatus::Todo);
        assert_eq!(
            TaskStatus::try_from("in_progress").unwrap(),
            TaskStatus::InProgress
        );
    }

    #[test]
    fn task_status_rejects_invalid() {
        assert!(TaskStatus::try_from("INVALID").is_err());
        assert!(TaskStatus::try_from("").is_err());
    }

    #[test]
    fn task_priority_roundtrip() {
        for priority in [TaskPriority::Low, TaskPriority::Medium, TaskPriority::High] {
            let s = priority.as_str();
            let parsed = TaskPriority::try_from(s).unwrap();
            assert_eq!(parsed, priority);
        }
    }

    #[test]
    fn review_status_roundtrip() {
        for status in [
            ReviewStatus::Pending,
            ReviewStatus::Approved,
            ReviewStatus::ChangesRequested,
            ReviewStatus::Rejected,
        ] {
            let s = status.as_str();
            let parsed = ReviewStatus::try_from(s).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn actor_type_roundtrip() {
        assert_eq!(
            ActorType::try_from("HUMAN").unwrap(),
            ActorType::Human
        );
        assert_eq!(
            ActorType::try_from("EMPLOYEE").unwrap(),
            ActorType::Employee
        );
    }

    #[test]
    fn task_serialization_roundtrip() {
        let task = Task {
            id: "test-id".into(),
            organization_id: None,
            title: "Test task".into(),
            description: Some("A test".into()),
            status: TaskStatus::Todo,
            priority: TaskPriority::High,
            assignee_id: None,
            group_id: None,
            reviewer_id: None,
            human_review: false,
            review_status: None,
            review_comment: None,
            parent_task_id: None,
            created_by_employee_id: Some("emp-1".into()),
            created_by_user_id: None,
            due_date: None,
            completed_at: None,
            order_in_status: 0,
            order_in_parent: 0,
            metadata: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-id");
        assert_eq!(parsed.status, TaskStatus::Todo);
        assert_eq!(parsed.priority, TaskPriority::High);
    }

    #[test]
    fn create_task_minimal() {
        let ct: CreateTask = serde_json::from_str(r#"{"title":"Do thing"}"#).unwrap();
        assert_eq!(ct.title, "Do thing");
        assert!(ct.description.is_none());
        assert!(ct.priority.is_none());
    }
}
