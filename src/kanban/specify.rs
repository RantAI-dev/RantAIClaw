//! Triage specifier — turn a one-line triage card into a real `todo` task.
//!
//! Hermes uses an auxiliary LLM here to flesh out title + body with goal /
//! approach / acceptance criteria. This port keeps the same callable shape
//! but ships a deterministic stub today: it expands the body into a
//! structured template and promotes the task. A subsequent PR can wire an
//! auxiliary provider in (the entry point already accepts an optional
//! "specifier" callback).

use rusqlite::{params, Connection};

use crate::kanban::errors::{KanbanError, Result};
use crate::kanban::events::EventKind;
use crate::kanban::store::{append_event, get_task, write_txn};

/// Result of one specify pass.
#[derive(Debug, Clone)]
pub struct SpecifyOutcome {
    pub task_id: String,
    pub ok: bool,
    pub reason: Option<String>,
    pub new_title: Option<String>,
}

/// Trait abstraction so a future provider implementation can replace the
/// deterministic stub without touching callers.
pub trait Specifier {
    fn specify(&self, title: &str, body: Option<&str>) -> SpecifierOutput;
}

#[derive(Debug, Clone)]
pub struct SpecifierOutput {
    pub new_title: String,
    pub new_body: String,
}

/// Deterministic fallback used when no `Specifier` is wired in. Produces a
/// "spec needed" template that a human can fill in.
pub struct TemplateSpecifier;

impl Specifier for TemplateSpecifier {
    fn specify(&self, title: &str, body: Option<&str>) -> SpecifierOutput {
        let new_body = format!(
            "Goal:\n  {}\n\nApproach:\n  - TBD\n\nAcceptance criteria:\n  - TBD\n\nNotes:\n  {}\n",
            title,
            body.unwrap_or("(triage one-liner — needs spec)")
        );
        SpecifierOutput {
            new_title: title.trim().to_string(),
            new_body,
        }
    }
}

pub fn specify_triage(
    conn: &Connection,
    task_id: &str,
    specifier: &dyn Specifier,
    author: Option<&str>,
) -> Result<SpecifyOutcome> {
    let task =
        get_task(conn, task_id)?.ok_or_else(|| KanbanError::UnknownTask(task_id.to_string()))?;
    if task.status != "triage" {
        return Ok(SpecifyOutcome {
            task_id: task_id.to_string(),
            ok: false,
            reason: Some(format!(
                "task not in triage (current status: {})",
                task.status
            )),
            new_title: None,
        });
    }
    let out = specifier.specify(&task.title, task.body.as_deref());
    write_txn(conn, |conn| {
        conn.execute(
            "UPDATE tasks SET title = ?, body = ?, status = 'todo' WHERE id = ? AND status = 'triage'",
            params![out.new_title, out.new_body, task_id],
        )?;
        append_event(
            conn,
            task_id,
            EventKind::Edited,
            Some(serde_json::json!({
                "fields": ["title", "body", "status"],
                "specifier": "template",
                "author": author,
            })),
            None,
        )?;
        Ok(())
    })?;
    // Once promoted to todo, recompute_ready picks it up if no parents are
    // pending.
    crate::kanban::store::recompute_ready(conn)?;
    Ok(SpecifyOutcome {
        task_id: task_id.to_string(),
        ok: true,
        reason: None,
        new_title: Some(out.new_title),
    })
}

/// Sweep every triage task (optionally filtered by tenant) through the
/// specifier.
pub fn specify_all(
    conn: &Connection,
    specifier: &dyn Specifier,
    tenant: Option<&str>,
    author: Option<&str>,
) -> Result<Vec<SpecifyOutcome>> {
    let mut sql = String::from("SELECT id FROM tasks WHERE status = 'triage'");
    let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(t) = tenant {
        sql.push_str(" AND tenant = ?");
        params_dyn.push(Box::new(t.to_string()));
    }
    let mut stmt = conn.prepare(&sql)?;
    let ids: Vec<String> = {
        let refs: Vec<&dyn rusqlite::ToSql> = params_dyn.iter().map(|b| b.as_ref()).collect();
        stmt.query_map(rusqlite::params_from_iter(refs), |row| row.get(0))?
            .filter_map(std::result::Result::ok)
            .collect()
    };
    let mut out = Vec::new();
    for id in ids {
        out.push(specify_triage(conn, &id, specifier, author)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::store::{create_task, get_task, CreateTaskInput};

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::kanban::schema::apply_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn specify_promotes_triage_to_ready_when_no_parents() {
        let conn = fresh();
        let id = create_task(
            &conn,
            &CreateTaskInput {
                title: "ship dark mode".into(),
                triage: true,
                ..Default::default()
            },
        )
        .unwrap();
        let out = specify_triage(&conn, &id, &TemplateSpecifier, Some("alifia")).unwrap();
        assert!(out.ok);
        let t = get_task(&conn, &id).unwrap().unwrap();
        // Specifier sets status='todo'; recompute_ready then promotes to 'ready'
        // because the task has no parents. Hermes does the same.
        assert_eq!(t.status, "ready");
        assert!(t.body.unwrap().contains("Goal:"));
    }

    #[test]
    fn specify_no_op_on_non_triage() {
        let conn = fresh();
        let id = create_task(
            &conn,
            &CreateTaskInput {
                title: "x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let out = specify_triage(&conn, &id, &TemplateSpecifier, None).unwrap();
        assert!(!out.ok);
    }
}
