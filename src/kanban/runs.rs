//! `task_runs` CRUD. A *run* is one attempt to execute a task — created on
//! claim, closed on complete/block/crash/timeout/spawn-failure/reclaim. Carries
//! per-attempt metadata (claim, PID, runtime cap, summary, metadata).

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kanban::errors::Result;
use crate::kanban::store::now_secs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub task_id: String,
    pub profile: Option<String>,
    pub step_key: Option<String>,
    pub status: String,
    pub claim_lock: Option<String>,
    pub claim_expires: Option<i64>,
    pub worker_pid: Option<i64>,
    pub max_runtime_seconds: Option<i64>,
    pub last_heartbeat_at: Option<i64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: Option<String>,
    pub summary: Option<String>,
    pub metadata: Option<Value>,
    pub error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn create_run(
    conn: &Connection,
    task_id: &str,
    profile: Option<&str>,
    step_key: Option<&str>,
    claim_lock: Option<&str>,
    claim_expires: Option<i64>,
    max_runtime_seconds: Option<i64>,
    started_at: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO task_runs (task_id, profile, step_key, status, claim_lock, \
         claim_expires, max_runtime_seconds, started_at) \
         VALUES (?, ?, ?, 'running', ?, ?, ?, ?)",
        params![
            task_id,
            profile,
            step_key,
            claim_lock,
            claim_expires,
            max_runtime_seconds,
            started_at
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Close the task's currently-active run (if any) with the given outcome and
/// optional handoff. Returns the run id that was closed, or `None` when there
/// was no active run.
pub fn end_run(
    conn: &Connection,
    task_id: &str,
    outcome: &str,
    status: &str,
    summary: Option<&str>,
    metadata: Option<&Value>,
    error: Option<&str>,
) -> Result<Option<i64>> {
    let run_id: Option<i64> = conn
        .query_row(
            "SELECT current_run_id FROM tasks WHERE id = ?",
            [task_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let Some(rid) = run_id else {
        return Ok(None);
    };
    let now = now_secs();
    let meta_json = match metadata {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };
    conn.execute(
        "UPDATE task_runs SET status = ?, outcome = ?, summary = COALESCE(?, summary), \
         metadata = COALESCE(?, metadata), error = COALESCE(?, error), ended_at = ?, \
         claim_lock = NULL, claim_expires = NULL, worker_pid = NULL \
         WHERE id = ? AND ended_at IS NULL",
        params![status, outcome, summary, meta_json, error, now, rid],
    )?;
    conn.execute(
        "UPDATE tasks SET current_run_id = NULL WHERE id = ?",
        [task_id],
    )?;
    Ok(Some(rid))
}

/// Insert a zero-duration `task_runs` row carrying the handoff for a task
/// that was completed/blocked without ever being claimed (e.g. human closing
/// a `ready` task from the CLI with `--summary`).
pub fn synthesize_ended_run(
    conn: &Connection,
    task_id: &str,
    outcome: &str,
    summary: Option<&str>,
    metadata: Option<&Value>,
    error: Option<&str>,
) -> Result<i64> {
    let now = now_secs();
    let meta_json = match metadata {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };
    let status = match outcome {
        "completed" => "done",
        "blocked" => "blocked",
        _ => "released",
    };
    conn.execute(
        "INSERT INTO task_runs (task_id, status, started_at, ended_at, outcome, summary, \
         metadata, error) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![task_id, status, now, now, outcome, summary, meta_json, error],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_runs(conn: &Connection, task_id: &str) -> Result<Vec<Run>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, profile, step_key, status, claim_lock, claim_expires, \
         worker_pid, max_runtime_seconds, last_heartbeat_at, started_at, ended_at, \
         outcome, summary, metadata, error \
         FROM task_runs WHERE task_id = ? ORDER BY started_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([task_id], |row| {
            let metadata_raw: Option<String> = row.get(14)?;
            let metadata = metadata_raw
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            Ok(Run {
                id: row.get(0)?,
                task_id: row.get(1)?,
                profile: row.get(2)?,
                step_key: row.get(3)?,
                status: row.get(4)?,
                claim_lock: row.get(5)?,
                claim_expires: row.get(6)?,
                worker_pid: row.get(7)?,
                max_runtime_seconds: row.get(8)?,
                last_heartbeat_at: row.get(9)?,
                started_at: row.get(10)?,
                ended_at: row.get(11)?,
                outcome: row.get(12)?,
                summary: row.get(13)?,
                metadata,
                error: row.get(15)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
