//! Gateway notifier — tails `task_events` and forwards terminal events to
//! channels subscribed via `notify_subscribe`. The actual channel send is
//! deferred to a `Delivery` trait so we don't bake the channel registry into
//! the kanban module; rantaiclaw's existing channel adapters can implement it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::Connection;
use tokio::sync::watch;
use tracing::warn;

use crate::kanban::errors::Result;
use crate::kanban::events::TASK_TERMINAL_EVENT_KINDS;
use crate::kanban::notify::{
    advance_last_event_id, list_subscriptions, purge_for_terminal_task, NotifySubscription,
};
use crate::kanban::store::connect;

#[async_trait]
pub trait Delivery: Send + Sync {
    async fn deliver(
        &self,
        subscription: &NotifySubscription,
        event_kind: &str,
        message: &str,
    ) -> std::result::Result<(), String>;
}

/// In-memory no-op delivery used for tests and for installs that haven't
/// wired up a gateway yet.
pub struct NoopDelivery;

#[async_trait]
impl Delivery for NoopDelivery {
    async fn deliver(
        &self,
        _subscription: &NotifySubscription,
        _event_kind: &str,
        _message: &str,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
}

pub struct Notifier {
    board: Option<String>,
    delivery: Arc<dyn Delivery>,
    interval: Duration,
}

impl Notifier {
    pub fn new(board: Option<String>, delivery: Arc<dyn Delivery>) -> Self {
        Self {
            board,
            delivery,
            interval: Duration::from_secs(5),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub async fn tick(&self) -> Result<usize> {
        let conn = connect(self.board.as_deref())?;
        let subs = list_subscriptions(&conn, None)?;
        let mut delivered = 0usize;
        for sub in subs {
            let new_events = events_since(&conn, &sub.task_id, sub.last_event_id)?;
            for (event_id, kind, payload) in new_events {
                if TASK_TERMINAL_EVENT_KINDS.contains(&kind.as_str()) {
                    let line = format_terminal(&sub.task_id, &kind, payload.as_deref());
                    if let Err(e) = self.delivery.deliver(&sub, &kind, &line).await {
                        warn!(task_id=%sub.task_id, error=%e, "kanban delivery failed");
                        break;
                    }
                    delivered += 1;
                }
                advance_last_event_id(
                    &conn,
                    &sub.task_id,
                    &sub.platform,
                    &sub.chat_id,
                    &sub.thread_id,
                    event_id,
                )?;
                if matches!(kind.as_str(), "completed" | "archived") {
                    purge_for_terminal_task(&conn, &sub.task_id)?;
                    break;
                }
            }
        }
        Ok(delivered)
    }
}

fn events_since(
    conn: &Connection,
    task_id: &str,
    last_event_id: i64,
) -> Result<Vec<(i64, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, payload FROM task_events WHERE task_id = ? AND id > ? ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![task_id, last_event_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn format_terminal(task_id: &str, kind: &str, payload: Option<&str>) -> String {
    let summary = payload
        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .and_then(|v| {
            v.get("summary")
                .and_then(|s| s.as_str().map(str::to_string))
        });
    let glyph = match kind {
        "completed" => "✓",
        "blocked" => "■",
        "gave_up" => "✗",
        "crashed" => "💥",
        "timed_out" => "⏱",
        _ => "·",
    };
    match summary {
        Some(s) if !s.is_empty() => format!("{glyph} {kind} {task_id} — {s}"),
        _ => format!("{glyph} {kind} {task_id}"),
    }
}

pub struct NotifierHandle {
    _stop_tx: watch::Sender<bool>,
    _running: Arc<Mutex<bool>>,
}

impl NotifierHandle {
    pub fn spawn(notifier: Arc<Notifier>) -> Self {
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let running = Arc::new(Mutex::new(true));
        let running_inner = running.clone();
        let interval = notifier.interval;
        tokio::spawn(async move {
            while !*stop_rx.borrow() {
                if let Err(e) = notifier.tick().await {
                    warn!(error = %e, "kanban notifier tick failed");
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = stop_rx.changed() => break,
                }
            }
            *running_inner.lock() = false;
        });
        Self {
            _stop_tx: stop_tx,
            _running: running,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::notify::{subscribe, SubscribeInput};
    use crate::kanban::store::{complete_task, create_task, CreateTaskInput};
    use std::sync::Mutex as StdMutex;

    struct CollectDelivery {
        out: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl Delivery for CollectDelivery {
        async fn deliver(
            &self,
            _sub: &NotifySubscription,
            kind: &str,
            message: &str,
        ) -> std::result::Result<(), String> {
            self.out.lock().unwrap().push(format!("{kind}: {message}"));
            Ok(())
        }
    }

    #[test]
    fn delivers_completion_event() {
        let tmp = tempfile::tempdir().unwrap();
        crate::kanban::test_env::with_home(tmp.path(), || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let conn = connect(None).unwrap();
                let id = create_task(
                    &conn,
                    &CreateTaskInput {
                        title: "n".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
                subscribe(
                    &conn,
                    &SubscribeInput {
                        task_id: &id,
                        platform: "test",
                        chat_id: "chat-1",
                        thread_id: None,
                        user_id: None,
                        notifier_profile: None,
                    },
                )
                .unwrap();
                complete_task(&conn, &id, Some("ok"), Some("worker done"), None).unwrap();
                let delivery = Arc::new(CollectDelivery {
                    out: StdMutex::new(Vec::new()),
                });
                let notifier = Notifier::new(None, delivery.clone());
                notifier.tick().await.unwrap();
                let v = delivery.out.lock().unwrap();
                assert!(v.iter().any(|m| m.contains("completed")));
            });
        });
    }
}
