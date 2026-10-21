use super::{
    kill_shared, new_shared_process, program_context, SharedProcess, Tunnel, TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// Cloudflare Tunnel — wraps the `cloudflared` binary.
///
/// Requires `cloudflared` installed and a tunnel token from the
/// Cloudflare Zero Trust dashboard.
pub struct CloudflareTunnel {
    token: String,
    /// Executable name or absolute path. A field so a test can point at a fake
    /// binary instead of whatever `cloudflared` happens to be on `PATH`.
    program: String,
    proc: SharedProcess,
}

impl CloudflareTunnel {
    pub fn new(token: String) -> Self {
        Self {
            token,
            program: "cloudflared".into(),
            proc: new_shared_process(),
        }
    }

    /// Point the provider at a specific program path instead of looking up
    /// `cloudflared` on `PATH`. Test-only: tests pass the absolute path of a
    /// fake script, so a missing fake fails the test instead of running a real
    /// tunnel binary found on `PATH`.
    pub(crate) fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }
}

#[async_trait::async_trait]
impl Tunnel for CloudflareTunnel {
    fn name(&self) -> &str {
        "cloudflare"
    }

    async fn start(&self, _local_host: &str, local_port: u16) -> Result<String> {
        // cloudflared tunnel --no-autoupdate run --token <TOKEN> --url http://localhost:<port>
        let mut child = Command::new(&self.program)
            .args([
                "tunnel",
                "--no-autoupdate",
                "run",
                "--token",
                &self.token,
                "--url",
                &format!("http://localhost:{local_port}"),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| program_context("start", &self.program, "tunnel run"))?;

        // Read stderr to find the public URL (cloudflared prints it there)
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture cloudflared stderr"))?;

        let mut reader = tokio::io::BufReader::new(stderr).lines();
        let mut public_url = String::new();

        // Wait up to 30s for the tunnel URL to appear
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            let line =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), reader.next_line()).await;

            match line {
                Ok(Ok(Some(l))) => {
                    tracing::debug!("cloudflared: {l}");
                    // Look for the URL pattern in cloudflared output
                    if let Some(idx) = l.find("https://") {
                        let url_part = &l[idx..];
                        let end = url_part
                            .find(|c: char| c.is_whitespace())
                            .unwrap_or(url_part.len());
                        public_url = url_part[..end].to_string();
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading cloudflared output: {e}"),
                Err(_) => {} // timeout on this line, keep trying
            }
        }

        if public_url.is_empty() {
            child.kill().await.ok();
            bail!("cloudflared did not produce a public URL within 30s. Is the token valid?");
        }

        let mut guard = self.proc.lock().await;
        *guard = Some(TunnelProcess {
            child,
            public_url: public_url.clone(),
        });

        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        let guard = self.proc.lock().await;
        guard.as_ref().is_some_and(|tp| tp.child.id().is_some())
    }

    fn public_url(&self) -> Option<String> {
        // Can't block on async lock in a sync fn, so we try_lock
        self.proc
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|tp| tp.public_url.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_stores_token() {
        let tunnel = CloudflareTunnel::new("cf-token".into());
        assert_eq!(tunnel.token, "cf-token");
    }

    #[test]
    fn public_url_is_none_before_start() {
        let tunnel = CloudflareTunnel::new("cf-token".into());
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn stop_without_started_process_is_ok() {
        let tunnel = CloudflareTunnel::new("cf-token".into());
        let result = tunnel.stop().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn health_check_is_false_before_start() {
        let tunnel = CloudflareTunnel::new("cf-token".into());
        assert!(!tunnel.health_check().await);
    }

    /// A missing binary must be diagnosable: the error names the program it
    /// tried to run, and never the token that rides in argv.
    #[tokio::test]
    async fn missing_program_error_names_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("cloudflared");
        let tunnel = CloudflareTunnel::new("rantaiclaw_cf_token".into())
            .with_program(missing.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a program that does not exist cannot start");

        let message = format!("{err:#}");
        assert!(
            message.contains(missing.to_str().expect("utf-8 temp path")),
            "the error must name the program it tried to run: {message}"
        );
        assert!(
            !message.contains("rantaiclaw_cf_token"),
            "the error must not echo the token argument: {message}"
        );
    }
}
