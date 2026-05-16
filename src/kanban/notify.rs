//! Gateway → task subscription table. A row pairs a chat (platform + chat +
//! thread) with a task; the notifier loop tails `task_events` and pushes one
//! message per terminal event back to that chat.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::kanban::errors::Result;
use crate::kanban::store::now_secs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifySubscription {
    pub task_id: String,
    pub platform: String,
    pub chat_id: String,
    pub thread_id: String,
    pub user_id: Option<String>,
    pub notifier_profile: Option<String>,
    pub created_at: i64,
    pub last_event_id: i64,
}

#[derive(Debug, Clone, Default)]
pub struct SubscribeInput<'a> {
    pub task_id: &'a str,
    pub platform: &'a str,
    pub chat_id: &'a str,
    pub thread_id: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub notifier_profile: Option<&'a str>,
}

pub fn subscribe(conn: &Connection, input: &SubscribeInput<'_>) -> Result<()> {
    let thread_id = input.thread_id.unwrap_or("");
    conn.execute(
        "INSERT OR REPLACE INTO kanban_notify_subs \
         (task_id, platform, chat_id, thread_id, user_id, notifier_profile, created_at, \
          last_event_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, COALESCE((SELECT last_event_id FROM kanban_notify_subs \
              WHERE task_id = ? AND platform = ? AND chat_id = ? AND thread_id = ?), 0))",
        params![
            input.task_id,
            input.platform,
            input.chat_id,
            thread_id,
            input.user_id,
            input.notifier_profile,
            now_secs(),
            input.task_id,
            input.platform,
            input.chat_id,
            thread_id,
        ],
    )?;
    Ok(())
}

pub fn unsubscribe(
    conn: &Connection,
    task_id: &str,
    platform: &str,
    chat_id: &str,
    thread_id: Option<&str>,
) -> Result<bool> {
    let thread_id = thread_id.unwrap_or("");
    let updated = conn.execute(
        "DELETE FROM kanban_notify_subs WHERE task_id = ? AND platform = ? AND chat_id = ? \
         AND thread_id = ?",
        params![task_id, platform, chat_id, thread_id],
    )?;
    Ok(updated > 0)
}

pub fn list_subscriptions(
    conn: &Connection,
    task_id: Option<&str>,
) -> Result<Vec<NotifySubscription>> {
    let (sql, ids) = match task_id {
        Some(t) => (
            "SELECT task_id, platform, chat_id, thread_id, user_id, notifier_profile, \
             created_at, last_event_id FROM kanban_notify_subs WHERE task_id = ? \
             ORDER BY created_at ASC"
                .to_string(),
            vec![t.to_string()],
        ),
        None => (
            "SELECT task_id, platform, chat_id, thread_id, user_id, notifier_profile, \
             created_at, last_event_id FROM kanban_notify_subs ORDER BY created_at ASC"
                .to_string(),
            vec![],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<NotifySubscription> {
        Ok(NotifySubscription {
            task_id: row.get(0)?,
            platform: row.get(1)?,
            chat_id: row.get(2)?,
            thread_id: row.get(3)?,
            user_id: row.get(4)?,
            notifier_profile: row.get(5)?,
            created_at: row.get(6)?,
            last_event_id: row.get(7)?,
        })
    };
    let rows = if ids.is_empty() {
        stmt.query_map([], map)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(rusqlite::params_from_iter(ids.iter()), map)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

pub fn advance_last_event_id(
    conn: &Connection,
    task_id: &str,
    platform: &str,
    chat_id: &str,
    thread_id: &str,
    last_event_id: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE kanban_notify_subs SET last_event_id = ? \
         WHERE task_id = ? AND platform = ? AND chat_id = ? AND thread_id = ?",
        params![last_event_id, task_id, platform, chat_id, thread_id],
    )?;
    Ok(())
}

pub fn purge_for_terminal_task(conn: &Connection, task_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM kanban_notify_subs WHERE task_id = ?",
        [task_id],
    )?;
    Ok(())
}
