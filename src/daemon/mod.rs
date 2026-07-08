pub mod handoff;

use crate::config::Config;
use anyhow::Result;
use chrono::Utc;
use std::future::Future;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

const STATUS_FLUSH_SECONDS: u64 = 5;

/// How long to let the gateway finish in-flight HTTP requests after a shutdown
/// signal before it is force-aborted. Well under systemd's `TimeoutStopSec=30`
/// so the whole stop (drain + `stop_all`) stays inside the unit's window.
const GATEWAY_DRAIN_TIMEOUT: Duration = Duration::from_secs(8);

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
    // the rest; it is the only component with in-flight request state to save.
    let mut gateway_handle = {
        let gateway_cfg = config.clone();
        let gateway_host = host.clone();
        let gateway_shutdown = shutdown.clone();
        spawn_component_supervisor(
            "gateway",
            initial_backoff,
            max_backoff,
            shutdown.clone(),
            move || {
                let cfg = gateway_cfg.clone();
                let host = gateway_host.clone();
                let sd = gateway_shutdown.clone();
                async move { crate::gateway::run_gateway(&host, port, cfg, sd).await }
            },
        )
    };

    {
        if has_supervised_channels(&config) {
            let channels_cfg = config.clone();
            handles.push(spawn_component_supervisor(
                "channels",
                initial_backoff,
                max_backoff,
                shutdown.clone(),
                move || {
                    let cfg = channels_cfg.clone();
                    async move { crate::channels::start_channels(cfg).await }
                },
            ));
        } else {
            crate::health::mark_component_ok("channels");
            tracing::info!("No real-time channels configured; channel supervisor disabled");
        }
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

    if config.cron.enabled {
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
        tracing::info!("Cron disabled; scheduler supervisor not started");
    }

    println!("🧠 RantaiClaw daemon started");
    println!("   Gateway:  http://{host}:{port}");
    println!("   Components: gateway, channels, heartbeat, scheduler");
    println!("   Ctrl+C to stop");

    shutdown_signal().await;
    println!("⏻ shutting down — draining in-flight requests, then cleaning up…");
    crate::health::mark_component_error("daemon", "shutdown requested");

    // Signal graceful shutdown: the gateway stops accepting new connections and
    // finishes in-flight requests; supervisors won't restart on the resulting
    // clean exit.
    shutdown.cancel();

    // Give the gateway a bounded window to drain. On timeout, force it.
    if tokio::time::timeout(GATEWAY_DRAIN_TIMEOUT, &mut gateway_handle)
        .await
        .is_err()
    {
        gateway_handle.abort();
        let _ = gateway_handle.await;
    }

    // The remaining components (channels/heartbeat/scheduler) have no in-flight
    // request state to save, so abort them directly.
    for handle in &handles {
        handle.abort();
    }
    for handle in handles {
        let _ = handle.await;
    }

    // Stop auto-managed services after the supervised components have been aborted,
    // so in-flight tool calls don't get a torn-down container mid-request.
    if !services.is_empty() {
        crate::services::stop_all(&services).await;
    }

    // Clear sentinel — best-effort; a stale sentinel from a crash will be
    // ignored by handoff anyway since the unit will not be active.
    if let Err(e) = crate::profile::sentinel::clear_sentinel(&active_profile) {
        tracing::warn!("Failed to clear daemon sentinel: {e}");
    }

    Ok(())
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
async fn shutdown_signal() {
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
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("daemon_state.json")
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
            let _ = tokio::fs::write(&path, data).await;
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
            tokio::time::sleep(Duration::from_secs(backoff)).await;
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
            if let Err(e) =
                crate::agent::run(config.clone(), Some(prompt), None, None, temp, vec![]).await
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
    fn state_file_path_uses_config_directory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let path = state_file_path(&config);
        assert_eq!(path, tmp.path().join("daemon_state.json"));
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
}
