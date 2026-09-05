// src/gateway/task_handlers.rs
//! Axum handlers for the task engine gateway API.
//!
//! Routes:
//!   GET    /tasks           — list tasks (query params for filtering)
//!   POST   /tasks           — create task
//!   GET    /tasks/{id}      — get task detail
//!   PUT    /tasks/{id}      — update task
//!   DELETE /tasks/{id}      — delete task
//!   POST   /tasks/{id}/review — submit review
//!   GET    /tasks/{id}/comments — list comments
//!   POST   /tasks/{id}/comments — add comment
//!   GET    /tasks/{id}/events   — list events

use super::AppState;
use crate::tasks::{
    self, state, ActorType, CreateTask, ReviewRequest, TaskEventType, TaskFilter, TaskPatch,
};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Json,
};
use serde::Deserialize;

/// Response type alias for all task handlers.
type TaskResponse = (StatusCode, Json<serde_json::Value>);

fn require_auth(state: &AppState, headers: &HeaderMap) -> Option<TaskResponse> {
    if !state.pairing.require_pairing() {
        return None;
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    if !state.pairing.is_authenticated(token) {
        return Some((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            })),
        ));
    }
    None
}

fn err_disabled() -> TaskResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "Task engine is disabled"})),
    )
}

/// Strip anything that tells a caller where the operator's files are.
///
/// Every error message this module returns is built from an `anyhow` chain, and
/// `tasks::store::open` wraps *any* failure with `Failed to open tasks DB at
/// <absolute path>` — so the leak is not confined to the 500s. A malformed id
/// reaches `err_bad_request` and a missing row reaches `err_not_found` through
/// the same `open()`, which is why all three constructors scrub rather than
/// just the internal one. On a static literal the pass is a no-op.
fn scrub(msg: &str) -> String {
    crate::providers::sanitize_api_error(&super::api_v1::redact_profile_paths(msg))
}

fn err_bad_request(msg: &str) -> TaskResponse {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": scrub(msg)})),
    )
}

fn err_not_found(msg: &str) -> TaskResponse {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": scrub(msg)})),
    )
}

fn err_internal(msg: &str) -> TaskResponse {
    // Full chain server-side, scrubbed detail to the caller — the same split
    // `api_v1::err_500` makes.
    tracing::error!(error = %msg, "tasks internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": scrub(msg)})),
    )
}

// ── Query params ─────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct TaskListQuery {
    pub status: Option<String>,
    pub assignee_id: Option<String>,
    pub group_id: Option<String>,
    pub priority: Option<String>,
    pub parent_task_id: Option<String>,
    pub top_level_only: Option<bool>,
    pub organization_id: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

impl TaskListQuery {
    fn to_filter(&self) -> Result<TaskFilter, String> {
        Ok(TaskFilter {
            status: self
                .status
                .as_deref()
                .map(tasks::TaskStatus::try_from)
                .transpose()?,
            assignee_id: self.assignee_id.clone(),
            group_id: self.group_id.clone(),
            priority: self
                .priority
                .as_deref()
                .map(tasks::TaskPriority::try_from)
                .transpose()?,
            parent_task_id: self.parent_task_id.clone(),
            top_level_only: self.top_level_only,
            organization_id: self.organization_id.clone(),
            limit: self.limit,
            offset: self.offset,
        })
    }
}

// ── Handlers ─────────────────────────────────────────────────

pub async fn handle_list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TaskListQuery>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    let filter = match query.to_filter() {
        Ok(f) => f,
        Err(e) => return err_bad_request(&e),
    };

    match tasks::list_tasks(&config, &filter) {
        Ok(list) => (StatusCode::OK, Json(serde_json::json!(list))),
        Err(e) => err_internal(&e.to_string()),
    }
}

pub async fn handle_create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateTask>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    if body.title.trim().is_empty() {
        return err_bad_request("Title is required");
    }

    match tasks::create_task(&config, &body) {
        Ok(task) => (StatusCode::CREATED, Json(serde_json::json!(task))),
        Err(e) => err_internal(&e.to_string()),
    }
}

pub async fn handle_get_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    match tasks::get_task_detail(&config, &id) {
        Ok(detail) => (StatusCode::OK, Json(serde_json::json!(detail))),
        Err(e) => err_not_found(&e.to_string()),
    }
}

pub async fn handle_update_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<TaskPatch>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    // Validate status transition if status is being changed
    if let Some(ref new_status) = patch.status {
        match tasks::get_task(&config, &id) {
            Ok(existing) => {
                if let Err(e) = state::validate_transition(existing.status, *new_status) {
                    return err_bad_request(&e.to_string());
                }
            }
            Err(e) => {
                return err_not_found(&e.to_string());
            }
        }
    }

    match tasks::update_task(&config, &id, &patch) {
        Ok(task) => (StatusCode::OK, Json(serde_json::json!(task))),
        Err(e) => err_internal(&e.to_string()),
    }
}

pub async fn handle_delete_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    match tasks::delete_task(&config, &id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"deleted": id}))),
        Err(e) => err_not_found(&e.to_string()),
    }
}

pub async fn handle_review_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(review): Json<ReviewRequest>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    let task = match tasks::get_task(&config, &id) {
        Ok(t) => t,
        Err(e) => return err_not_found(&e.to_string()),
    };

    // Prevent self-review: if the reviewer is also the acting employee
    if let Some(ref actor_emp_id) = review.actor_employee_id {
        if !state::can_self_review(task.reviewer_id.as_deref(), actor_emp_id) {
            return err_bad_request("Cannot review your own task when assigned as reviewer");
        }
    }

    let (new_status, review_status) = match state::apply_review(task.status, review.action) {
        Ok(result) => result,
        Err(e) => return err_bad_request(&e.to_string()),
    };

    let patch = TaskPatch {
        status: Some(new_status),
        review_status: Some(Some(review_status)),
        review_comment: review.comment.as_ref().map(|c: &String| Some(c.clone())),
        ..TaskPatch::default()
    };

    match tasks::update_task(&config, &id, &patch) {
        Ok(updated) => {
            // Record review event (best-effort)
            let actor_type = review.actor_type.unwrap_or(ActorType::Human);
            let action_str = review.action.as_str();
            let status_str = new_status.as_str();
            let _ = tasks::record_event(
                &config,
                &id,
                TaskEventType::ReviewResponded,
                actor_type,
                review.actor_employee_id.as_deref(),
                review.actor_user_id.as_deref(),
                serde_json::json!({
                    "action": action_str,
                    "new_status": status_str,
                    "comment": review.comment,
                }),
            );
            (StatusCode::OK, Json(serde_json::json!(updated)))
        }
        Err(e) => err_internal(&e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddCommentBody {
    pub content: String,
    pub author_type: Option<String>,
    pub author_employee_id: Option<String>,
    pub author_user_id: Option<String>,
}

pub async fn handle_list_comments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    match tasks::list_comments(&config, &id) {
        Ok(comments) => (StatusCode::OK, Json(serde_json::json!(comments))),
        Err(e) => err_internal(&e.to_string()),
    }
}

pub async fn handle_add_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AddCommentBody>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    if body.content.trim().is_empty() {
        return err_bad_request("Content is required");
    }

    let author_type = body
        .author_type
        .as_deref()
        .and_then(|s| ActorType::try_from(s).ok())
        .unwrap_or(ActorType::Human);

    match tasks::add_comment(
        &config,
        &id,
        &body.content,
        author_type,
        body.author_employee_id.as_deref(),
        body.author_user_id.as_deref(),
    ) {
        Ok(comment) => (StatusCode::CREATED, Json(serde_json::json!(comment))),
        Err(e) => err_internal(&e.to_string()),
    }
}

pub async fn handle_list_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> TaskResponse {
    if let Some(err) = require_auth(&state, &headers) {
        return err;
    }
    let config = state.config.lock();
    if !config.tasks.enabled {
        return err_disabled();
    }

    match tasks::list_events(&config, &id) {
        Ok(events) => (StatusCode::OK, Json(serde_json::json!(events))),
        Err(e) => err_internal(&e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{HomeGuard, ENV_LOCK};

    /// Build the message `tasks::store::open` actually produces on failure, so
    /// the assertion is against a real leak and not an invented one.
    fn db_open_error(home: &std::path::Path) -> String {
        format!(
            "Failed to open tasks DB at {}/.rantaiclaw/profiles/default/workspace/tasks.db: unable to open database file",
            home.display()
        )
    }

    fn leaked_path(body: &serde_json::Value, home: &std::path::Path) -> bool {
        body["error"]
            .as_str()
            .expect("error is a string")
            .contains(home.to_str().expect("utf-8 temp path"))
    }

    // One test per constructor, not one aggregate: the three reach the caller
    // through different status codes, and covering only the 500 is how the leak
    // survived in `err_not_found` in the first place.

    #[tokio::test]
    async fn err_internal_returns_no_filesystem_path() {
        let _lock = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(tmp.path());

        let (status, Json(body)) = err_internal(&db_open_error(tmp.path()));

        assert_eq!(status.as_u16(), 500);
        assert!(!leaked_path(&body, tmp.path()), "body was {body}");
    }

    #[tokio::test]
    async fn err_not_found_returns_no_filesystem_path() {
        let _lock = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(tmp.path());

        let (status, Json(body)) = err_not_found(&db_open_error(tmp.path()));

        assert_eq!(status.as_u16(), 404);
        assert!(!leaked_path(&body, tmp.path()), "body was {body}");
    }

    #[tokio::test]
    async fn err_bad_request_returns_no_filesystem_path() {
        let _lock = ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::set(tmp.path());

        let (status, Json(body)) = err_bad_request(&db_open_error(tmp.path()));

        assert_eq!(status.as_u16(), 400);
        assert!(!leaked_path(&body, tmp.path()), "body was {body}");
    }

    #[test]
    fn a_static_message_survives_the_scrub_unchanged() {
        assert_eq!(scrub("Title is required"), "Title is required");
    }

    // `scrub` is two passes, and redaction is only one of them. Without these
    // the second pass could be deleted with every other test still green.

    #[test]
    fn scrub_removes_a_secret_looking_token() {
        let out = scrub("upstream rejected sk-abcdef0123456789 while opening the task store");
        assert!(!out.contains("sk-abcdef0123456789"), "was: {out}");
    }

    #[test]
    fn scrub_truncates_an_oversized_message() {
        let out = scrub(&"x".repeat(400));
        assert!(
            out.chars().count() <= 203,
            "was {} chars",
            out.chars().count()
        );
    }

    /// The contract is not "the three constructors scrub" — it is "no error
    /// body in this module is built anywhere else". A handler that inlines
    /// a bare `(404, Json(json!({"error": e.to_string()})))` tuple would
    /// pass every test above and reintroduce the leak, so assert the whole
    /// surface: each of these status codes is spelled exactly once, inside its
    /// scrubbing constructor. The tests above deliberately compare `as_u16()`
    /// so they do not count against it.
    #[test]
    fn error_statuses_are_built_only_by_the_scrubbing_constructors() {
        let src = include_str!("task_handlers.rs");
        // Assembled at runtime: a verbatim literal here would be counted by the
        // very scan it feeds, and the assertion would be measuring itself.
        for variant in ["BAD_REQUEST", "NOT_FOUND", "INTERNAL_SERVER_ERROR"] {
            let needle = format!("StatusCode::{variant}");
            assert_eq!(
                src.matches(needle.as_str()).count(),
                1,
                "{needle} is built outside its scrubbing constructor"
            );
        }
    }
}
