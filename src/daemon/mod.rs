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

/// How long the gateway and channels together get to finish in-flight work
/// after a shutdown signal before what is left is force-aborted: HTTP requests,
/// replies, long-poll offsets.
///
/// One deadline for both, measured from the start of the drain. It used to be
/// eight seconds each in turn, and the gateway normally exits at once, so its
/// unused eight seconds were lost and channels were cut off eight seconds after
/// SIGTERM. It is the channel runtime's own stop timeout, sixteen seconds, well
/// under systemd's `TimeoutStopSec=30`, so `services::stop_all` still fits after
/// it. The runtime ends its turns before this (`CHANNEL_DRAIN_DEADLINE`, checked
/// at compile time against the same value) so their notices go out first.
const DRAIN_TIMEOUT: Duration = crate::channels::CHANNEL_RUNTIME_STOP_TIMEOUT;

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

/// The profile this daemon serves, and whether that profile is pinned.
///
/// `pinned` is true when the launcher named the profile explicitly
/// (`RANTAICLAW_PROFILE` — the line a service unit carries when the operator
/// chose a profile at install time) and false when the daemon follows the
/// active-profile marker. `profile use` restarts only a daemon that follows
/// the marker, and the sentinel records which kind this daemon is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonProfile {
    /// The profile whose config this daemon loaded.
    pub(crate) name: String,
    /// True when the profile came from `RANTAICLAW_PROFILE`.
    pub(crate) pinned: bool,
}

/// Resolve the profile this process serves, once per process.
///
/// The same resolution the config loader uses (env → active-profile marker →
/// "default") — a daemon must write, read and clear its sentinel under the
/// profile whose config it actually loaded, and the two resolutions used to
/// disagree (the sentinel used to key on the env var alone, so a daemon
/// following a non-default marker wrote its sentinel under `default`).
pub(crate) fn resolve_daemon_profile() -> DaemonProfile {
    let pinned = std::env::var("RANTAICLAW_PROFILE")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    DaemonProfile {
        name: crate::profile::ProfileManager::resolve_active_name(),
        pinned,
    }
}

pub async fn run(config: Config, host: String, port: u16) -> Result<()> {
    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config
        .reliability
        .channel_max_backoff_secs
        .max(initial_backoff);

    // The state file the background writer flushes and the cleanup removes.
    // Derived once here and passed into `drain_and_cleanup`, so teardown never
    // re-derives it from the config.
    let state_file = state_file_path(&config);

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
    //
    // `daemon_profile` is resolved ONCE here and kept for the life of the
    // process: every sentinel write AND the shutdown clear must land under
    // the profile whose config this daemon loaded. Re-resolving at shutdown
    // could clear a sentinel that belongs to a different profile than the
    // one this daemon wrote (the marker may have changed in between).
    let daemon_profile = resolve_daemon_profile();
    if let Err(e) = crate::profile::sentinel::write_sentinel(
        &daemon_profile.name,
        &crate::profile::sentinel::DaemonSentinel {
            pid: std::process::id(),
            unit: std::env::var("RANTAICLAW_UNIT").ok(),
            started_at: Some(Utc::now().to_rfc3339()),
            profile_pinned: daemon_profile.pinned,
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

    // ONE observer for this process. The gateway serves `/metrics` from the
    // registry inside it, so anything that builds its own — channels, cron, the
    // heartbeat worker — reports into a registry nobody scrapes. In daemon mode
    // those three do the actual work, which is why `/metrics` described almost
    // nothing. Passed down as an argument; no global, no static.
    let observer: std::sync::Arc<dyn crate::observability::Observer> =
        std::sync::Arc::from(crate::observability::create_observer(&config.observability));

    // ONE bus handle for this process. The channels supervisor publishes its
    // sender into it on every (re)start and clears it on exit; the gateway
    // enqueues parsed webhook messages through it instead of running a second
    // dispatch implementation (plan 313).
    let channel_bus = std::sync::Arc::new(crate::channels::ChannelBus::default());

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
        observer.clone(),
        std::sync::Arc::clone(&channel_bus),
    );

    // Fires even when no supervisor starts at all, so an operator whose only
    // configured channel cannot run in this build still sees why, instead of a
    // silently quiet daemon.
    crate::channels::warn_configured_channels_that_will_not_start(&config);

    // The cron scheduler needs the live channel registry the daemon builds —
    // delivery on WhatsApp Web only sends through the wa-rs client `listen`
    // already opened, and HTTP senders reuse the listener's connection state.
    // Both supervisor tasks share this handle; each call gets its own clone.
    let channels_registry = std::sync::Arc::new(crate::channels::ChannelsRegistry::new());

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
        let channels_observer = observer.clone();
        let channels_bus = std::sync::Arc::clone(&channel_bus);
        let channels_registry_for_runtime = std::sync::Arc::clone(&channels_registry);
        channels_handle = Some(spawn_component_supervisor(
            "channels",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = channels_cfg.clone();
                let sd = channels_shutdown.clone();
                let obs = channels_observer.clone();
                let bus = std::sync::Arc::clone(&channels_bus);
                let reg = std::sync::Arc::clone(&channels_registry_for_runtime);
                async move {
                    crate::channels::start_channels_with_cancellation(
                        cfg,
                        sd,
                        Some(obs),
                        Some(bus),
                        Some(reg),
                    )
                    .await
                }
            },
        ));
    } else {
        crate::health::mark_component_ok("channels");
        tracing::info!("No real-time channels configured; channel supervisor disabled");
    }

    if config.heartbeat.enabled {
        let heartbeat_cfg = config.clone();
        let heartbeat_observer = observer.clone();
        handles.push(spawn_component_supervisor(
            "heartbeat",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = heartbeat_cfg.clone();
                Box::pin(run_heartbeat_worker(cfg, heartbeat_observer.clone()))
            },
        ));
    }

    if scheduler_enabled(&config) {
        let scheduler_cfg = config.clone();
        let scheduler_observer = observer.clone();
        let scheduler_channels = std::sync::Arc::clone(&channels_registry);
        handles.push(spawn_component_supervisor(
            "scheduler",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = scheduler_cfg.clone();
                let obs = scheduler_observer.clone();
                let ch = scheduler_channels.clone();
                // Box the CALL, not the enclosing block: `scheduler::run`
                // crossed clippy::large_futures (23 KB) once it carried the
                // shared observer, and it is the `.await` of that future that
                // sits on the supervisor's poll stack. Same shape as the
                // heartbeat worker below.
                Box::pin(crate::cron::scheduler::run(cfg, Some(obs), (*ch).clone()))
            },
        ));
    } else {
        // "ok" read as healthy on every surface that reads the state file,
        // while the cron docs promise that a disabled scheduler never runs
        // a job. The component records the config's choice instead.
        crate::health::mark_component_disabled("scheduler");
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
                &daemon_profile,
                &state_file,
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
                &daemon_profile,
                &state_file,
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
        &daemon_profile,
        &state_file,
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
/// The gateway and channels share one bounded drain window (they have in-flight
/// state — HTTP requests, replies, long-poll offsets); the rest are aborted
/// directly. Shared by the fatal-exit, early-shutdown, and normal-stop paths so
/// teardown stays identical.
///
/// `state_file_path` is the `daemon_state.json` this daemon flushes: once the
/// state writer has been aborted AND awaited below, the file is removed — a
/// reader must never describe a daemon that has stopped.
async fn drain_and_cleanup(
    mut gateway_handle: JoinHandle<()>,
    channels_handle: Option<JoinHandle<()>>,
    handles: Vec<JoinHandle<()>>,
    services: &[Box<dyn crate::services::Service>],
    profile: &DaemonProfile,
    state_file_path: &Path,
) {
    // One deadline for both drains, so the time the gateway does not use is
    // left to channels instead of lost.
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;

    // Gateway: bounded drain, then force.
    if tokio::time::timeout_at(deadline, &mut gateway_handle)
        .await
        .is_err()
    {
        gateway_handle.abort();
        let _ = gateway_handle.await;
    }

    // Channels: what is left of the same deadline (cancellation makes them
    // return cleanly) before falling back to abort.
    if let Some(mut channels_handle) = channels_handle {
        if tokio::time::timeout_at(deadline, &mut channels_handle)
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

    // The state file must not outlive this daemon. The writer has been
    // aborted AND awaited above, and its write + rename are one synchronous
    // section, so a flush that was in flight runs to its end before this
    // point and the removal below always comes last. Remove it plus any
    // own-pid temp an interrupted atomic write (`write_state_file_atomic`) may
    // have left; another pid's temp is not this daemon's to touch. A missing
    // file is already the goal; any other failure is logged and left to the
    // reader-side age rule (`read_live_state`) to cover.
    let own_pid_tmp = state_file_path.with_extension(format!("json.tmp.{}", std::process::id()));
    for path in [state_file_path, own_pid_tmp.as_path()] {
        if let Err(e) = tokio::fs::remove_file(path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %path.display(),
                    "failed to remove daemon state file: {e}"
                );
            }
        }
    }

    // Stop auto-managed services after the supervised components are down, so
    // in-flight tool calls don't get a torn-down container mid-request.
    if !services.is_empty() {
        crate::services::stop_all(services).await;
    }

    // Clear the sentinel THIS daemon wrote — under the profile resolved at
    // start, never re-resolved now: the marker may have changed while the
    // daemon ran, and the sentinel that belongs to another profile is not
    // this daemon's to clear. Best-effort; a stale sentinel from a crash is
    // ignored by handoff anyway since the unit will not be active.
    if let Err(e) = crate::profile::sentinel::clear_sentinel(&profile.name) {
        tracing::warn!("Failed to clear daemon sentinel: {e}");
    }
}

/// How a failed gateway run should be treated by its supervisor.
///
/// `pub(crate)` so the gateway tests can classify a `run_gateway` error at
/// this seam and pin the fatal/retry split end to end.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GatewayFailure {
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
pub(crate) fn classify_gateway_failure(e: &anyhow::Error) -> GatewayFailure {
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
    observer: std::sync::Arc<dyn crate::observability::Observer>,
    // The process's channel bus, so a verified webhook is enqueued onto the same
    // dispatch loop the channel supervisor drains rather than running a second
    // turn implementation here (plan 313).
    channel_bus: std::sync::Arc<crate::channels::ChannelBus>,
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
                Some(observer.clone()),
                Some(std::sync::Arc::clone(&channel_bus)),
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

/// Read the daemon state file, but only while it can still be describing a
/// live daemon. The state writer stamps `written_at` on every flush every
/// `STATUS_FLUSH_SECONDS`, so a stamp older than three intervals belongs to a
/// daemon that has since exited or crashed (cleanup removes the file on every
/// exit path that runs, but a crash — SIGKILL, OOM — cannot). Returns the
/// parsed state, or `None` for anything a reader must treat as "daemon not
/// running": absent, unreadable, unparseable, or stale. The age rule lives
/// only here — readers stay free of it.
pub fn read_live_state(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    let state: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let written_at = state.get("written_at")?.as_str()?;
    let written_at = chrono::DateTime::parse_from_rfc3339(written_at).ok()?;
    let max_age = chrono::Duration::seconds((STATUS_FLUSH_SECONDS * 3) as i64);
    let age = chrono::Utc::now().signed_duration_since(written_at);
    (age <= max_age).then_some(state)
}

/// The scheduler component's health, as the daemon's state file reports it.
///
/// One verdict for every surface that reports the scheduler — the TUI's
/// `/doctor` panel and the `doctor` CLI's scheduler check — so the two cannot
/// drift apart. The rules live here; each surface maps the verdict onto its
/// own severity type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerVerdict {
    /// No usable state file: the daemon is not running. Absent, unreadable,
    /// unparseable and stale files all land here — [`read_live_state`] owns
    /// the age rule.
    DaemonNotRunning,
    /// The scheduler component reported `ok`.
    Healthy,
    /// The scheduler component reported `error`.
    Unhealthy,
    /// The daemon is running with the scheduler turned off in config
    /// (`[cron].enabled` and `[scheduler].enabled` gate it together): the
    /// supervisor never started the component, so there is nothing healthy
    /// to report.
    Disabled,
    /// The daemon is running but does not track a scheduler component yet.
    NotTracked,
    /// The scheduler component reported a status string this build does not
    /// know (`starting`, for one).
    Other(String),
}

impl SchedulerVerdict {
    /// One line describing the verdict — the same words on every surface.
    pub fn detail(&self) -> String {
        match self {
            Self::DaemonNotRunning => {
                "daemon not running (start `rantaiclaw daemon` for scheduled jobs)".to_string()
            }
            Self::Healthy => "healthy".to_string(),
            Self::Disabled => "disabled by config — scheduled jobs do not run".to_string(),
            Self::Unhealthy => "unhealthy — see daemon logs".to_string(),
            Self::NotTracked => "not tracked yet".to_string(),
            Self::Other(status) => status.clone(),
        }
    }
}

/// Classify the scheduler's health from the daemon's state file.
///
/// Reads through [`read_live_state`], so a state file the daemon has stopped
/// refreshing reads exactly like an absent one: the scheduler of a daemon that
/// is not running is not healthy.
pub fn scheduler_verdict(state_file: &Path) -> SchedulerVerdict {
    let Some(state) = read_live_state(state_file) else {
        return SchedulerVerdict::DaemonNotRunning;
    };
    let status = state
        .get("components")
        .and_then(|c| c.get("scheduler"))
        .and_then(|s| s.get("status"))
        .and_then(|v| v.as_str());
    match status {
        Some("ok") => SchedulerVerdict::Healthy,
        Some("disabled") => SchedulerVerdict::Disabled,
        Some("error") => SchedulerVerdict::Unhealthy,
        Some(other) => SchedulerVerdict::Other(other.to_string()),
        None => SchedulerVerdict::NotTracked,
    }
}

/// Write the daemon state file atomically: serialize to a per-pid temp file in
/// the same directory, then `rename` it over the target. `rename` within a
/// directory is atomic, so a concurrent reader (`doctor`, the TUI) sees either
/// the old file or the new one in full — never a half-written flush, which used
/// to surface as an intermittent false "daemon state corrupt". Best-effort: a
/// failed temp write leaves the previous good file untouched.
///
/// `std::fs`, not `tokio::fs`, and with NO await between the write and the
/// rename. The file is a few KB, so the blocking call is short; keeping the
/// two steps in one synchronous section means a flush that is in flight when
/// the daemon is aborted runs to its end, and the cleanup removal — which runs
/// after the writer is aborted AND awaited — always happens last. With
/// `tokio::fs` the write and the rename ran on the blocking pool, where
/// `abort()` cancels nothing: a flush in flight could rename the file back
/// over the one the cleanup had just removed.
fn write_state_file_atomic(path: &Path, data: &[u8]) {
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, data).is_ok() {
        let _ = std::fs::rename(&tmp, path);
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
            write_state_file_atomic(&path, &data);
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

async fn run_heartbeat_worker(
    config: Config,
    observer: std::sync::Arc<dyn crate::observability::Observer>,
) -> Result<()> {
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
            // Box the agent future: `crate::agent::run` is a large future and
            // boxing it keeps the enclosing heartbeat loop off the poll-loop
            // stack (clippy::large_futures), mirroring the cron scheduler.
            // Heartbeat always passes `Some(prompt)`, so the REPL branch is
            // never reached; the reader is unused. `tokio::io::BufReader::new
            // (tokio::io::stdin())` is `AsyncBufRead + Unpin + Send`, so the
            // future carrying it can run on the multi-threaded runtime.
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            if let Err(e) = Box::pin(crate::agent::run(
                config.clone(),
                Some(prompt),
                None,
                None,
                temp,
                "scheduler",
                // Heartbeat runs under the daemon, where stdout is the
                // journal; the reply (if any) is logged through the
                // observer, not printed.
                true,
                &mut stdin,
            ))
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
    crate::channels::any_catalog_channel_configured(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The heartbeat runs on the host and its reply goes to the journal and the
    /// observer, never to a chat, so it is a place private to the operator: it
    /// reads all of memory. The worker is the real door. Its first tick is
    /// immediate, so it reaches the local server without waiting for the interval.
    #[tokio::test]
    async fn the_heartbeat_door_reads_all_of_memory() {
        let fixture = crate::agent::door_test_support::DoorFixture::start().await;
        std::fs::write(
            fixture.workspace.join("HEARTBEAT.md"),
            "# Periodic Tasks\n\n- check the lantern\n",
        )
        .unwrap();

        let observer: std::sync::Arc<dyn crate::observability::Observer> =
            std::sync::Arc::new(crate::observability::NoopObserver);
        let worker = tokio::spawn(run_heartbeat_worker(fixture.config.clone(), observer));
        fixture.llm.wait_for_requests(1).await;
        worker.abort();

        fixture.assert_every_note_was_sent();
    }

    /// The heartbeat tick runs `agent::run` once per task. A row enters the
    /// `memories` table only because someone asked — never because a
    /// periodic check fired. A re-added `mem.store(...)` inside the worker's
    /// turn shows up as a `count_after > count_before` here. Sibling to
    /// `agent::tests::headless_door_does_not_write_a_memory_row` (same
    /// `agent::run` path, different caller).
    #[tokio::test]
    async fn the_heartbeat_door_does_not_write_a_memory_row() {
        let fixture = crate::agent::door_test_support::DoorFixture::start().await;
        std::fs::write(
            fixture.workspace.join("HEARTBEAT.md"),
            "# Periodic Tasks\n\n- check the lantern\n",
        )
        .unwrap();

        let mem: std::sync::Arc<dyn crate::memory::Memory> =
            std::sync::Arc::from(crate::memory::SqliteMemory::new(&fixture.workspace).unwrap());
        let count_before = mem.count(crate::memory::SessionScope::Any).await.unwrap();

        let observer: std::sync::Arc<dyn crate::observability::Observer> =
            std::sync::Arc::new(crate::observability::NoopObserver);
        let worker = tokio::spawn(run_heartbeat_worker(fixture.config.clone(), observer));
        // One request means one heartbeat tick reached the model; if a
        // regression that auto-wrote a row fired on the inbound task, the
        // count would have grown by then.
        fixture.llm.wait_for_requests(1).await;
        worker.abort();

        let count_after = mem.count(crate::memory::SessionScope::Any).await.unwrap();
        assert_eq!(
            count_before, count_after,
            "the heartbeat door must not change the memories table \
             (before={count_before}, after={count_after})"
        );
    }

    /// Plan 353: the gateway and channels share one drain deadline instead of
    /// eight seconds each in turn. The gateway normally exits at once, and its
    /// unused eight seconds were lost, so channels were aborted eight seconds
    /// after SIGTERM with replies still on their way.
    #[tokio::test(start_paused = true)]
    async fn channels_get_the_drain_time_the_gateway_did_not_use() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let home = TempDir::new().expect("temp home");
        let _home = crate::test_env::HomeGuard::set(home.path());

        let gateway = tokio::spawn(async {});
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let channels = tokio::spawn({
            let finished = std::sync::Arc::clone(&finished);
            async move {
                tokio::time::sleep(Duration::from_secs(12)).await;
                finished.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        drain_and_cleanup(
            gateway,
            Some(channels),
            Vec::new(),
            &[],
            &DaemonProfile {
                name: "drain-test".to_string(),
                pinned: false,
            },
            &home.path().join("daemon_state.json"),
        )
        .await;

        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "channels that needed 12 s were aborted before they finished"
        );
    }

    /// The shared deadline still ends the drain at 16 s in total, today's
    /// eight plus eight, so the unit's `TimeoutStopSec=30` keeps its room for
    /// `services::stop_all` after it.
    #[tokio::test(start_paused = true)]
    async fn the_drain_ends_sixteen_seconds_after_it_starts_at_the_latest() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let home = TempDir::new().expect("temp home");
        let _home = crate::test_env::HomeGuard::set(home.path());

        let gateway = tokio::spawn(tokio::time::sleep(Duration::from_secs(5)));
        let channels = tokio::spawn(tokio::time::sleep(Duration::from_mins(1)));

        let started = tokio::time::Instant::now();
        drain_and_cleanup(
            gateway,
            Some(channels),
            Vec::new(),
            &[],
            &DaemonProfile {
                name: "drain-test".to_string(),
                pinned: false,
            },
            &home.path().join("daemon_state.json"),
        )
        .await;

        assert_eq!(started.elapsed(), Duration::from_secs(16));
    }

    /// Nothing a reader can see may outlive this daemon. A real state-writer
    /// handle (`handles[0]`, as `run` pushes it) has flushed at least once —
    /// its first interval tick is immediate — before the drain begins; after
    /// the writer is aborted AND awaited, the file and any own-pid temp left
    /// by an interrupted atomic write must be gone (other pids' temp files
    /// are not this daemon's to touch).
    #[tokio::test]
    async fn drain_and_cleanup_removes_the_state_file_after_the_writer_is_done() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = TempDir::new().expect("temp home");
        let _home = crate::test_env::HomeGuard::set(tmp.path());

        let state_path = tmp.path().join("daemon_state.json");
        let config = test_config(&tmp);
        let writer = spawn_state_writer(config);
        let flushed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tokio::fs::try_exists(&state_path).await.unwrap_or(false) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            flushed.is_ok(),
            "the state writer must flush to the state path before the drain"
        );

        let own_pid_tmp = state_path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&own_pid_tmp, br#"{"partial":true}"#).unwrap();
        let other_pid_tmp =
            state_path.with_extension(format!("json.tmp.{}", std::process::id() + 7));
        std::fs::write(&other_pid_tmp, br#"{"other":true}"#).unwrap();

        drain_and_cleanup(
            tokio::spawn(async {}),
            None,
            vec![writer],
            &[],
            &DaemonProfile {
                name: "drain-test".to_string(),
                pinned: false,
            },
            &state_path,
        )
        .await;

        assert!(
            !state_path.exists(),
            "drain_and_cleanup must remove daemon_state.json once the state writer is down"
        );
        assert!(
            !own_pid_tmp.exists(),
            "drain_and_cleanup must remove daemon_state.json.tmp.<own pid>"
        );
        assert!(
            other_pid_tmp.exists(),
            "another pid's temp file is not ours to remove"
        );
    }

    /// The reproduction: the marker names `work`, the env var is unset, so
    /// the daemon's sentinel must be keyed on `work` — the profile whose
    /// config the loader actually resolved. The daemon used to key its
    /// sentinel on the env var alone and fell back to `"default"`, so this
    /// daemon's sentinel landed under the wrong profile and `profile use`
    /// never saw it.
    #[test]
    fn the_daemon_resolves_the_profile_the_marker_names() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = TempDir::new().expect("temp home");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let _profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");

        crate::profile::ProfileManager::ensure("work").unwrap();
        std::fs::write(crate::profile::paths::active_profile_file(), "work\n").unwrap();

        let resolved = resolve_daemon_profile();
        assert_eq!(resolved.name, "work");
        assert!(!resolved.pinned, "a marker-resolved profile is not pinned");
    }

    /// The pinned twin: a launcher that names the profile explicitly pins
    /// the daemon to it.
    #[test]
    fn the_daemon_is_pinned_when_the_env_var_names_a_profile() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = TempDir::new().expect("temp home");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let _profile = crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "work");

        crate::profile::ProfileManager::ensure("work").unwrap();

        let resolved = resolve_daemon_profile();
        assert_eq!(resolved.name, "work");
        assert!(resolved.pinned, "an env-var profile is pinned");
    }

    /// The daemon clears the sentinel IT wrote — under the profile resolved
    /// at start — even though the marker changed while it ran. Re-resolving
    /// at shutdown would clear (or keep) the wrong file: here the sentinel
    /// under the NEW marker's profile belongs to another daemon and must
    /// survive this daemon's teardown.
    #[tokio::test]
    async fn the_daemon_clears_the_sentinel_it_wrote_even_after_the_marker_changes() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = TempDir::new().expect("temp home");
        let _home = crate::test_env::HomeGuard::set(tmp.path());
        let _profile = crate::test_env::EnvGuard::unset("RANTAICLAW_PROFILE");

        crate::profile::ProfileManager::ensure("work").unwrap();
        crate::profile::ProfileManager::ensure("play").unwrap();

        // The daemon started while the marker named `work`.
        let identity = DaemonProfile {
            name: "work".to_string(),
            pinned: false,
        };
        crate::profile::sentinel::write_sentinel(
            &identity.name,
            &crate::profile::sentinel::DaemonSentinel {
                pid: std::process::id(),
                unit: None,
                started_at: None,
                profile_pinned: identity.pinned,
            },
        )
        .unwrap();
        // Mid-run, the operator switches the marker to `play`, and a daemon
        // serving `play` writes its own sentinel there.
        std::fs::write(crate::profile::paths::active_profile_file(), "play\n").unwrap();
        crate::profile::sentinel::write_sentinel(
            "play",
            &crate::profile::sentinel::DaemonSentinel {
                pid: 2_147_483_646,
                unit: None,
                started_at: None,
                profile_pinned: false,
            },
        )
        .unwrap();

        drain_and_cleanup(
            tokio::spawn(async {}),
            None,
            vec![],
            &[],
            &identity,
            &tmp.path().join("daemon_state.json"),
        )
        .await;

        assert!(
            !crate::profile::sentinel::sentinel_path("work")
                .expect("a valid profile name")
                .exists(),
            "this daemon must clear the sentinel it wrote"
        );
        assert!(
            crate::profile::sentinel::sentinel_path("play")
                .expect("a valid profile name")
                .exists(),
            "the sentinel under the NEW marker's profile is not this daemon's to clear"
        );
    }

    /// One registry per process is a wiring fact with no behavioural test that
    /// can see it from outside: each component builds fine on its own, and the
    /// only symptom is that `/metrics` under-reports. Assert the wiring itself —
    /// this module builds an observer exactly once, and every component gets a
    /// clone of it. A component reverted to `create_observer` of its own bumps
    /// the count and fails here.
    #[test]
    fn the_daemon_builds_exactly_one_observer() {
        let src = include_str!("mod.rs");
        // Assembled at runtime so this assertion does not count itself.
        let needle = format!("create_{}(", "observer");
        assert_eq!(
            src.matches(needle.as_str()).count(),
            1,
            "the daemon must build ONE observer and pass it down; a second call \
             means a component reports into a registry `/metrics` never serves"
        );
    }

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    /// The config-disabled branch must record the component as `disabled`,
    /// never `ok`: `mark_component_ok` on this path was the defect — every
    /// surface that reads the state file called a scheduler the config
    /// turned off healthy, while the jobs never ran. No state-file test can
    /// catch a regression here (the write happens inside `run`, which no
    /// unit test drives), so the branch source is scanned the way the
    /// surface guards scan their own module.
    #[test]
    fn the_disabled_scheduler_branch_records_disabled_not_ok() {
        let source = include_str!("mod.rs");
        let start = source
            .find("if scheduler_enabled(&config)")
            .expect("the scheduler gate must stay in run()");
        let region = &source[start..];
        let end = region.find("// Gate the").unwrap_or(region.len());
        let branch = &region[..end];
        assert!(
            branch.contains(r#"mark_component_disabled("scheduler")"#),
            "the config-disabled branch must record the component as disabled:\n{branch}"
        );
        assert!(
            !branch.contains(r#"mark_component_ok("scheduler")"#),
            "mark_component_ok on the disabled path is the defect:\n{branch}"
        );
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

    /// A state file body stamped the way the daemon's state writer stamps it:
    /// `written_at` from some seconds ago.
    fn scheduler_state_body(status: &str, written_age_seconds: i64) -> String {
        serde_json::json!({
            "components": {"scheduler": {"status": status}},
            "written_at": (chrono::Utc::now()
                - chrono::Duration::seconds(written_age_seconds))
                .to_rfc3339(),
        })
        .to_string()
    }

    #[test]
    fn scheduler_verdict_reads_a_healthy_scheduler() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(&path, scheduler_state_body("ok", 0)).unwrap();

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::Healthy);
    }

    #[test]
    fn scheduler_verdict_reads_an_unhealthy_scheduler() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(&path, scheduler_state_body("error", 0)).unwrap();

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::Unhealthy);
    }

    /// The daemon records a config-disabled scheduler as "disabled", and the
    /// verdict must carry that as its own fact — never as "healthy", which
    /// is what `mark_component_ok` on the disabled path used to say while
    /// the cron docs promised the jobs never run.
    #[test]
    fn scheduler_verdict_reads_a_disabled_scheduler_as_disabled() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(&path, scheduler_state_body("disabled", 0)).unwrap();

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::Disabled);
        assert!(
            scheduler_verdict(&path)
                .detail()
                .contains("disabled by config"),
            "the detail must say the config turned it off: {}",
            scheduler_verdict(&path).detail()
        );
        assert_ne!(
            scheduler_verdict(&path),
            SchedulerVerdict::Healthy,
            "a disabled scheduler is not a healthy one"
        );
    }

    /// No state file at all: the daemon never ran, so nothing can be healthy.
    #[test]
    fn scheduler_verdict_reads_an_absent_state_file_as_daemon_not_running() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::DaemonNotRunning);
    }

    /// A file whose `written_at` is older than three flush intervals was
    /// written by a daemon that has since exited or crashed. It must read
    /// EXACTLY like an absent file — never a healthy scheduler.
    #[test]
    fn scheduler_verdict_reads_a_stale_state_file_as_daemon_not_running() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(&path, scheduler_state_body("ok", 60)).unwrap();

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::DaemonNotRunning);
    }

    #[test]
    fn scheduler_verdict_reads_an_unparseable_state_file_as_daemon_not_running() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::DaemonNotRunning);
    }

    /// A daemon that is running but has not published a scheduler component
    /// yet is a different state from "not running" — and neither is healthy.
    #[test]
    fn scheduler_verdict_reads_a_daemon_that_does_not_track_the_scheduler() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "components": {"gateway": {"status": "ok"}},
                "written_at": chrono::Utc::now().to_rfc3339(),
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(scheduler_verdict(&path), SchedulerVerdict::NotTracked);
    }

    /// A status string this build does not know (`starting`, for one) is
    /// reported as it stands rather than folded into "ok".
    #[test]
    fn scheduler_verdict_keeps_an_unknown_status_string() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");
        std::fs::write(&path, scheduler_state_body("starting", 0)).unwrap();

        assert_eq!(
            scheduler_verdict(&path),
            SchedulerVerdict::Other("starting".to_string())
        );
    }

    #[tokio::test]
    async fn state_file_write_is_atomic_and_leaves_no_temp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon_state.json");

        write_state_file_atomic(&path, br#"{"ok":true}"#);

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

    /// The gateway supervisor, not the generic component one: a `[tunnel]
    /// provider` value the factory cannot build follows from the config alone,
    /// so the supervisor must send the error on `fatal_tx` once — `daemon::run`
    /// exits non-zero — and then stop, instead of retrying the same `Config`
    /// forever behind the "daemon started" banner. It is the `fatal_tx`
    /// counterpart of `supervisor_marks_error_and_restart_on_failure`.
    #[tokio::test]
    async fn gateway_supervisor_sends_fatal_config_error_once_and_does_not_retry() {
        let mut config = Config::default();
        config.tunnel.provider = "cloudflared".into();

        let shutdown = CancellationToken::new();
        let (fatal_tx, fatal_rx) = tokio::sync::oneshot::channel();
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let observer: std::sync::Arc<dyn crate::observability::Observer> =
            std::sync::Arc::new(crate::observability::NoopObserver);

        let handle = spawn_gateway_supervisor(
            "127.0.0.1".to_string(),
            0,
            config,
            shutdown.clone(),
            std::sync::Arc::clone(&ready),
            fatal_tx,
            // Large backoff: the ONLY acceptable outcome is the fatal send; a
            // retry loop would wait it out and the first expect below would not.
            60,
            60,
            observer,
            std::sync::Arc::new(crate::channels::ChannelBus::default()),
        );

        let e = tokio::time::timeout(Duration::from_secs(10), fatal_rx)
            .await
            .expect("the supervisor must send the fatal error promptly instead of retrying")
            .expect("the sender must still be alive when the failure is fatal");

        assert_eq!(
            classify_gateway_failure(&e),
            GatewayFailure::Fatal,
            "the error must be classified fatal: {e:#}"
        );
        assert!(
            e.to_string()
                .contains(r#"[tunnel] provider = "cloudflared""#),
            "the message must name the config key: {e:#}"
        );

        // No retry: after the fatal send the supervisor task must be DONE, not
        // parked in a 60s backoff for the next attempt.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the supervisor must stop after a fatal error instead of retrying")
            .expect("the supervisor task must not panic");
        shutdown.cancel();
    }

    #[test]
    fn detects_no_supervised_channels() {
        let config = Config::default();
        assert!(!has_supervised_channels(&config));
    }

    #[test]
    fn a_webhook_only_config_has_no_supervised_channel() {
        let config = crate::channels::tests::config_with_only_channel("webhook");
        assert!(!has_supervised_channels(&config));
    }

    /// One table-driven case per catalog key (except `webhook`, served by the
    /// gateway, not the supervisor), so a channel added to the catalog without
    /// wiring it here fails this test instead of silently never starting.
    ///
    /// `matrix` and `lark` are feature-gated: their expected answer follows
    /// whether this build actually compiles them in, the same rule
    /// `channel_is_configured` applies — a configured table for a feature this
    /// build lacks is not a channel the supervisor can run.
    #[test]
    fn every_catalog_channel_that_can_run_in_this_build_is_supervised() {
        for key in crate::channels::channel_catalog_keys() {
            if crate::channels::NON_CHANNEL_CATALOG_KEYS.contains(&key) {
                continue;
            }
            let config = crate::channels::tests::config_with_only_channel(key);
            let expected = match key {
                "matrix" => cfg!(feature = "channel-matrix"),
                "lark" => cfg!(feature = "channel-lark"),
                _ => true,
            };
            assert_eq!(
                has_supervised_channels(&config),
                expected,
                "{key}: expected has_supervised_channels to answer {expected}"
            );
        }
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
