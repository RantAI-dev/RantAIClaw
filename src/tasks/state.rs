// src/tasks/state.rs
use crate::tasks::types::{ReviewAction, ReviewStatus, TaskStatus};
use anyhow::{bail, Result};

/// Validate a status transition and return the new status.
///
/// Valid transitions:
///   TODO -> IN_PROGRESS
///   IN_PROGRESS -> IN_REVIEW | DONE | CANCELLED
///   IN_REVIEW -> IN_PROGRESS (changes requested) | DONE (approved) | CANCELLED (rejected)
///   DONE -> (terminal)
///   CANCELLED -> TODO (reopen)
pub fn validate_transition(from: TaskStatus, to: TaskStatus) -> Result<()> {
    let valid = matches!(
        (from, to),
        (TaskStatus::Todo, TaskStatus::InProgress)
            | (
                TaskStatus::InProgress,
                TaskStatus::InReview | TaskStatus::Done | TaskStatus::Cancelled
            )
            | (
                TaskStatus::InReview,
                TaskStatus::InProgress | TaskStatus::Done | TaskStatus::Cancelled
            )
            | (TaskStatus::Cancelled, TaskStatus::Todo)
    );

    if !valid {
        bail!(
            "Invalid status transition: {} -> {}",
            from.as_str(),
            to.as_str()
        );
    }
    Ok(())
}

/// Apply a review action to a task that is IN_REVIEW.
/// Returns (new_status, review_status).
pub fn apply_review(
    current_status: TaskStatus,
    action: ReviewAction,
) -> Result<(TaskStatus, ReviewStatus)> {
    if current_status != TaskStatus::InReview {
        bail!(
            "Cannot review a task in status '{}', must be IN_REVIEW",
            current_status.as_str()
        );
    }

    match action {
        ReviewAction::Approve => Ok((TaskStatus::Done, ReviewStatus::Approved)),
        ReviewAction::Changes => Ok((TaskStatus::InProgress, ReviewStatus::ChangesRequested)),
        ReviewAction::Reject => Ok((TaskStatus::Cancelled, ReviewStatus::Rejected)),
    }
}

/// Check if an employee can submit a review on a task.
/// Denies if the acting employee is the designated reviewer (prevents self-review).
pub fn can_self_review(reviewer_id: Option<&str>, acting_employee_id: &str) -> bool {
    // If reviewer is the same as the actor, deny self-review
    if let Some(rev) = reviewer_id {
        if rev == acting_employee_id {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_forward_transitions() {
        assert!(validate_transition(TaskStatus::Todo, TaskStatus::InProgress).is_ok());
        assert!(validate_transition(TaskStatus::InProgress, TaskStatus::InReview).is_ok());
        assert!(validate_transition(TaskStatus::InProgress, TaskStatus::Done).is_ok());
        assert!(validate_transition(TaskStatus::InReview, TaskStatus::Done).is_ok());
        assert!(validate_transition(TaskStatus::InReview, TaskStatus::InProgress).is_ok());
    }

    #[test]
    fn valid_cancellation() {
        assert!(validate_transition(TaskStatus::InProgress, TaskStatus::Cancelled).is_ok());
        assert!(validate_transition(TaskStatus::InReview, TaskStatus::Cancelled).is_ok());
    }

    #[test]
    fn reopen_from_cancelled() {
        assert!(validate_transition(TaskStatus::Cancelled, TaskStatus::Todo).is_ok());
    }

    #[test]
    fn invalid_transitions_rejected() {
        assert!(validate_transition(TaskStatus::Todo, TaskStatus::Done).is_err());
        assert!(validate_transition(TaskStatus::Todo, TaskStatus::InReview).is_err());
        assert!(validate_transition(TaskStatus::Done, TaskStatus::InProgress).is_err());
        assert!(validate_transition(TaskStatus::Done, TaskStatus::Todo).is_err());
        assert!(validate_transition(TaskStatus::Todo, TaskStatus::Todo).is_err());
    }

    #[test]
    fn review_approve_moves_to_done() {
        let (status, review) = apply_review(TaskStatus::InReview, ReviewAction::Approve).unwrap();
        assert_eq!(status, TaskStatus::Done);
        assert_eq!(review, ReviewStatus::Approved);
    }

    #[test]
    fn review_changes_moves_to_in_progress() {
        let (status, review) = apply_review(TaskStatus::InReview, ReviewAction::Changes).unwrap();
        assert_eq!(status, TaskStatus::InProgress);
        assert_eq!(review, ReviewStatus::ChangesRequested);
    }

    #[test]
    fn review_reject_moves_to_cancelled() {
        let (status, review) = apply_review(TaskStatus::InReview, ReviewAction::Reject).unwrap();
        assert_eq!(status, TaskStatus::Cancelled);
        assert_eq!(review, ReviewStatus::Rejected);
    }

    #[test]
    fn review_requires_in_review_status() {
        assert!(apply_review(TaskStatus::Todo, ReviewAction::Approve).is_err());
        assert!(apply_review(TaskStatus::InProgress, ReviewAction::Approve).is_err());
        assert!(apply_review(TaskStatus::Done, ReviewAction::Approve).is_err());
    }

    #[test]
    fn self_review_denied_when_assigned_as_reviewer() {
        assert!(!can_self_review(Some("emp-1"), "emp-1"));
    }

    #[test]
    fn self_review_allowed_when_different_reviewer() {
        assert!(can_self_review(Some("emp-2"), "emp-1"));
    }

    #[test]
    fn self_complete_allowed_when_no_reviewer() {
        assert!(can_self_review(None, "emp-1"));
    }
}
