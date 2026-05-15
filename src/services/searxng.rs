//! SearXNG meta-search engine, auto-launched as a Docker container.
//!
//! When the user sets `[services.searxng] auto_launch = true`, the daemon
//! pulls and runs `searxng/searxng:latest` on `127.0.0.1:<port>`, then
//! `WebSearchTool` resolves its endpoint to that local URL instead of asking
//! the user to provide one.
//!
//! Container is launched detached with `--rm` so it auto-cleans on stop. We
//! use a deterministic container name (`rantaiclaw-searxng-<port>`) so stop
//! is idempotent and a re-run after a crash reuses cleanly.

use super::Service;
use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

/// Container name prefix — port suffix keeps multiple profiles non-conflicting.
const CONTAINER_PREFIX: &str = "rantaiclaw-searxng";

/// Default upstream SearXNG container port.
const SEARXNG_INTERNAL_PORT: u16 = 8080;

/// How long to wait for the HTTP endpoint to come up after `docker run`.
const READY_TIMEOUT_SECS: u64 = 30;

pub struct SearxngService {
    port: u16,
    image: String,
}

impl SearxngService {
    pub fn new(port: u16, image: String) -> Self {
        Self { port, image }
    }

    fn container_name(&self) -> String {
        format!("{CONTAINER_PREFIX}-{}", self.port)
    }

    /// HTTP endpoint the running container will serve on.
    fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Generate a per-instance secret. SearXNG refuses to start without one;
    /// regenerating on each launch is fine since we don't persist sessions.
    fn secret_key() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("rantaiclaw-searxng-{nanos:x}")
    }

    async fn docker_available() -> Result<()> {
        let out = Command::new("docker")
            .arg("version")
            .output()
            .await
            .map_err(|e| anyhow!("docker not found on PATH: {e}. Install Docker or set [services.searxng] auto_launch = false and provide a manual endpoint."))?;
        if !out.status.success() {
            bail!(
                "`docker version` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// True when a container with our deterministic name is already running.
    async fn already_running(&self) -> bool {
        Command::new("docker")
            .args([
                "ps",
                "--quiet",
                "--filter",
                &format!("name=^{}$", self.container_name()),
            ])
            .output()
            .await
            .is_ok_and(|out| out.status.success() && !out.stdout.trim_ascii().is_empty())
    }

    /// Best-effort stop+remove of any prior container with our name (e.g. left
    /// behind by a hard kill). Idempotent.
    async fn purge_stale(&self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_name()])
            .output()
            .await;
    }

    /// Poll the local endpoint until it answers or the deadline expires.
    async fn wait_ready(&self) -> Result<()> {
        let url = self.local_url();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .context("building HTTP client for SearXNG readiness probe")?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(READY_TIMEOUT_SECS);
        while tokio::time::Instant::now() < deadline {
            if client
                .get(&url)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success() || r.status().is_redirection())
            {
                return Ok(());
            }
            sleep(Duration::from_millis(500)).await;
        }
        bail!(
            "SearXNG container started but endpoint {url} did not respond within {READY_TIMEOUT_SECS}s"
        )
    }
}

#[async_trait::async_trait]
impl Service for SearxngService {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn start(&self) -> Result<String> {
        Self::docker_available().await?;

        if self.already_running().await {
            tracing::info!(
                container = %self.container_name(),
                "SearXNG container already running; reusing"
            );
            return Ok(self.local_url());
        }

        // Drop any stopped-but-not-removed container with the same name.
        self.purge_stale().await;

        let port_map = format!("127.0.0.1:{}:{}", self.port, SEARXNG_INTERNAL_PORT);
        let secret_env = format!("SEARXNG_SECRET={}", Self::secret_key());
        let base_env = format!("SEARXNG_BASE_URL={}/", self.local_url());

        let out = Command::new("docker")
            .args([
                "run",
                "--detach",
                "--rm",
                "--name",
                &self.container_name(),
                "--publish",
                &port_map,
                "--env",
                &secret_env,
                "--env",
                &base_env,
                &self.image,
            ])
            .output()
            .await
            .context("spawning `docker run` for SearXNG")?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let trimmed = stderr.trim();
            let detail = if trimmed.is_empty() {
                "(no stderr)"
            } else {
                trimmed
            };
            bail!("`docker run searxng` failed: {detail}");
        }

        self.wait_ready().await?;
        Ok(self.local_url())
    }

    async fn stop(&self) -> Result<()> {
        // `docker stop` is idempotent enough — it errors quietly if the
        // container doesn't exist. We discard the error to keep shutdown clean.
        let _ = Command::new("docker")
            .args(["stop", &self.container_name()])
            .output()
            .await;
        Ok(())
    }

    async fn health_check(&self) -> bool {
        self.already_running().await
    }

    fn endpoint(&self) -> Option<String> {
        Some(self.local_url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_includes_port() {
        let svc = SearxngService::new(8888, "searxng/searxng:latest".into());
        assert_eq!(svc.container_name(), "rantaiclaw-searxng-8888");
    }

    #[test]
    fn local_url_uses_loopback() {
        let svc = SearxngService::new(9999, "searxng/searxng:latest".into());
        assert_eq!(svc.local_url(), "http://127.0.0.1:9999");
    }

    #[test]
    fn endpoint_predictable_before_start() {
        let svc = SearxngService::new(8888, "searxng/searxng:latest".into());
        assert_eq!(svc.endpoint(), Some("http://127.0.0.1:8888".into()));
    }

    #[test]
    fn name_is_stable() {
        let svc = SearxngService::new(8888, "searxng/searxng:latest".into());
        assert_eq!(svc.name(), "searxng");
    }

    #[test]
    fn secret_key_is_nonempty() {
        let s = SearxngService::secret_key();
        assert!(!s.is_empty());
        assert!(s.starts_with("rantaiclaw-searxng-"));
    }

    #[tokio::test]
    async fn stop_without_running_container_is_ok() {
        let svc = SearxngService::new(60001, "searxng/searxng:latest".into());
        let result = svc.stop().await;
        assert!(result.is_ok());
    }
}
