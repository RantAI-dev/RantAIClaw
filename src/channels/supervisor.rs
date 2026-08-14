//! Listener supervision: the per-channel single-runner lock, restart/backoff,
//! the typing indicator task and the in-flight accounting the dispatch loop
//! uses to interrupt a sender's previous turn.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 7). No behaviour change; the
//! tests stayed with the dispatch fixtures they share, so the moved items are
//! `pub(crate)`.

use super::traits::{self, Channel};
use super::{
    CHANNEL_HEALTH_HEARTBEAT_SECS, CHANNEL_MAX_IN_FLIGHT_MESSAGES, CHANNEL_MIN_IN_FLIGHT_MESSAGES,
    CHANNEL_PARALLELISM_PER_CHANNEL, CHANNEL_TYPING_REFRESH_INTERVAL_SECS,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct InFlightSenderTaskState {
    pub(crate) task_id: u64,
    pub(crate) cancellation: CancellationToken,
    pub(crate) completion: Arc<InFlightTaskCompletion>,
}

pub(crate) struct InFlightTaskCompletion {
    pub(crate) done: AtomicBool,
    pub(crate) notify: tokio::sync::Notify,
}

impl InFlightTaskCompletion {
    pub(crate) fn new() -> Self {
        Self {
            done: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        // Register the waiter BEFORE re-checking `done`.
        //
        // `notify_waiters()` stores no permit — it only wakes waiters that are
        // already registered — and `notified()` does not register until first
        // polled. Checking `done` and then awaiting left a window where a
        // `mark_done()` on another worker landed between the two and was lost,
        // parking this sender's next message forever.
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.done.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// Marks an [`InFlightTaskCompletion`] done on drop, including on unwind.
///
/// `mark_done()` used to be the last statement of the worker closure, so a panic
/// anywhere in the message path — provider, tool loop, renderer, a channel's
/// `send` — skipped it. The next message from that sender then waited on a
/// signal that would never come, and the worker's semaphore permit was never
/// released. After enough of those the dispatch loop stops draining its queue
/// and **every** channel goes quiet, with nothing logging a deadlock because the
/// task never finishes.
pub(crate) struct CompletionGuard(pub(crate) Arc<InFlightTaskCompletion>);

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.mark_done();
    }
}

/// Outcome of trying to claim the per-channel single-runner lock.
pub(crate) enum ChannelLock {
    /// Lock held — keep the `File` alive for the listener's lifetime.
    Acquired(std::fs::File),
    /// Another live process already runs this channel — skip the listener.
    HeldByOther,
    /// Lock infrastructure unavailable (no data dir / IO error) — fail open
    /// and run anyway; the guard is best-effort, not a hard gate.
    Unavailable,
}

/// Claim an exclusive advisory lock for `channel` under the shared data dir
/// (`<data>/locks/channel-<name>.lock`). The lock is global (the WhatsApp
/// session and Telegram bot token are shared resources), so only one process
/// runs a given channel at a time. Released automatically on drop / exit.
pub(crate) fn acquire_channel_lock(channel: &str) -> ChannelLock {
    use fs2::FileExt;
    let Some(dirs) = directories::ProjectDirs::from("", "", "rantaiclaw") else {
        return ChannelLock::Unavailable;
    };
    let lock_dir = dirs.data_dir().join("locks");
    if std::fs::create_dir_all(&lock_dir).is_err() {
        return ChannelLock::Unavailable;
    }
    let lock_path = lock_dir.join(format!("channel-{channel}.lock"));
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return ChannelLock::Unavailable;
    };
    match file.try_lock_exclusive() {
        Ok(()) => ChannelLock::Acquired(file),
        Err(_) => ChannelLock::HeldByOther,
    }
}

pub(crate) fn spawn_supervised_listener(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    spawn_supervised_listener_with_health_interval(
        ch,
        tx,
        initial_backoff_secs,
        max_backoff_secs,
        Duration::from_secs(CHANNEL_HEALTH_HEARTBEAT_SECS),
        shutdown,
    )
}

pub(crate) fn spawn_supervised_listener_with_health_interval(
    ch: Arc<dyn Channel>,
    tx: tokio::sync::mpsc::Sender<traits::ChannelMessage>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    health_interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let health_interval = if health_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        health_interval
    };

    tokio::spawn(async move {
        // Single-runner guard: one OS process per channel. Hold an advisory
        // flock for the listener's lifetime. If another live process already
        // holds it (e.g. a daemon while a TUI also auto-starts channels), skip
        // this listener — running both causes duplicate/contradictory replies
        // (WhatsApp) or `409 Conflict` poll flapping (Telegram). Lock releases
        // on drop / process exit, so a crashed holder never blocks restart.
        let _channel_lock = match acquire_channel_lock(ch.name()) {
            ChannelLock::Acquired(lock) => Some(lock),
            ChannelLock::Unavailable => {
                tracing::debug!(
                    "channel {}: lock unavailable; running without single-runner guard",
                    ch.name()
                );
                None
            }
            ChannelLock::HeldByOther => {
                tracing::warn!(
                    "channel {} already running in another process; skipping this listener",
                    ch.name()
                );
                return;
            }
        };

        let component = format!("channel:{}", ch.name());
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        'supervise: loop {
            crate::health::mark_component_ok(&component);
            let mut health = tokio::time::interval(health_interval);
            health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let result = {
                // Pass the shared shutdown token so a well-behaved channel
                // (e.g. Telegram) aborts its long-poll cleanly. The
                // `shutdown.cancelled()` select arm is a backstop for
                // channels that ignore the token: breaking the loop drops
                // the pinned listen future, cancelling its in-flight work.
                let listen_future = ch.listen(tx.clone(), shutdown.clone());
                tokio::pin!(listen_future);

                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => break 'supervise,
                        _ = health.tick() => {
                            crate::health::mark_component_ok(&component);
                        }
                        result = &mut listen_future => break result,
                    }
                }
            };

            if tx.is_closed() || shutdown.is_cancelled() {
                break;
            }

            match result {
                Ok(()) => {
                    tracing::warn!("Channel {} exited unexpectedly; restarting", ch.name());
                    crate::health::mark_component_error(&component, "listener exited unexpectedly");
                    // Clean exit — reset backoff since the listener ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    tracing::error!("Channel {} error: {e}; restarting", ch.name());
                    crate::health::mark_component_error(&component, e.to_string());
                }
            }

            crate::health::bump_component_restart(&component);
            // Cancellable backoff: a restart/shutdown request must not wait
            // out a long backoff window before the listener stops.
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            }
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

pub(crate) fn compute_max_in_flight_messages(channel_count: usize) -> usize {
    channel_count
        .saturating_mul(CHANNEL_PARALLELISM_PER_CHANNEL)
        .clamp(
            CHANNEL_MIN_IN_FLIGHT_MESSAGES,
            CHANNEL_MAX_IN_FLIGHT_MESSAGES,
        )
}

pub(crate) fn log_worker_join_result(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!("Channel message worker crashed: {error}");
    }
}

pub(crate) fn spawn_scoped_typing_task(
    channel: Arc<dyn Channel>,
    recipient: String,
    cancellation_token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let stop_signal = cancellation_token;
    let refresh_interval = Duration::from_secs(CHANNEL_TYPING_REFRESH_INTERVAL_SECS);
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                () = stop_signal.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = channel.start_typing(&recipient).await {
                        tracing::debug!("Failed to start typing on {}: {e}", channel.name());
                    }
                }
            }
        }

        if let Err(e) = channel.stop_typing(&recipient).await {
            tracing::debug!("Failed to stop typing on {}: {e}", channel.name());
        }
    });

    handle
}
