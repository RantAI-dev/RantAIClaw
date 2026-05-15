//! External service supervisor.
//!
//! Auto-launches container/sidecar dependencies (e.g. SearXNG for web search)
//! that the user has explicitly opted into via `[services.<name>] auto_launch = true`.
//! Mirrors the `tunnel/` trait+factory pattern: deny-by-default, opt-in per service,
//! daemon-coupled lifecycle (start on boot, stop on shutdown).

mod searxng;

pub use searxng::SearxngService;

use crate::config::schema::ServicesConfig;
use anyhow::Result;

/// Auto-managed external dependency (Docker container, sidecar, etc.).
///
/// Implementations are expected to be cheap to construct; expensive work
/// (image pull, container launch) happens in `start()`. `stop()` must be
/// idempotent so the daemon can call it on shutdown without checking state.
#[async_trait::async_trait]
pub trait Service: Send + Sync {
    /// Stable service name — used for logging and conflict detection.
    fn name(&self) -> &str;

    /// Bring the dependency up. Returns the resolved endpoint URL on success.
    /// Implementations should detect "already running" and reuse rather than fail.
    async fn start(&self) -> Result<String>;

    /// Tear the dependency down. Must be idempotent.
    async fn stop(&self) -> Result<()>;

    /// Cheap liveness check — the daemon may probe periodically.
    async fn health_check(&self) -> bool;

    /// Endpoint URL, if known. Predictable from config alone for most impls,
    /// so callers can resolve it before `start()` completes (e.g. tool wiring).
    fn endpoint(&self) -> Option<String>;
}

/// Build the list of services the user has opted into.
/// Empty vec when no service has `auto_launch = true`.
pub fn create_services(cfg: &ServicesConfig) -> Vec<Box<dyn Service>> {
    let mut out: Vec<Box<dyn Service>> = Vec::new();

    if let Some(s) = &cfg.searxng {
        if s.auto_launch {
            out.push(Box::new(SearxngService::new(s.port, s.image.clone())));
        }
    }

    out
}

/// Start every service in order. Logs and continues on individual failures —
/// a failed service should not block other services or the daemon itself.
pub async fn start_all(services: &[Box<dyn Service>]) {
    for svc in services {
        match svc.start().await {
            Ok(endpoint) => {
                tracing::info!(
                    service = svc.name(),
                    endpoint = %endpoint,
                    "service started"
                );
            }
            Err(e) => {
                tracing::error!(
                    service = svc.name(),
                    error = %e,
                    "service failed to start; downstream consumers will fall back or error"
                );
            }
        }
    }
}

/// Stop every service. Errors are logged but do not propagate — shutdown is
/// best-effort and must not stall the daemon.
pub async fn stop_all(services: &[Box<dyn Service>]) {
    for svc in services {
        if let Err(e) = svc.stop().await {
            tracing::warn!(
                service = svc.name(),
                error = %e,
                "service failed to stop cleanly"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{SearxngServiceConfig, ServicesConfig};

    #[test]
    fn create_services_empty_when_none_opted_in() {
        let cfg = ServicesConfig::default();
        let services = create_services(&cfg);
        assert!(services.is_empty());
    }

    #[test]
    fn create_services_skips_searxng_when_auto_launch_false() {
        let cfg = ServicesConfig {
            searxng: Some(SearxngServiceConfig {
                auto_launch: false,
                port: 8888,
                image: "searxng/searxng:latest".into(),
            }),
        };
        let services = create_services(&cfg);
        assert!(services.is_empty());
    }

    #[test]
    fn create_services_includes_searxng_when_opted_in() {
        let cfg = ServicesConfig {
            searxng: Some(SearxngServiceConfig {
                auto_launch: true,
                port: 8888,
                image: "searxng/searxng:latest".into(),
            }),
        };
        let services = create_services(&cfg);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name(), "searxng");
    }

    #[tokio::test]
    async fn start_all_and_stop_all_handle_empty() {
        let services: Vec<Box<dyn Service>> = Vec::new();
        start_all(&services).await;
        stop_all(&services).await;
    }
}
