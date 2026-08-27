pub mod handoff;

use crate::config::Config;
use anyhow::Result;
use chrono::Utc;
use std::future::Future;
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const STATUS_FLUSH_SECONDS: u64 = 5;

/// How long to let the gateway finish in-flight HTTP requests after a shutdown
/// signal before it is force-aborted. Well under systemd's `TimeoutStopSec=30`
/// so the whole stop (drain + `stop_all`) stays inside the unit's window.
const GATEWAY_DRAIN_TIMEOUT: Duration = Duration::from_secs(8);

/// How long to let channels drain in-flight replies and commit long-poll offsets
/// after a shutdown signal before they are force-aborted. Kept with the gateway
/// drain inside systemd's `TimeoutStopSec=30` window.
const CHANNELS_DRAIN_TIMEOUT: Duration = Duration::from_secs(8);

/// Consecutive `EADDRINUSE` binds the gateway tolerates before the port conflict
/// is treated as fatal. A few retries cover a fast restart where the old process
/// is still releasing the port; beyond that it is a real conflict that must
/// propagate rather than loop forever.
const GATEWAY_ADDR_IN_USE_MAX_RETRIES: u32 = 5;

/// The background scheduler runs only when BOTH the cron feature master switch
/// (`[cron].enabled`) and the scheduler-loop switch (`[scheduler].enabled`) are
/// on. Previously only `[cron].enabled` was honored, leaving `[scheduler].enabled`
/// dead config.
fn scheduler_enabled(config: &Config) -> bool {
    config.cron.enabled && config.scheduler.enabled
}

pub async fn run(config: Config, host: String, port: u16) -> Result<()> {
    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config
        .reliability
        .channel_max_backoff_secs
        .max(initial_backoff);

    crate::health::mark_component_ok("daemon");

    // Auto-managed external services (e.g. SearXNG) — opt-in via
    // [services.<name>] auto_launch = true. Started before gateway/channels so
    // tools constructed at request time see ready endpoints.
    let services = crate::services::create_services(&config.services);
    if !services.is_empty() {
        crate::services::start_all(&services).await;
    }

    // Write per-profile sentinel so `profile use` knows a daemon is bound.
    // Best-effort — failure to write must not block the daemon.
    let active_profile = std::env::var("RANTAICLAW_PROFILE").unwrap_or_else(|_| "default".into());
    if let Err(e) = crate::profile::sentinel::write_sentinel(
        &active_profile,
        &crate::profile::sentinel::DaemonSentinel {
            pid: std::process::id(),
            unit: std::env::var("RANTAICLAW_UNIT").ok(),
            started_at: Some(Utc::now().to_rfc3339()),
        },
    ) {
        tracing::warn!("Failed to write daemon sentinel: {e}");
    }

    if config.heartbeat.enabled {
        let _ =
            crate::heartbeat::engine::HeartbeatEngine::ensure_heartbeat_file(&config.workspace_dir)
                .await;
    }

    // Shared shutdown signal. Cancelled once on stop so the gateway can drain
    // in-flight HTTP requests (via axum `with_graceful_shutdown`) instead of
    // being dropped mid-request, and so supervisors don't restart a component
    // that exited *because* of the shutdown.
    let shutdown = CancellationToken::new();

    let mut handles: Vec<JoinHandle<()>> = vec![spawn_state_writer(config.clone())];

    // The gateway is held separately so we can await its drain before aborting
    // the rest, and so a fatal startup failure (a refused public bind, an
    // unparseable address, a persistent port conflict) exits the process instead
    // of looping behind a false "started" banner. `gateway_ready` fires on the
    // first successful bind; `fatal_rx` carries a non-retryable error.
    let gateway_ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let (fatal_tx, mut fatal_rx) = tokio::sync::oneshot::channel::<anyhow::Error>();
    let gateway_handle = spawn_gateway_supervisor(
        host.clone(),
        port,
        config.clone(),
        shutdown.clone(),
        gateway_ready.clone(),
        fatal_tx,
        initial_backoff,
        max_backoff,
    );

    // Channels are held separately too, so shutdown can DRAIN them instead of a
    // bare `abort()`. They run under `start_channels_with_cancellation` (the same
    // cancellable path the TUI uses): cancelling the token stops each listener,
    // closes the dispatch loop, and returns cleanly — where the old
    // non-cancellable `start_channels` + `abort()` dropped in-flight replies and
    // uncommitted long-poll offsets (duplicate reprocessing on the next start).
    let mut channels_handle: Option<JoinHandle<()>> = None;
    if has_supervised_channels(&config) {
        let channels_cfg = config.clone();
        let channels_shutdown = shutdown.clone();
        channels_handle = Some(spawn_component_supervisor(
            "channels",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = channels_cfg.clone();
                let sd = channels_shutdown.clone();
                async move { crate::channels::start_channels_with_cancellation(cfg, sd).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("channels");
        tracing::info!("No real-time channels configured; channel supervisor disabled");
    }

    if config.heartbeat.enabled {
        let heartbeat_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "heartbeat",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = heartbeat_cfg.clone();
                Box::pin(run_heartbeat_worker(cfg))
            },
        ));
    }

    if scheduler_enabled(&config) {
        let scheduler_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "scheduler",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = scheduler_cfg.clone();
                async move { crate::cron::scheduler::run(cfg).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("scheduler");
        tracing::info!(
            "Scheduler disabled (cron.enabled/scheduler.enabled); supervisor not started"
        );
    }

    // Gate the "started" banner on the gateway's first successful bind. If the
    // gateway fails fatally first (refused/unparseable bind, persistent port
    // conflict), exit non-zero so `systemctl status` shows a FAILED unit — not
    // "active (running)" for a daemon that never served a request. A stop signal
    // before the first bind is a clean early shutdown.
    enum Startup {
        Ready,
        Fatal(anyhow::Error),
        ShutdownEarly,
    }
    let startup = tokio::select! {
        () = gateway_ready.notified() => Startup::Ready,
        result = &mut fatal_rx => Startup::Fatal(
            result.unwrap_or_else(|_| anyhow::anyhow!("gateway supervisor ended before binding")),
        ),
        () = shutdown_signal() => Startup::ShutdownEarly,
    };
    match startup {
        Startup::Fatal(e) => {
            tracing::error!("Gateway failed to start (fatal): {e:#}");
            shutdown.cancel();
            drain_and_cleanup(
                gateway_handle,
                channels_handle,
                handles,
                &services,
                &active_profile,
            )
            .await;
            return Err(e);
        }
        Startup::ShutdownEarly => {
            shutdown.cancel();
            drain_and_cleanup(
                gateway_handle,
                channels_handle,
                handles,
                &services,
                &active_profile,
            )
            .await;
            return Ok(());
        }
        Startup::Ready => {}
    }

    println!("🧠 RantaiClaw daemon started");
    println!("   Gateway:  http://{host}:{port}");
    println!("   Components: gateway, channels, heartbeat, scheduler");
    println!("   Ctrl+C to stop");

    // Main wait: a normal stop signal, or a fatal gateway error that surfaces
    // AFTER the first bind (e.g. `EADDRINUSE`-after-N on a later restart).
    let fatal_after: Option<anyhow::Error> = tokio::select! {
        () = shutdown_signal() => None,
        result = &mut fatal_rx => Some(
            result.unwrap_or_else(|_| anyhow::anyhow!("gateway supervisor ended")),
        ),
    };

    println!("⏻ shutting down — draining in-flight requests, then cleaning up…");
    crate::health::mark_component_error("daemon", "shutdown requested");
    // Signal graceful shutdown: the gateway stops accepting new connections and
    // finishes in-flight requests; channels drain their listeners; supervisors
    // won't restart a component that exited because of the shutdown.
    shutdown.cancel();
    drain_and_cleanup(
        gateway_handle,
        channels_handle,
        handles,
        &services,
        &active_profile,
    )
    .await;

    match fatal_after {
        Some(e) => {
            tracing::error!("Gateway failed (fatal) after start: {e:#}");
            Err(e)
        }
        None => Ok(()),
    }
}

/// Drain and tear down all daemon components after `shutdown` has been cancelled.
/// The gateway and channels get bounded drain windows (they have in-flight state
/// — HTTP requests, long-poll offsets); the rest are aborted directly. Shared by
/// the fatal-exit, early-shutdown, and normal-stop paths so teardown stays
/// identical.
async fn drain_and_cleanup(
    mut gateway_handle: JoinHandle<()>,
    channels_handle: Option<JoinHandle<()>>,
    handles: Vec<JoinHandle<()>>,
    services: &[Box<dyn crate::services::Service>],
    active_profile: &str,
) {
    // Gateway: bounded drain, then force.
    if tokio::time::timeout(GATEWAY_DRAIN_TIMEOUT, &mut gateway_handle)
        .await
        .is_err()
    {
        gateway_handle.abort();
        let _ = gateway_handle.await;
    }

    // Channels: bounded drain window (cancellation makes them return cleanly)
    // before falling back to abort.
    if let Some(mut channels_handle) = channels_handle {
        if tokio::time::timeout(CHANNELS_DRAIN_TIMEOUT, &mut channels_handle)
            .await
            .is_err()
        {
            channels_handle.abort();
            let _ = channels_handle.await;
        }
    }

    // The rest (heartbeat/scheduler/state-writer) have no in-flight state to
    // save, so abort them directly.
    for handle in &handles {
        handle.abort();
    }
    for handle in handles {
        let _ = handle.await;
    }

    // Stop auto-managed services after the supervised components are down, so
    // in-flight tool calls don't get a torn-down container mid-request.
    if !services.is_empty() {
        crate::services::stop_all(services).await;
    }

    // Clear sentinel — best-effort; a stale sentinel from a crash is ignored by
    // handoff anyway since the unit will not be active.
    if let Err(e) = crate::profile::sentinel::clear_sentinel(active_profile) {
        tracing::warn!("Failed to clear daemon sentinel: {e}");
    }
}

/// How a failed gateway run should be treated by its supervisor.
#[derive(Debug, PartialEq, Eq)]
enum GatewayFailure {
    /// Never retry — propagate so the process exits non-zero.
    Fatal,
    /// The port is occupied; retry a bounded number of times (a restart may just
    /// be racing the old process releasing the port) before treating it as fatal.
    AddrInUse,
    /// A recoverable error — retry with backoff, as before.
    Transient,
}

/// Classify a gateway run error. A [`crate::gateway::GatewayStartupFatal`] in the
/// chain is unrecoverable; an `EADDRINUSE` io-error is fatal-after-N; anything
/// else is transient.
fn classify_gateway_failure(e: &anyhow::Error) -> GatewayFailure {
    if e.chain().any(|c| {
        c.downcast_ref::<crate::gateway::GatewayStartupFatal>()
            .is_some()
    }) {
        return GatewayFailure::Fatal;
    }
    if e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
    }) {
        return GatewayFailure::AddrInUse;
    }
    GatewayFailure::Transient
}

/// Supervise the gateway: retry transient failures with backoff (racing
/// shutdown), signal `ready` on the first successful bind, and on a fatal error
/// (or a persistent port conflict) send it on `fatal_tx`, cancel `shutdown`, and
/// stop — so `run` can exit non-zero instead of looping.
#[allow(clippy::too_many_arguments)]
fn spawn_gateway_supervisor(
    host: String,
    port: u16,
    config: Config,
    shutdown: CancellationToken,
    ready: std::sync::Arc<tokio::sync::Notify>,
    fatal_tx: tokio::sync::oneshot::Sender<anyhow::Error>,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);
        let mut fatal_tx = Some(fatal_tx);
        let mut addr_in_use_retries: u32 = 0;

        loop {
            crate::health::mark_component_ok("gateway");
            let outcome = crate::gateway::run_gateway(
                &host,
                port,
                config.clone(),
                shutdown.clone(),
                Some(ready.clone()),
            )
            .await;
            if shutdown.is_cancelled() {
                break;
            }
            match outcome {
                Ok(()) => {
                    crate::health::mark_component_error("gateway", "component exited unexpectedly");
                    tracing::warn!("Daemon component 'gateway' exited unexpectedly");
                    backoff = initial_backoff_secs.max(1);
                    addr_in_use_retries = 0;
                }
                Err(e) => {
                    let fatal = match classify_gateway_failure(&e) {
                        GatewayFailure::Fatal => true,
                        GatewayFailure::AddrInUse => {
                            addr_in_use_retries += 1;
                            addr_in_use_retries >= GATEWAY_ADDR_IN_USE_MAX_RETRIES
                        }
                        GatewayFailure::Transient => false,
                    };
                    if fatal {
                        crate::health::mark_component_error("gateway", format!("fatal: {e}"));
                        tracing::error!("Daemon component 'gateway' failed fatally: {e:#}");
                        if let Some(tx) = fatal_tx.take() {
                            let _ = tx.send(e);
                        }
                        // Stop the other components too; `run` drives the exit.
                        shutdown.cancel();
                        break;
                    }
                    crate::health::mark_component_error("gateway", e.to_string());
                    tracing::error!("Daemon component 'gateway' failed: {e}");
                }
            }

            crate::health::bump_component_restart("gateway");
            // Race the backoff against shutdown so a SIGTERM mid-backoff stops
            // promptly instead of waiting out the sleep.
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                () = shutdown.cancelled() => break,
            }
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

/// Block until the daemon receives a shutdown signal — Ctrl+C (SIGINT) or, on
/// Unix, SIGTERM (what `systemctl stop` / `launchctl stop` and a plain `kill`
/// send). Handling SIGTERM is the point: without this arm the daemon took the
/// default "terminate immediately" disposition on every service stop/restart/
/// reboot, so it never ran the graceful path below (component abort →
/// `services::stop_all` → `clear_sentinel`), leaking auto-managed containers
/// and leaving a stale sentinel.
///
/// Infallible on purpose: if the SIGTERM handler can't be installed we log and
/// fall back to Ctrl+C only, rather than refusing to start the daemon.
/// Wait for SIGTERM/SIGINT (Ctrl-C). Shared so the standalone `gateway` command
/// can drive a graceful-drain token the same way the daemon does.
pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!("SIGTERM handler unavailable ({e}); Ctrl+C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub fn state_file_path(config: &Config) -> PathBuf {
    state_file_path_for(&config.config_path)
}

/// The daemon state file that sits next to `config_path`. Split out so callers
/// that only have the config PATH (e.g. the TUI `/config` panel, via
/// `Config::resolve_active_paths`) can find it without a full `load_or_init`
/// (migration + decrypt + env-override + proxy-env mutation on the render
/// thread just to read a directory).
pub fn state_file_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon_state.json")
}

/// Write the daemon state file atomically: serialize to a per-pid temp file in
/// the same directory, then `rename` it over the target. `rename` within a
/// directory is atomic, so a concurrent reader (`doctor`, the TUI) sees either
/// the old file or the new one in full — never a half-written flush, which used
/// to surface as an intermittent false "daemon state corrupt". Best-effort: a
/// failed temp write leaves the previous good file untouched.
async fn write_state_file_atomic(path: &Path, data: &[u8]) {
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if tokio::fs::write(&tmp, data).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, path).await;
    }
}

fn spawn_state_writer(config: Config) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = state_file_path(&config);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(STATUS_FLUSH_SECONDS));
        loop {
            interval.tick().await;
            let mut json = crate::health::snapshot_json();
            if let Some(obj) = json.as_object_mut() {
                obj.insert(
                    "written_at".into(),
                    serde_json::json!(Utc::now().to_rfc3339()),
                );
            }
            let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
            write_state_file_atomic(&path, &data).await;
        }
    })
}

fn spawn_component_supervisor<F, Fut>(
    name: &'static str,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    shutdown: CancellationToken,
    mut run_component: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        loop {
            crate::health::mark_component_ok(name);
            let outcome = run_component().await;
            // The component exited because we're shutting down (e.g. the gateway
            // finished its graceful drain) — stop, don't restart it.
            if shutdown.is_cancelled() {
                break;
            }
            match outcome {
                Ok(()) => {
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    tracing::warn!("Daemon component '{name}' exited unexpectedly");
                    // Clean exit — reset backoff since the component ran successfully
                    backoff = initial_backoff_secs.max(1);
                }
                Err(e) => {
                    crate::health::mark_component_error(name, e.to_string());
                    tracing::error!("Daemon component '{name}' failed: {e}");
                }
            }

            crate::health::bump_component_restart(name);
            // Race the backoff against shutdown — a SIGTERM arriving mid-backoff
            // must stop the component promptly, not wait out the (up to
            // max_backoff) sleep. Every `service stop`/`restart` hit this whenever
            // a component was in its retry window.
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                () = shutdown.cancelled() => break,
            }
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

async fn run_heartbeat_worker(config: Config) -> Result<()> {
    let observer: std::sync::Arc<dyn crate::observability::Observer> =
        std::sync::Arc::from(crate::observability::create_observer(&config.observability));
    let engine = crate::heartbeat::engine::HeartbeatEngine::new(
        config.heartbeat.clone(),
        config.workspace_dir.clone(),
        observer,
    );

    let interval_mins = config.heartbeat.interval_minutes.max(5);
    let mut interval = tokio::time::interval(Duration::from_secs(u64::from(interval_mins) * 60));

    loop {
        interval.tick().await;

        let tasks = engine.collect_tasks().await?;
        if tasks.is_empty() {
            continue;
        }

        for task in tasks {
            let prompt = format!("[Heartbeat Task] {task}");
            let temp = config.default_temperature;
            if let Err(e) = crate::agent::run(
                config.clone(),
                Some(prompt),
                None,
                None,
                temp,
                vec![],
                "scheduler",
            )
            .await
            {
                crate::health::mark_component_error("heartbeat", e.to_string());
                tracing::warn!("Heartbeat task failed: {e}");
            } else {
                crate::health::mark_component_ok("heartbeat");
            }
        }
    }
}

fn has_supervised_channels(config: &Config) -> bool {
    let crate::config::ChannelsConfig {
        cli: _,     // `cli` is used only when running the CLI manually
        webhook: _, // Managed by the gateway
        telegram,
        discord,
        slack,
        mattermost,
        imessage,
        matrix,
        signal,
        whatsapp,
        email,
        irc,
        lark,
        dingtalk,
        linq,
        nextcloud_talk,
        qq,
        ..
    } = &config.channels_config;

    telegram.is_some()
        || discord.is_some()
        || slack.is_some()
        || mattermost.is_some()
        || imessage.is_some()
        || matrix.is_some()
        || signal.is_some()
        || whatsapp.is_some()
        || email.is_some()
        || irc.is_some()
        || lark.is_some()
        || dingtalk.is_some()
        || linq.is_some()
        || nextcloud_talk.is_some()
        || qq.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    #[test]
    fn scheduler_enabled_requires_both_cron_and_scheduler_flags() {
        let mut c = crate::config::Config::default();
        assert!(scheduler_enabled(&c), "both flags default to true");
        c.scheduler.enabled = false;
        assert!(
            !scheduler_enabled(&c),
            "scheduler.enabled=false disables it"
        );
        c.scheduler.enabled = true;
        c.cron.enabled = false;
        assert!(!scheduler_enabled(&c), "cron.enabled=false disables it");
    }

    #[test]
    fn state_file_path_uses_config_directory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let path = state_file_path(&config);
        assert_eq!(path, tmp.path().join("daemon_state.json"));
    }

    #[tokio::test]
    async fn state_file_write_is_atomic_and_leaves_no_temp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");

        write_state_file_atomic(&path, br#"{"ok":true}"#).await;

        // Target parses as JSON…
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["ok"], serde_json::json!(true));

        // …and no `.tmp` scratch file was left behind in the directory.
        let mut entries = tokio::fs::read_dir(tmp.path()).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            let name = e.file_name();
            assert!(
                !name.to_string_lossy().contains(".tmp"),
                "leftover temp file: {name:?}"
            );
        }
    }

    #[tokio::test]
    async fn supervisor_marks_error_and_restart_on_failure() {
        let handle = spawn_component_supervisor(
            "daemon-test-fail",
            1,
            1,
            CancellationToken::new(),
            || async { anyhow::bail!("boom") },
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-fail"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("boom"));
    }

    #[tokio::test]
    async fn supervisor_marks_unexpected_exit_as_error() {
        let handle = spawn_component_supervisor(
            "daemon-test-exit",
            1,
            1,
            CancellationToken::new(),
            || async { Ok(()) },
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;

        let snapshot = crate::health::snapshot_json();
        let component = &snapshot["components"]["daemon-test-exit"];
        assert_eq!(component["status"], "error");
        assert!(component["restart_count"].as_u64().unwrap_or(0) >= 1);
        assert!(component["last_error"]
            .as_str()
            .unwrap_or("")
            .contains("component exited unexpectedly"));
    }

    #[tokio::test]
    async fn supervisor_stops_promptly_during_backoff() {
        let shutdown = CancellationToken::new();
        let handle = spawn_component_supervisor(
            "daemon-test-backoff",
            // Large backoff so the task is parked in the retry sleep, not spinning.
            60,
            60,
            shutdown.clone(),
            || async { anyhow::bail!("always fails") },
        );

        // Let the component fail once and enter the backoff sleep.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A shutdown arriving mid-backoff must stop the supervisor promptly
        // instead of waiting out the full 60s sleep.
        shutdown.cancel();

        let stopped = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            stopped.is_ok(),
            "supervisor did not stop promptly during backoff"
        );
    }

    #[test]
    fn detects_no_supervised_channels() {
        let config = Config::default();
        assert!(!has_supervised_channels(&config));
    }

    #[test]
    fn detects_supervised_channels_present() {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "token".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_dingtalk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.dingtalk = Some(crate::config::schema::DingTalkConfig {
            client_id: "client_id".into(),
            client_secret: "client_secret".into(),
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_mattermost_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.mattermost = Some(crate::config::schema::MattermostConfig {
            url: "https://mattermost.example.com".into(),
            bot_token: "token".into(),
            channel_id: Some("channel-id".into()),
            allowed_users: vec!["*".into()],
            thread_replies: Some(true),
            mention_only: Some(false),
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_qq_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.qq = Some(crate::config::schema::QQConfig {
            app_id: "app-id".into(),
            app_secret: "app-secret".into(),
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn detects_nextcloud_talk_as_supervised_channel() {
        let mut config = Config::default();
        config.channels_config.nextcloud_talk = Some(crate::config::schema::NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: None,
            allowed_users: vec!["*".into()],
        });
        assert!(has_supervised_channels(&config));
    }

    #[test]
    fn typed_startup_error_is_classified_fatal() {
        // A GatewayStartupFatal anywhere in the chain → the supervisor propagates
        // (exits non-zero) instead of retrying the bind forever.
        let e = anyhow::Error::new(crate::gateway::GatewayStartupFatal(
            "refusing public bind".into(),
        ));
        assert_eq!(classify_gateway_failure(&e), GatewayFailure::Fatal);
        // Still fatal when wrapped with added context.
        let wrapped = e.context("while starting the gateway");
        assert_eq!(classify_gateway_failure(&wrapped), GatewayFailure::Fatal);
    }

    #[test]
    fn addr_in_use_is_classified_addr_in_use() {
        let e = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "address already in use",
        ));
        assert_eq!(classify_gateway_failure(&e), GatewayFailure::AddrInUse);
    }

    #[test]
    fn other_errors_are_transient() {
        let e = anyhow::anyhow!("some transient provider hiccup");
        assert_eq!(classify_gateway_failure(&e), GatewayFailure::Transient);
        // A non-AddrInUse io-error is transient too.
        let io = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert_eq!(classify_gateway_failure(&io), GatewayFailure::Transient);
    }
}
