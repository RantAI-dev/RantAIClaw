use super::{
    kill_shared, new_shared_process, program_context, spawn_drain, url_in, SharedProcess, Tunnel,
    TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// How long cloudflared gets to report a registered tunnel connection.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

    async fn start(&self, _local_host: &str, local_port: u16) -> Result<Option<String>> {
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

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture {} stdout", self.program))?;
        // The console logger writes everything to stderr, including the
        // readiness line and the connection count.
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture {} stderr", self.program))?;
        let mut lines = tokio::io::BufReader::new(stderr).lines();

        // Cloudflare's token mode never prints the public hostname: it is
        // provisioned in the dashboard and cloudflared never learns it. The
        // readiness signal is "Registered tunnel connection" (Info level, one
        // per connection); waiting for an `https://` line would wait for
        // something that never comes. If a URL line does appear first, it is
        // the provider's own and is returned.
        let mut public_url = None;
        let mut ready = false;
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let line =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), lines.next_line()).await;

            match line {
                Ok(Ok(Some(l))) => {
                    tracing::debug!("{}: {l}", self.program);
                    if l.contains("Registered tunnel connection") {
                        ready = true;
                        break;
                    }
                    if let Some(url) = url_in(&l, "https://") {
                        public_url = Some(url);
                        ready = true;
                        break;
                    }
                }
                // The child closed stderr or exited: it never got connected.
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading {} output: {e}", self.program),
                Err(_) => {} // timeout on this line, keep trying
            }
        }

        if !ready {
            child.kill().await.ok();
            child.wait().await.ok();
            bail!(
                "{} (tunnel run) did not report a registered tunnel connection within {} s — \
                 is the token valid?",
                self.program,
                READY_TIMEOUT.as_secs()
            );
        }

        {
            let mut guard = self.proc.lock().await;
            *guard = Some(TunnelProcess {
                child,
                public_url: public_url.clone(),
            });
        }
        // Keep both streams drained for the child's whole life: dropping a
        // reader makes the next log write kill cloudflared with SIGPIPE.
        spawn_drain(
            "cloudflared stdout",
            tokio::io::BufReader::new(stdout).lines(),
        );
        spawn_drain("cloudflared stderr", lines);

        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        // `try_wait` reports a child that exited and was polled as gone; `id()`
        // kept reporting a dead child as alive until it was reaped.
        let mut guard = self.proc.lock().await;
        match guard.as_mut() {
            Some(tp) => matches!(tp.child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn public_url(&self) -> Option<String> {
        // Can't block on async lock in a sync fn, so we try_lock
        self.proc
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|tp| tp.public_url.clone()))
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

    /// A Go tunnel binary exits with SIGPIPE on its next stderr write once the
    /// gateway drops the reader (pkg.go.dev/os/signal). The fake below keeps
    /// writing every 100 ms, so it outlives `start` only if every piped stream
    /// is still drained.
    #[cfg(unix)]
    #[tokio::test]
    async fn keeps_the_child_alive_after_start_returns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "cloudflared",
            &format!(
                "#!/bin/sh\n\
                 echo 'Registered tunnel connection (connIndex=0)' >&2\n\
                 echo 'https://rantaiclaw_tunnel.example.com' >&2\n\
                 echo $$ > {}\n\
                 while :; do echo 'stream alive' >&2; sleep 0.1; done\n",
                pid_file.display()
            ),
        );
        let tunnel = CloudflareTunnel::new("rantaiclaw_cf_token".into())
            .with_program(fake.to_str().expect("utf-8 temp path"));

        let _ = tunnel.start("127.0.0.1", 8080).await;
        let pid = crate::tunnel::wait_for_pid_file(&pid_file).await;

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        assert!(
            crate::tunnel::process_is_alive(pid),
            "the tunnel child must survive the gateway reading its output: \
             a Go binary exits on SIGPIPE once its log reader is dropped (pid {pid})"
        );
        tunnel.stop().await.ok();
    }

    /// Cloudflare's token mode never prints the hostname (it lives in the
    /// dashboard); the readiness line is the whole signal, so the started
    /// tunnel reports no URL rather than waiting for one that never comes.
    #[cfg(unix)]
    #[tokio::test]
    async fn reports_no_url_when_only_the_readiness_line_appears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "cloudflared",
            "#!/bin/sh\necho 'Registered tunnel connection (connIndex=0)' >&2\nsleep 30\n",
        );
        let tunnel = CloudflareTunnel::new("rantaiclaw_cf_token".into())
            .with_program(fake.to_str().expect("utf-8 temp path"));

        let url = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("a registered connection is a started tunnel");

        assert_eq!(url, None, "cloudflared printed no URL to report");
        assert_eq!(tunnel.public_url(), None);
        assert!(tunnel.health_check().await);

        tunnel.stop().await.ok();
    }

    /// A child that exits on its own must not be reported as alive: `id()`
    /// kept saying it was, because the child was never polled.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_is_false_after_the_child_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "cloudflared",
            "#!/bin/sh\necho 'Registered tunnel connection (connIndex=0)' >&2\nexit 0\n",
        );
        let tunnel = CloudflareTunnel::new("rantaiclaw_cf_token".into())
            .with_program(fake.to_str().expect("utf-8 temp path"));

        tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("a registered connection is a started tunnel");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tunnel.health_check().await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "health_check must report an exited child as not alive"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
