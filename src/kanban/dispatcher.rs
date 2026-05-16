//! Tick loop that reclaims stale claims, promotes ready tasks, atomically
//! claims, and (when wired up) spawns workers. Designed to run inside the
//! gateway process so a single dispatcher sweeps all boards per tick.
//!
//! ### Worker spawn extension point
//!
//! `Dispatcher::set_spawner` accepts a callable that receives the just-claimed
//! task. Hermes shells out to `hermes -p <profile>` here; rantaiclaw's worker
//! model is still being shaped, so the dispatcher ships without a default
//! spawner. The claim is still durable — when a spawner is set, every claim is
//! handed to it. When no spawner is set, the dispatcher leaves the task in
//! `running` and the operator can complete/block via CLI.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{watch, Notify};
use tracing::{debug, warn};

use crate::kanban::errors::Result;
use crate::kanban::events::EventKind;
use crate::kanban::store::{
    append_event, claim_task, connect, list_tasks, recompute_ready, release_stale_claims,
    ListFilter,
};

pub type WorkerSpawnFn = dyn Fn(&str) -> Result<()> + Send + Sync;

#[derive(Debug, Clone)]
pub struct DispatcherOptions {
    pub board: Option<String>,
    pub interval: Duration,
    pub max_claims_per_tick: usize,
    pub failure_limit: u32,
}

impl Default for DispatcherOptions {
    fn default() -> Self {
        Self {
            board: None,
            interval: Duration::from_secs(60),
            max_claims_per_tick: 8,
            failure_limit: 2,
        }
    }
}

pub struct Dispatcher {
    opts: DispatcherOptions,
    spawner: Mutex<Option<Arc<WorkerSpawnFn>>>,
}

impl Dispatcher {
    pub fn new(opts: DispatcherOptions) -> Self {
        Self {
            opts,
            spawner: Mutex::new(None),
        }
    }

    pub fn set_spawner<F>(&self, spawner: F)
    where
        F: Fn(&str) -> Result<()> + Send + Sync + 'static,
    {
        *self.spawner.lock() = Some(Arc::new(spawner));
    }

    /// Run one tick — used by tests and by `rantaiclaw kanban dispatch`.
    pub fn tick(&self) -> Result<TickReport> {
        let conn = connect(self.opts.board.as_deref())?;
        let mut report = TickReport::default();
        report.reclaimed = release_stale_claims(&conn)?;
        report.promoted = recompute_ready(&conn)?;
        let ready = list_tasks(
            &conn,
            &ListFilter {
                status: Some("ready".to_string()),
                limit: Some(self.opts.max_claims_per_tick as i64 * 4),
                ..Default::default()
            },
        )?;
        let spawner = self.spawner.lock().clone();
        for task in ready.into_iter().take(self.opts.max_claims_per_tick) {
            match claim_task(&conn, &task.id, None, None)? {
                Some(_) => {
                    report.claimed += 1;
                    if let Some(s) = spawner.as_ref() {
                        if let Err(err) = s(&task.id) {
                            warn!(task_id = %task.id, error = %err, "worker spawn failed");
                            // Record failure and bump the breaker
                            let _ = append_event(
                                &conn,
                                &task.id,
                                EventKind::SpawnFailed,
                                Some(serde_json::json!({"error": err.to_string()})),
                                None,
                            );
                            record_spawn_failure(
                                &conn,
                                &task.id,
                                &err.to_string(),
                                self.opts.failure_limit,
                            )?;
                        } else {
                            append_event(&conn, &task.id, EventKind::Spawned, None, None)?;
                        }
                    } else {
                        debug!(task_id = %task.id, "no spawner configured; task stays in running");
                    }
                }
                None => continue,
            }
        }
        Ok(report)
    }
}

#[derive(Debug, Clone, Default)]
pub struct TickReport {
    pub reclaimed: usize,
    pub promoted: usize,
    pub claimed: usize,
}

/// Increment the failure counter and trip the breaker (auto-block) once the
/// task has hit `failure_limit` consecutive failures.
fn record_spawn_failure(
    conn: &rusqlite::Connection,
    task_id: &str,
    err: &str,
    failure_limit: u32,
) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET consecutive_failures = consecutive_failures + 1, \
         last_failure_error = ?, status = 'ready', claim_lock = NULL, claim_expires = NULL, \
         current_run_id = NULL WHERE id = ?",
        rusqlite::params![err, task_id],
    )?;
    let failures: i64 = conn.query_row(
        "SELECT consecutive_failures FROM tasks WHERE id = ?",
        [task_id],
        |row| row.get(0),
    )?;
    let max_retries: Option<i64> = conn
        .query_row(
            "SELECT max_retries FROM tasks WHERE id = ?",
            [task_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let trip = max_retries.unwrap_or(failure_limit as i64);
    if failures >= trip {
        conn.execute(
            "UPDATE tasks SET status = 'blocked' WHERE id = ?",
            [task_id],
        )?;
        append_event(
            conn,
            task_id,
            EventKind::GaveUp,
            Some(serde_json::json!({"failures": failures, "error": err})),
            None,
        )?;
    }
    Ok(())
}

/// Background tokio handle. Drop to stop.
pub struct DispatcherHandle {
    notify: Arc<Notify>,
    _stop_tx: watch::Sender<bool>,
}

impl DispatcherHandle {
    pub fn spawn(dispatcher: Arc<Dispatcher>) -> Self {
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let notify = Arc::new(Notify::new());
        let notify_inner = notify.clone();
        let interval = dispatcher.opts.interval;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => {
                        if *stop_rx.borrow() {
                            break;
                        }
                    }
                    _ = notify_inner.notified() => {}
                    _ = tokio::time::sleep(interval) => {}
                }
                if let Err(e) = dispatcher.tick() {
                    warn!(error = %e, "kanban dispatcher tick failed");
                }
            }
        });
        Self {
            notify,
            _stop_tx: stop_tx,
        }
    }

    /// Wake the loop right now (used by CLI `nudge`).
    pub fn nudge(&self) {
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::store::{create_task, get_task, CreateTaskInput};
    use crate::kanban::test_env::with_temp_home;

    #[test]
    fn tick_with_no_spawner_claims_task() {
        with_temp_home(|_| {
            let dispatcher = Dispatcher::new(DispatcherOptions::default());
            let conn = connect(None).unwrap();
            let id = create_task(
                &conn,
                &CreateTaskInput {
                    title: "x".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let report = dispatcher.tick().unwrap();
            assert!(report.claimed >= 1);
            let t = get_task(&conn, &id).unwrap().unwrap();
            assert_eq!(t.status, "running");
        });
    }
}
