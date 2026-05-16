//! Build the human-readable handoff text a worker sees when it picks up a
//! task. Identical structure to Hermes `build_worker_context`:
//!
//! ```text
//! # Task: <title>
//! <body, truncated to 8 KB>
//!
//! ## Parents
//! - <parent_id>: <last completed run summary> [metadata: ...]
//!
//! ## Prior attempts on this task
//! - run #<n> (<outcome>): <summary> [metadata: ...] [error: ...]
//!
//! ## Comments (most recent <CAP> shown)
//! - <author> @ <ts>: <body>
//! ```

use rusqlite::Connection;

use crate::kanban::errors::Result;
use crate::kanban::runs::list_runs;
use crate::kanban::store::{
    get_task, list_comments, list_parent_ids, CTX_MAX_BODY_BYTES, CTX_MAX_COMMENTS,
    CTX_MAX_COMMENT_BYTES, CTX_MAX_FIELD_BYTES, CTX_MAX_PRIOR_ATTEMPTS,
};

pub fn build_worker_context(conn: &Connection, task_id: &str) -> Result<String> {
    let task = get_task(conn, task_id)?
        .ok_or_else(|| crate::kanban::errors::KanbanError::UnknownTask(task_id.to_string()))?;
    let mut out = String::new();
    out.push_str(&format!("# Task: {}\n", task.title));
    if let Some(body) = task.body.as_deref() {
        out.push_str(&truncate(body, CTX_MAX_BODY_BYTES));
        out.push('\n');
    }
    if let Some(tenant) = task.tenant.as_deref() {
        out.push_str(&format!("\nTenant: {tenant}\n"));
    }
    let parents = list_parent_ids(conn, task_id)?;
    if !parents.is_empty() {
        out.push_str("\n## Parents\n");
        for pid in &parents {
            if let Some(parent_task) = get_task(conn, pid)? {
                let title = parent_task.title;
                let runs = list_runs(conn, pid)?;
                let latest = runs
                    .iter()
                    .rev()
                    .find(|r| r.outcome.as_deref() == Some("completed"));
                let summary = latest
                    .and_then(|r| r.summary.as_deref())
                    .unwrap_or_else(|| parent_task.result.as_deref().unwrap_or(""));
                let summary = truncate(summary, CTX_MAX_FIELD_BYTES);
                out.push_str(&format!("- {pid} — {title}\n"));
                if !summary.is_empty() {
                    out.push_str(&format!("    summary: {summary}\n"));
                }
                if let Some(meta) = latest.and_then(|r| r.metadata.as_ref()) {
                    let meta_str = truncate(&meta.to_string(), CTX_MAX_FIELD_BYTES);
                    out.push_str(&format!("    metadata: {meta_str}\n"));
                }
            }
        }
    }
    let runs = list_runs(conn, task_id)?;
    let prior: Vec<_> = runs.iter().filter(|r| r.ended_at.is_some()).collect();
    let shown = prior
        .iter()
        .rev()
        .take(CTX_MAX_PRIOR_ATTEMPTS)
        .collect::<Vec<_>>();
    if !shown.is_empty() {
        out.push_str("\n## Prior attempts on this task\n");
        for r in shown {
            let outcome = r.outcome.as_deref().unwrap_or("running");
            out.push_str(&format!("- run #{} ({outcome})\n", r.id));
            if let Some(s) = r.summary.as_deref() {
                out.push_str(&format!(
                    "    summary: {}\n",
                    truncate(s, CTX_MAX_FIELD_BYTES)
                ));
            }
            if let Some(m) = r.metadata.as_ref() {
                let m = truncate(&m.to_string(), CTX_MAX_FIELD_BYTES);
                out.push_str(&format!("    metadata: {m}\n"));
            }
            if let Some(e) = r.error.as_deref() {
                out.push_str(&format!(
                    "    error: {}\n",
                    truncate(e, CTX_MAX_FIELD_BYTES)
                ));
            }
        }
    }
    let comments = list_comments(conn, task_id)?;
    let shown_comments: Vec<_> = comments
        .iter()
        .rev()
        .take(CTX_MAX_COMMENTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !shown_comments.is_empty() {
        out.push_str(&format!(
            "\n## Comments (most recent {CTX_MAX_COMMENTS} shown)\n"
        ));
        for c in shown_comments {
            let body = truncate(&c.body, CTX_MAX_COMMENT_BYTES);
            out.push_str(&format!("- {} @ {}: {body}\n", c.author, c.created_at));
        }
    }
    Ok(out)
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    // Avoid splitting a UTF-8 codepoint.
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::store::{add_comment, complete_task, create_task, CreateTaskInput};
    use rusqlite::Connection;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::kanban::schema::apply_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn context_includes_title_body_parents_comments() {
        let conn = fresh();
        let p = create_task(
            &conn,
            &CreateTaskInput {
                title: "parent".into(),
                ..Default::default()
            },
        )
        .unwrap();
        complete_task(
            &conn,
            &p,
            Some("did the thing"),
            Some("parent summary"),
            None,
        )
        .unwrap();
        let c = create_task(
            &conn,
            &CreateTaskInput {
                title: "child".into(),
                body: Some("body of child".into()),
                parents: vec![p.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        add_comment(&conn, &c, "you", "looks good").unwrap();
        let ctx = build_worker_context(&conn, &c).unwrap();
        assert!(ctx.contains("# Task: child"));
        assert!(ctx.contains("body of child"));
        assert!(ctx.contains("## Parents"));
        assert!(ctx.contains(&p));
        assert!(ctx.contains("## Comments"));
        assert!(ctx.contains("looks good"));
    }
}
