use super::{
    kill_shared, new_shared_process, program_context, SharedProcess, Tunnel, TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Tailscale Tunnel — uses `tailscale serve` (tailnet-only) or
/// `tailscale funnel` (public internet).
///
/// Requires Tailscale installed and authenticated (`tailscale up`).
pub struct TailscaleTunnel {
    funnel: bool,
    hostname: Option<String>,
    /// Executable name or absolute path. A field so a test can point at a fake
    /// binary instead of whatever `tailscale` happens to be on `PATH`.
    program: String,
    proc: SharedProcess,
}

impl TailscaleTunnel {
    pub fn new(funnel: bool, hostname: Option<String>) -> Self {
        Self {
            funnel,
            hostname,
            program: "tailscale".into(),
            proc: new_shared_process(),
        }
    }

    /// Point the provider at a specific program path instead of looking up
    /// `tailscale` on `PATH`. Test-only, for fake scripts in a tempdir.
    pub(crate) fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }
}

#[async_trait::async_trait]
impl Tunnel for TailscaleTunnel {
    fn name(&self) -> &str {
        "tailscale"
    }

    async fn start(&self, _local_host: &str, local_port: u16) -> Result<String> {
        let subcommand = if self.funnel { "funnel" } else { "serve" };

        // Get the tailscale hostname for URL construction
        let hostname = if let Some(ref h) = self.hostname {
            h.clone()
        } else {
            // Query tailscale for the current hostname
            let output = Command::new(&self.program)
                .args(["status", "--json"])
                .output()
                .await
                .with_context(|| program_context("run", &self.program, "status --json"))?;

            if !output.status.success() {
                bail!(
                    "{} (status --json) failed: {}",
                    self.program,
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let status: serde_json::Value =
                serde_json::from_slice(&output.stdout).unwrap_or_default();
            status["Self"]["DNSName"]
                .as_str()
                .unwrap_or("localhost")
                .trim_end_matches('.')
                .to_string()
        };

        // tailscale serve|funnel <port>
        let subcommand_port = format!("{subcommand} {local_port}");
        let child = Command::new(&self.program)
            .args([subcommand, &local_port.to_string()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| program_context("start", &self.program, &subcommand_port))?;

        let public_url = format!("https://{hostname}:{local_port}");

        let mut guard = self.proc.lock().await;
        *guard = Some(TunnelProcess {
            child,
            public_url: public_url.clone(),
        });

        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        // Also reset the tailscale serve/funnel published config. A failed
        // reset must not keep the process alive — the kill below still runs —
        // but it is worth a warning that names the program it ran.
        let subcommand = if self.funnel { "funnel" } else { "serve" };
        match Command::new(&self.program)
            .args([subcommand, "reset"])
            .output()
            .await
        {
            Ok(output) if !output.status.success() => tracing::warn!(
                program = %self.program,
                subcommand,
                status = %output.status,
                "tailscale reset failed; stopping the tunnel process anyway"
            ),
            Err(error) => tracing::warn!(
                program = %self.program,
                subcommand,
                error = %error,
                "tailscale reset failed; stopping the tunnel process anyway"
            ),
            Ok(_) => {}
        }

        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        let guard = self.proc.lock().await;
        guard.as_ref().is_some_and(|tp| tp.child.id().is_some())
    }

    fn public_url(&self) -> Option<String> {
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
    fn constructor_stores_hostname_and_mode() {
        let tunnel = TailscaleTunnel::new(true, Some("myhost.tailnet.ts.net".into()));
        assert!(tunnel.funnel);
        assert_eq!(tunnel.hostname.as_deref(), Some("myhost.tailnet.ts.net"));
    }

    #[test]
    fn public_url_is_none_before_start() {
        let tunnel = TailscaleTunnel::new(false, None);
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn health_check_is_false_before_start() {
        let tunnel = TailscaleTunnel::new(false, None);
        assert!(!tunnel.health_check().await);
    }

    #[tokio::test]
    async fn stop_without_started_process_is_ok() {
        // Never the real `tailscale`: stop() runs `serve reset`, and this
        // machine's tailscale is a live daemon.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake =
            crate::tunnel::write_fake_program(dir.path(), "tailscale", "#!/bin/sh\nexit 0\n");
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));
        let result = tunnel.stop().await;
        assert!(result.is_ok());
    }

    /// A missing binary must be diagnosable: the serve spawn error names the
    /// program and the subcommand.
    #[tokio::test]
    async fn missing_program_serve_error_names_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("tailscale");
        // A hostname skips `status --json`, so this is the serve spawn.
        let tunnel = TailscaleTunnel::new(false, Some("rantaiclaw_node".into()))
            .with_program(missing.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a program that does not exist cannot serve");

        let message = format!("{err:#}");
        assert!(
            message.contains(missing.to_str().expect("utf-8 temp path")),
            "the error must name the program it tried to run: {message}"
        );
        assert!(
            message.contains("serve"),
            "the error must say which subcommand failed: {message}"
        );
    }

    /// Without a configured hostname, start() runs `status --json` first; that
    /// spawn error names the program too.
    #[tokio::test]
    async fn missing_program_status_error_names_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("tailscale");
        let tunnel = TailscaleTunnel::new(false, None)
            .with_program(missing.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a program that does not exist cannot report status");

        let message = format!("{err:#}");
        assert!(
            message.contains(missing.to_str().expect("utf-8 temp path")),
            "the error must name the program it tried to run: {message}"
        );
        assert!(
            message.contains("status"),
            "the error must say which subcommand failed: {message}"
        );
    }

    /// stop() must reset the published serve config through the configured
    /// program, before it stops the child — never through a `PATH` lookup.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_resets_serve_through_the_configured_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("argv.log");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n[ \"$2\" = \"reset\" ] && exit 0\nexec sleep 30\n",
                log.display()
            ),
        );
        let tunnel = TailscaleTunnel::new(false, Some("rantaiclaw_node".into()))
            .with_program(fake.to_str().expect("utf-8 temp path"));

        tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("the fake starts");
        tunnel.stop().await.expect("stop is ok");

        let recorded = std::fs::read_to_string(&log).expect("argv log");
        assert!(
            recorded.lines().any(|line| line.trim() == "serve reset"),
            "the reset must run through the configured program: {recorded}"
        );
    }

    /// Funnel mode resets funnel, not serve.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_resets_funnel_through_the_configured_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("argv.log");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n[ \"$2\" = \"reset\" ] && exit 0\nexec sleep 30\n",
                log.display()
            ),
        );
        let tunnel = TailscaleTunnel::new(true, Some("rantaiclaw_node".into()))
            .with_program(fake.to_str().expect("utf-8 temp path"));

        tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("the fake starts");
        tunnel.stop().await.expect("stop is ok");

        let recorded = std::fs::read_to_string(&log).expect("argv log");
        assert!(
            recorded.lines().any(|line| line.trim() == "funnel reset"),
            "funnel mode must reset funnel: {recorded}"
        );
    }
}
