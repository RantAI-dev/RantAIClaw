//! Kanban kernel — CRUD + lifecycle on a single board's SQLite DB.
//!
//! Every write goes through `write_txn` which wraps the body in `BEGIN
//! IMMEDIATE` so concurrent writers serialize at the SQLite WAL lock. All
//! status transitions use compare-and-swap UPDATE statements (matching the
//! `WHERE status = '<expected>'` pattern) so losers observe `rowcount == 0`
//! and move on without retry loops.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::kanban::errors::{KanbanError, Result};
use crate::kanban::events::EventKind;
use crate::kanban::paths::kanban_db_path;
use crate::kanban::runs::{end_run, synthesize_ended_run};
use crate::kanban::schema::{apply_schema, valid_status, valid_workspace_kind};

/// 15 minutes — matches Hermes `DEFAULT_CLAIM_TTL_SECONDS`.
pub const DEFAULT_CLAIM_TTL_SECONDS: i64 = 15 * 60;

/// Worker-context caps so downstream readers stay bounded.
pub const CTX_MAX_PRIOR_ATTEMPTS: usize = 10;
pub const CTX_MAX_COMMENTS: usize = 30;
pub const CTX_MAX_FIELD_BYTES: usize = 4 * 1024;
pub const CTX_MAX_BODY_BYTES: usize = 8 * 1024;
pub const CTX_MAX_COMMENT_BYTES: usize = 2 * 1024;

// ──────────────────────────────────────────────────────────────────────────
// Types
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub assignee: Option<String>,
    pub status: String,
    pub priority: i64,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub workspace_kind: String,
    pub workspace_path: Option<String>,
    pub claim_lock: Option<String>,
    pub claim_expires: Option<i64>,
    pub tenant: Option<String>,
    pub result: Option<String>,
    pub idempotency_key: Option<String>,
    pub consecutive_failures: i64,
    pub worker_pid: Option<i64>,
    pub last_failure_error: Option<String>,
    pub max_runtime_seconds: Option<i64>,
    pub last_heartbeat_at: Option<i64>,
    pub current_run_id: Option<i64>,
    pub workflow_template_id: Option<String>,
    pub current_step_key: Option<String>,
    pub skills: Option<Vec<String>>,
    pub max_retries: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: i64,
    pub task_id: String,
    pub author: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub task_id: String,
    pub run_id: Option<i64>,
    pub kind: String,
    pub payload: Option<Value>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListFilter {
    pub assignee: Option<String>,
    pub status: Option<String>,
    pub tenant: Option<String>,
    pub include_archived: bool,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateTaskInput {
    pub title: String,
    pub body: Option<String>,
    pub assignee: Option<String>,
    pub created_by: Option<String>,
    pub workspace_kind: Option<String>,
    pub workspace_path: Option<String>,
    pub tenant: Option<String>,
    pub priority: Option<i64>,
    pub parents: Vec<String>,
    pub triage: bool,
    pub idempotency_key: Option<String>,
    pub max_runtime_seconds: Option<i64>,
    pub skills: Option<Vec<String>>,
    pub max_retries: Option<i64>,
}

// ──────────────────────────────────────────────────────────────────────────
// Connection
// ──────────────────────────────────────────────────────────────────────────

static INITIALIZED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

pub fn connect(board: Option<&str>) -> Result<Connection> {
    let path = kanban_db_path(board);
    open_at(&path)
}

pub fn open_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let cache = INITIALIZED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = cache.lock().expect("kanban INITIALIZED mutex poisoned");
    if guard.insert(resolved) {
        apply_schema(&conn)?;
    }
    Ok(conn)
}

/// Public entry point for `rantaiclaw kanban init`. Always re-runs migration.
pub fn init_db(board: Option<&str>) -> Result<PathBuf> {
    let path = kanban_db_path(board);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
    )?;
    apply_schema(&conn)?;
    Ok(path)
}

// ──────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn new_task_id() -> String {
    let bytes: [u8; 8] = rand::random();
    format!("t_{}", hex::encode(bytes))
}

/// Stable, host-local id used as `claim_lock`. Format: `<hostname>:<pid>:<rand>`.
pub(crate) fn claimer_id() -> String {
    let host = hostname::get()
        .ok()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let pid = std::process::id();
    let nonce: [u8; 4] = rand::random();
    format!("{host}:{pid}:{}", hex::encode(nonce))
}

fn canonical_assignee(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(s.to_string())
    }
}

pub(crate) fn write_txn<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(v) => {
            conn.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

pub(crate) fn append_event(
    conn: &Connection,
    task_id: &str,
    kind: EventKind,
    payload: Option<Value>,
    run_id: Option<i64>,
) -> Result<i64> {
    let payload_json = match payload {
        Some(v) => Some(serde_json::to_string(&v)?),
        None => None,
    };
    conn.execute(
        "INSERT INTO task_events (task_id, run_id, kind, payload, created_at) VALUES (?, ?, ?, ?, ?)",
        params![task_id, run_id, kind.as_str(), payload_json, now_secs()],
    )?;
    Ok(conn.last_insert_rowid())
}

fn find_missing_parents(conn: &Connection, parents: &[String]) -> Result<Vec<String>> {
    if parents.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = std::iter::repeat("?")
        .take(parents.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM tasks WHERE id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let present: HashSet<String> = stmt
        .query_map(params_from_iter(parents.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    Ok(parents
        .iter()
        .filter(|p| !present.contains(*p))
        .cloned()
        .collect())
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let skills_raw: Option<String> = row.get("skills")?;
    let skills = skills_raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());
    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        body: row.get("body")?,
        assignee: row.get("assignee")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        created_by: row.get("created_by")?,
        created_at: row.get("created_at")?,
        started_at: row.get("started_at")?,
        completed_at: row.get("completed_at")?,
        workspace_kind: row.get("workspace_kind")?,
        workspace_path: row.get("workspace_path")?,
        claim_lock: row.get("claim_lock")?,
        claim_expires: row.get("claim_expires")?,
        tenant: row.get("tenant")?,
        result: row.get("result")?,
        idempotency_key: row.get("idempotency_key")?,
        consecutive_failures: row.get("consecutive_failures")?,
        worker_pid: row.get("worker_pid")?,
        last_failure_error: row.get("last_failure_error")?,
        max_runtime_seconds: row.get("max_runtime_seconds")?,
        last_heartbeat_at: row.get("last_heartbeat_at")?,
        current_run_id: row.get("current_run_id")?,
        workflow_template_id: row.get("workflow_template_id")?,
        current_step_key: row.get("current_step_key")?,
        skills,
        max_retries: row.get("max_retries")?,
    })
}

// ──────────────────────────────────────────────────────────────────────────
// Create / read / list
// ──────────────────────────────────────────────────────────────────────────

pub fn create_task(conn: &Connection, input: &CreateTaskInput) -> Result<String> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err(KanbanError::MissingTitle);
    }
    let workspace_kind = input.workspace_kind.as_deref().unwrap_or("scratch");
    if !valid_workspace_kind(workspace_kind) {
        return Err(KanbanError::InvalidWorkspaceKind(
            workspace_kind.to_string(),
        ));
    }
    let parents: Vec<String> = input
        .parents
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();
    let assignee = canonical_assignee(input.assignee.as_deref());

    if let Some(key) = input.idempotency_key.as_deref() {
        if let Some(existing) = conn
            .query_row(
                "SELECT id FROM tasks WHERE idempotency_key = ? AND status != 'archived' \
                 ORDER BY created_at DESC LIMIT 1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .ok()
        {
            return Ok(existing);
        }
    }

    let now = now_secs();
    let skills_json = match &input.skills {
        Some(list) => Some(serde_json::to_string(list)?),
        None => None,
    };

    for attempt in 0..2 {
        let task_id = new_task_id();
        let parents_arg = parents.clone();
        let result = write_txn(conn, |conn| {
            let initial_status = if input.triage {
                "triage".to_string()
            } else {
                let mut s = "ready".to_string();
                if !parents_arg.is_empty() {
                    let missing = find_missing_parents(conn, &parents_arg)?;
                    if !missing.is_empty() {
                        return Err(KanbanError::UnknownParents(missing.join(", ")));
                    }
                    let placeholders = std::iter::repeat("?")
                        .take(parents_arg.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!("SELECT status FROM tasks WHERE id IN ({placeholders})");
                    let mut stmt = conn.prepare(&sql)?;
                    let statuses: Vec<String> = stmt
                        .query_map(params_from_iter(parents_arg.iter()), |row| row.get(0))?
                        .filter_map(std::result::Result::ok)
                        .collect();
                    if statuses.iter().any(|s| s != "done") {
                        s = "todo".to_string();
                    }
                }
                s
            };
            if input.triage && !parents_arg.is_empty() {
                let missing = find_missing_parents(conn, &parents_arg)?;
                if !missing.is_empty() {
                    return Err(KanbanError::UnknownParents(missing.join(", ")));
                }
            }
            conn.execute(
                "INSERT INTO tasks (id, title, body, assignee, status, priority, created_by, \
                 created_at, workspace_kind, workspace_path, tenant, idempotency_key, \
                 max_runtime_seconds, skills, max_retries) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    task_id,
                    title,
                    input.body,
                    assignee,
                    initial_status,
                    input.priority.unwrap_or(0),
                    input.created_by,
                    now,
                    workspace_kind,
                    input.workspace_path,
                    input.tenant,
                    input.idempotency_key,
                    input.max_runtime_seconds,
                    skills_json,
                    input.max_retries,
                ],
            )?;
            for pid in &parents_arg {
                conn.execute(
                    "INSERT OR IGNORE INTO task_links (parent_id, child_id) VALUES (?, ?)",
                    params![pid, task_id],
                )?;
            }
            append_event(
                conn,
                &task_id,
                EventKind::Created,
                Some(json!({
                    "assignee": assignee,
                    "status": initial_status,
                    "parents": parents_arg,
                    "tenant": input.tenant,
                    "skills": input.skills,
                })),
                None,
            )?;
            Ok(task_id.clone())
        });
        match result {
            Ok(id) => return Ok(id),
            Err(KanbanError::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == rusqlite::ErrorCode::ConstraintViolation && attempt == 0 =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("retry loop above exits on Ok or Err")
}

pub fn get_task(conn: &Connection, task_id: &str) -> Result<Option<Task>> {
    let mut stmt = conn.prepare("SELECT * FROM tasks WHERE id = ?")?;
    let mut rows = stmt.query([task_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_task(row)?))
    } else {
        Ok(None)
    }
}

pub fn list_tasks(conn: &Connection, filter: &ListFilter) -> Result<Vec<Task>> {
    let mut sql = String::from("SELECT * FROM tasks WHERE 1=1");
    let mut params_dyn: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(a) = filter.assignee.as_deref() {
        sql.push_str(" AND assignee = ?");
        params_dyn.push(Box::new(canonical_assignee(Some(a))));
    }
    if let Some(s) = filter.status.as_deref() {
        if !valid_status(s) {
            return Err(KanbanError::InvalidStatus(s.to_string()));
        }
        sql.push_str(" AND status = ?");
        params_dyn.push(Box::new(s.to_string()));
    }
    if let Some(t) = filter.tenant.as_deref() {
        sql.push_str(" AND tenant = ?");
        params_dyn.push(Box::new(t.to_string()));
    }
    if !filter.include_archived && filter.status.as_deref() != Some("archived") {
        sql.push_str(" AND status != 'archived'");
    }
    sql.push_str(" ORDER BY priority DESC, created_at ASC");
    if let Some(l) = filter.limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params_dyn.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(refs), row_to_task)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn assign_task(conn: &Connection, task_id: &str, profile: Option<&str>) -> Result<bool> {
    let assignee = canonical_assignee(profile);
    write_txn(conn, |conn| {
        let updated = conn.execute(
            "UPDATE tasks SET assignee = ? WHERE id = ? AND claim_lock IS NULL",
            params![assignee, task_id],
        )?;
        if updated == 1 {
            append_event(
                conn,
                task_id,
                EventKind::Assigned,
                Some(json!({"assignee": assignee})),
                None,
            )?;
        }
        Ok(updated == 1)
    })
}

// ──────────────────────────────────────────────────────────────────────────
// Links
// ──────────────────────────────────────────────────────────────────────────

pub fn add_link(conn: &Connection, parent_id: &str, child_id: &str) -> Result<bool> {
    if parent_id == child_id {
        return Ok(false);
    }
    write_txn(conn, |conn| {
        if get_task(conn, parent_id)?.is_none() || get_task(conn, child_id)?.is_none() {
            return Err(KanbanError::UnknownTask(
                if get_task(conn, parent_id)?.is_none() {
                    parent_id.to_string()
                } else {
                    child_id.to_string()
                },
            ));
        }
        // Cycle check: walking up from parent should never reach child.
        let mut frontier = vec![parent_id.to_string()];
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(node) = frontier.pop() {
            if !seen.insert(node.clone()) {
                continue;
            }
            if node == child_id {
                return Ok(false);
            }
            let mut stmt = conn.prepare("SELECT parent_id FROM task_links WHERE child_id = ?")?;
            let next: Vec<String> = stmt
                .query_map([&node], |row| row.get::<_, String>(0))?
                .filter_map(std::result::Result::ok)
                .collect();
            frontier.extend(next);
        }
        let updated = conn.execute(
            "INSERT OR IGNORE INTO task_links (parent_id, child_id) VALUES (?, ?)",
            params![parent_id, child_id],
        )?;
        Ok(updated > 0)
    })
}

pub fn remove_link(conn: &Connection, parent_id: &str, child_id: &str) -> Result<bool> {
    let updated = conn.execute(
        "DELETE FROM task_links WHERE parent_id = ? AND child_id = ?",
        params![parent_id, child_id],
    )?;
    Ok(updated > 0)
}

// ──────────────────────────────────────────────────────────────────────────
// Comments / events
// ──────────────────────────────────────────────────────────────────────────

pub fn add_comment(conn: &Connection, task_id: &str, author: &str, body: &str) -> Result<i64> {
    write_txn(conn, |conn| {
        let now = now_secs();
        conn.execute(
            "INSERT INTO task_comments (task_id, author, body, created_at) VALUES (?, ?, ?, ?)",
            params![task_id, author, body, now],
        )?;
        let id = conn.last_insert_rowid();
        append_event(
            conn,
            task_id,
            EventKind::Edited,
            Some(json!({"fields": ["comment"]})),
            None,
        )?;
        Ok(id)
    })
}

pub fn list_comments(conn: &Connection, task_id: &str) -> Result<Vec<Comment>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, author, body, created_at FROM task_comments \
         WHERE task_id = ? ORDER BY created_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([task_id], |row| {
            Ok(Comment {
                id: row.get(0)?,
                task_id: row.get(1)?,
                author: row.get(2)?,
                body: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn list_events(conn: &Connection, task_id: &str) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, run_id, kind, payload, created_at FROM task_events \
         WHERE task_id = ? ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map([task_id], |row| {
            let payload_raw: Option<String> = row.get(4)?;
            let payload = payload_raw
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            Ok(Event {
                id: row.get(0)?,
                task_id: row.get(1)?,
                run_id: row.get(2)?,
                kind: row.get(3)?,
                payload,
                created_at: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn list_parent_ids(conn: &Connection, child_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT parent_id FROM task_links WHERE child_id = ?")?;
    let rows = stmt
        .query_map([child_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn list_child_ids(conn: &Connection, parent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT child_id FROM task_links WHERE parent_id = ?")?;
    let rows = stmt
        .query_map([parent_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ──────────────────────────────────────────────────────────────────────────
// Lifecycle: claim / heartbeat / complete / block / unblock / archive
// ──────────────────────────────────────────────────────────────────────────

pub fn claim_task(
    conn: &Connection,
    task_id: &str,
    ttl_seconds: Option<i64>,
    claimer: Option<&str>,
) -> Result<Option<Task>> {
    let now = now_secs();
    let ttl = ttl_seconds.unwrap_or(DEFAULT_CLAIM_TTL_SECONDS);
    let lock = claimer.map_or_else(claimer_id, str::to_string);
    let expires = now + ttl;
    write_txn(conn, |conn| {
        let undone: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM task_links l JOIN tasks p ON p.id = l.parent_id \
                 WHERE l.child_id = ? AND p.status NOT IN ('done', 'archived') LIMIT 1",
                [task_id],
                |row| row.get(0),
            )
            .ok();
        if undone.is_some() {
            conn.execute(
                "UPDATE tasks SET status = 'todo' WHERE id = ? AND status = 'ready'",
                [task_id],
            )?;
            append_event(
                conn,
                task_id,
                EventKind::ClaimRejected,
                Some(json!({"reason": "parents_not_done"})),
                None,
            )?;
            return Ok(None);
        }
        let updated = conn.execute(
            "UPDATE tasks SET status = 'running', claim_lock = ?, claim_expires = ?, \
             started_at = COALESCE(started_at, ?) \
             WHERE id = ? AND status = 'ready' AND claim_lock IS NULL",
            params![lock, expires, now, task_id],
        )?;
        if updated != 1 {
            return Ok(None);
        }
        let row = conn.query_row(
            "SELECT assignee, max_runtime_seconds, current_step_key FROM tasks WHERE id = ?",
            [task_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )?;
        let run_id = crate::kanban::runs::create_run(
            conn,
            task_id,
            row.0.as_deref(),
            row.2.as_deref(),
            Some(&lock),
            Some(expires),
            row.1,
            now,
        )?;
        conn.execute(
            "UPDATE tasks SET current_run_id = ? WHERE id = ?",
            params![run_id, task_id],
        )?;
        append_event(
            conn,
            task_id,
            EventKind::Claimed,
            Some(json!({"lock": lock, "expires": expires, "run_id": run_id})),
            Some(run_id),
        )?;
        Ok(get_task(conn, task_id)?)
    })
}

pub fn heartbeat_claim(
    conn: &Connection,
    task_id: &str,
    ttl_seconds: Option<i64>,
    claimer: Option<&str>,
) -> Result<bool> {
    let expires = now_secs() + ttl_seconds.unwrap_or(DEFAULT_CLAIM_TTL_SECONDS);
    let lock = claimer.map_or_else(claimer_id, str::to_string);
    write_txn(conn, |conn| {
        let updated = conn.execute(
            "UPDATE tasks SET claim_expires = ?, last_heartbeat_at = ? \
             WHERE id = ? AND status = 'running' AND claim_lock = ?",
            params![expires, now_secs(), task_id, lock],
        )?;
        if updated == 1 {
            if let Some(run_id) = current_run_id(conn, task_id)? {
                conn.execute(
                    "UPDATE task_runs SET claim_expires = ?, last_heartbeat_at = ? WHERE id = ?",
                    params![expires, now_secs(), run_id],
                )?;
            }
            append_event(
                conn,
                task_id,
                EventKind::Heartbeat,
                None,
                current_run_id(conn, task_id)?,
            )?;
            Ok(true)
        } else {
            Ok(false)
        }
    })
}

fn current_run_id(conn: &Connection, task_id: &str) -> Result<Option<i64>> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT current_run_id FROM tasks WHERE id = ?",
            [task_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    Ok(id)
}

pub fn complete_task(
    conn: &Connection,
    task_id: &str,
    result_text: Option<&str>,
    summary: Option<&str>,
    metadata: Option<&Value>,
) -> Result<bool> {
    let now = now_secs();
    let handoff_summary = summary.or(result_text);
    let did_complete = write_txn(conn, |conn| {
        let updated = conn.execute(
            "UPDATE tasks SET status = 'done', result = ?, completed_at = ?, \
             claim_lock = NULL, claim_expires = NULL, worker_pid = NULL \
             WHERE id = ? AND status IN ('running', 'ready', 'blocked')",
            params![result_text, now, task_id],
        )?;
        if updated != 1 {
            return Ok(false);
        }
        let mut run_id = end_run(
            conn,
            task_id,
            "completed",
            "done",
            handoff_summary,
            metadata,
            None,
        )?;
        if run_id.is_none() && (summary.is_some() || metadata.is_some() || result_text.is_some()) {
            run_id = Some(synthesize_ended_run(
                conn,
                task_id,
                "completed",
                handoff_summary,
                metadata,
                None,
            )?);
        }
        let payload = json!({
            "result_len": result_text.map(str::len).unwrap_or(0),
            "summary": handoff_summary
                .map(|s| {
                    let line = s.lines().next().unwrap_or("").to_string();
                    if line.len() > 400 { line[..400].to_string() } else { line }
                }),
        });
        append_event(conn, task_id, EventKind::Completed, Some(payload), run_id)?;
        clear_failure_counter(conn, task_id)?;
        Ok(true)
    })?;
    if did_complete {
        recompute_ready(conn)?;
    }
    Ok(did_complete)
}

pub fn block_task(conn: &Connection, task_id: &str, reason: Option<&str>) -> Result<bool> {
    write_txn(conn, |conn| {
        let updated = conn.execute(
            "UPDATE tasks SET status = 'blocked', claim_lock = NULL, claim_expires = NULL, \
             worker_pid = NULL WHERE id = ? AND status IN ('running', 'ready')",
            [task_id],
        )?;
        if updated != 1 {
            return Ok(false);
        }
        let mut run_id = end_run(conn, task_id, "blocked", "blocked", reason, None, None)?;
        if run_id.is_none() && reason.is_some() {
            run_id = Some(synthesize_ended_run(
                conn, task_id, "blocked", reason, None, None,
            )?);
        }
        append_event(
            conn,
            task_id,
            EventKind::Blocked,
            Some(json!({"reason": reason})),
            run_id,
        )?;
        Ok(true)
    })
}

pub fn unblock_task(conn: &Connection, task_id: &str) -> Result<bool> {
    write_txn(conn, |conn| {
        let undone: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM task_links l JOIN tasks p ON p.id = l.parent_id \
                 WHERE l.child_id = ? AND p.status != 'done' LIMIT 1",
                [task_id],
                |row| row.get(0),
            )
            .ok();
        let new_status = if undone.is_some() { "todo" } else { "ready" };
        let updated = conn.execute(
            "UPDATE tasks SET status = ?, current_run_id = NULL \
             WHERE id = ? AND status = 'blocked'",
            params![new_status, task_id],
        )?;
        if updated != 1 {
            return Ok(false);
        }
        let payload = if new_status == "ready" {
            None
        } else {
            Some(json!({"status": new_status}))
        };
        append_event(conn, task_id, EventKind::Unblocked, payload, None)?;
        Ok(true)
    })
}

pub fn archive_task(conn: &Connection, task_id: &str) -> Result<bool> {
    write_txn(conn, |conn| {
        let updated = conn.execute(
            "UPDATE tasks SET status = 'archived', claim_lock = NULL, claim_expires = NULL \
             WHERE id = ? AND status != 'archived'",
            [task_id],
        )?;
        if updated == 1 {
            append_event(conn, task_id, EventKind::Archived, None, None)?;
        }
        Ok(updated == 1)
    })
}

pub fn clear_failure_counter(conn: &Connection, task_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET consecutive_failures = 0, last_failure_error = NULL WHERE id = ?",
        [task_id],
    )?;
    Ok(())
}

/// Promote any `todo` task whose parents are all `done` to `ready`. Returns
/// the number of promotions.
pub fn recompute_ready(conn: &Connection) -> Result<usize> {
    let mut promoted = 0usize;
    write_txn(conn, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id FROM tasks WHERE status = 'todo' AND id NOT IN \
             (SELECT child_id FROM task_links l JOIN tasks p ON p.id = l.parent_id \
              WHERE p.status NOT IN ('done', 'archived'))",
        )?;
        let candidates: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(std::result::Result::ok)
            .collect();
        for id in candidates {
            let updated = conn.execute(
                "UPDATE tasks SET status = 'ready' WHERE id = ? AND status = 'todo'",
                [&id],
            )?;
            if updated == 1 {
                promoted += 1;
                append_event(conn, &id, EventKind::Promoted, None, None)?;
            }
        }
        Ok(())
    })?;
    Ok(promoted)
}

/// Re-queue any `running` task whose claim TTL has expired. Hermes parity:
/// alive PID → claim extended, dead PID → reclaimed.
pub fn release_stale_claims(conn: &Connection) -> Result<usize> {
    let now = now_secs();
    let mut reclaimed = 0usize;
    let stale: Vec<(String, Option<String>, Option<i64>, Option<i64>)> = conn
        .prepare(
            "SELECT id, claim_lock, worker_pid, claim_expires FROM tasks \
             WHERE status = 'running' AND claim_expires IS NOT NULL AND claim_expires < ?",
        )?
        .query_map([now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    for (id, lock, pid, _) in stale {
        let alive = pid.map_or(false, |p| pid_alive(p as i32));
        let host_local = lock
            .as_deref()
            .is_some_and(|l| l.starts_with(&format!("{}:", host_prefix())));
        if alive && host_local {
            let new_expires = now + DEFAULT_CLAIM_TTL_SECONDS;
            write_txn(conn, |conn| {
                let updated = conn.execute(
                    "UPDATE tasks SET claim_expires = ? WHERE id = ? AND status = 'running' \
                     AND claim_lock IS ? AND claim_expires IS NOT NULL AND claim_expires < ?",
                    params![new_expires, id, lock, now],
                )?;
                if updated == 1 {
                    if let Some(rid) = current_run_id(conn, &id)? {
                        conn.execute(
                            "UPDATE task_runs SET claim_expires = ? WHERE id = ?",
                            params![new_expires, rid],
                        )?;
                    }
                    append_event(
                        conn,
                        &id,
                        EventKind::ClaimExtended,
                        Some(json!({"new_expires": new_expires, "pid_alive": true})),
                        None,
                    )?;
                }
                Ok(())
            })?;
        } else {
            write_txn(conn, |conn| {
                let updated = conn.execute(
                    "UPDATE tasks SET status = 'ready', claim_lock = NULL, \
                     claim_expires = NULL, worker_pid = NULL, current_run_id = NULL \
                     WHERE id = ? AND status = 'running'",
                    [&id],
                )?;
                if updated == 1 {
                    reclaimed += 1;
                    end_run(conn, &id, "reclaimed", "reclaimed", None, None, None)?;
                    append_event(
                        conn,
                        &id,
                        EventKind::Reclaimed,
                        Some(json!({"stale_lock": lock})),
                        None,
                    )?;
                }
                Ok(())
            })?;
        }
    }
    Ok(reclaimed)
}

fn host_prefix() -> String {
    hostname::get()
        .ok()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // `kill -0` does not actually signal — it just checks reachability.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    // Best-effort fallback on non-Unix: assume the worker is alive so we
    // never falsely reclaim. Hermes equivalent ships only on Unix anyway.
    true
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::schema::apply_schema;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn create_and_get() {
        let conn = fresh();
        let id = create_task(
            &conn,
            &CreateTaskInput {
                title: "first task".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let task = get_task(&conn, &id).unwrap().unwrap();
        assert_eq!(task.title, "first task");
        assert_eq!(task.status, "ready");
    }

    #[test]
    fn parent_gates_status() {
        let conn = fresh();
        let p = create_task(
            &conn,
            &CreateTaskInput {
                title: "parent".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let c = create_task(
            &conn,
            &CreateTaskInput {
                title: "child".into(),
                parents: vec![p.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        let child = get_task(&conn, &c).unwrap().unwrap();
        assert_eq!(child.status, "todo");
        // complete parent → recompute promotes child
        complete_task(&conn, &p, Some("ok"), None, None).unwrap();
        let child = get_task(&conn, &c).unwrap().unwrap();
        assert_eq!(child.status, "ready");
    }

    #[test]
    fn claim_is_mutually_exclusive() {
        let conn = fresh();
        let id = create_task(
            &conn,
            &CreateTaskInput {
                title: "race".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let claimed = claim_task(&conn, &id, None, Some("a")).unwrap();
        assert!(claimed.is_some());
        let second = claim_task(&conn, &id, None, Some("b")).unwrap();
        assert!(second.is_none());
    }

    #[test]
    fn complete_and_block_emit_events() {
        let conn = fresh();
        let id = create_task(
            &conn,
            &CreateTaskInput {
                title: "x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        complete_task(&conn, &id, Some("done"), Some("worker did work"), None).unwrap();
        let evs = list_events(&conn, &id).unwrap();
        assert!(evs.iter().any(|e| e.kind == "completed"));
    }

    #[test]
    fn idempotency_key_returns_same_id() {
        let conn = fresh();
        let a = create_task(
            &conn,
            &CreateTaskInput {
                title: "k".into(),
                idempotency_key: Some("nightly".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let b = create_task(
            &conn,
            &CreateTaskInput {
                title: "k".into(),
                idempotency_key: Some("nightly".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn comments_round_trip() {
        let conn = fresh();
        let id = create_task(
            &conn,
            &CreateTaskInput {
                title: "c".into(),
                ..Default::default()
            },
        )
        .unwrap();
        add_comment(&conn, &id, "you", "looks good").unwrap();
        let cs = list_comments(&conn, &id).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].body, "looks good");
    }

    #[test]
    fn link_rejects_cycle() {
        let conn = fresh();
        let a = create_task(
            &conn,
            &CreateTaskInput {
                title: "a".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let b = create_task(
            &conn,
            &CreateTaskInput {
                title: "b".into(),
                parents: vec![a.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        // b is child of a — adding a as child of b would form a cycle.
        let ok = add_link(&conn, &b, &a).unwrap();
        assert!(!ok);
    }
}
