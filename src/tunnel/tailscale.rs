use super::{
    kill_shared, new_shared_process, program_context, spawn_drain, url_in, SharedProcess, Tunnel,
    TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// How long `tailscale serve|funnel` gets to print the URL it publishes.
const URL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The host part of a URL, for the confirmation below.
fn url_host(url: &str) -> &str {
    url.strip_prefix("https://")
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
}

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

    /// `Self.DNSName` from `status --json`, read before serving.
    ///
    /// It only ever CONFIRMS the URL that `serve`/`funnel` prints: the
    /// reported value is the provider's own output, never this. Reading it
    /// first makes a machine with no tailnet DNS name fail here, with an
    /// error naming the program, instead of publishing a URL the gateway
    /// cannot check. A missing or empty field, or JSON that does not parse, is
    /// an error — never `localhost`.
    async fn status_dns_name(&self) -> Result<String> {
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

        let status: serde_json::Value = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("{} (status --json) did not print JSON", self.program))?;
        let name = status["Self"]["DNSName"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('.');
        if name.is_empty() {
            bail!(
                "{} (status --json) reported no Self.DNSName — is Tailscale up?",
                self.program
            );
        }
        Ok(name.to_string())
    }
}

#[async_trait::async_trait]
impl Tunnel for TailscaleTunnel {
    fn name(&self) -> &str {
        "tailscale"
    }

    async fn start(&self, _local_host: &str, local_port: u16) -> Result<Option<String>> {
        let subcommand = if self.funnel { "funnel" } else { "serve" };

        // The URL the gateway reports is the one serve|funnel prints. A
        // configured hostname, or `Self.DNSName` when there is none, is only
        // used below to confirm it.
        let expected_host = match self.hostname.as_deref() {
            Some(host) => host.trim_end_matches('.').to_string(),
            None => self.status_dns_name().await?,
        };

        // tailscale serve|funnel <port>. It prints the URL on stdout and then
        // stays in the foreground, so it is never waited for.
        let subcommand_port = format!("{subcommand} {local_port}");
        let mut child = Command::new(&self.program)
            .args([subcommand, &local_port.to_string()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| program_context("start", &self.program, &subcommand_port))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture {} stdout", self.program))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture {} stderr", self.program))?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();

        // `tailscale serve|funnel` prints a header ("Available within your
        // tailnet:" / "Available on the internet:") and then one
        // `https://<machine>.<tailnet>.ts.net` line per handler, on stdout,
        // with the port trimmed because it serves on 443. The local port is
        // never appended.
        let mut public_url = None;
        let deadline = tokio::time::Instant::now() + URL_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let line =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), lines.next_line()).await;

            match line {
                Ok(Ok(Some(l))) => {
                    tracing::debug!("{}: {l}", self.program);
                    if let Some(url) = url_in(&l, "https://") {
                        public_url = Some(url);
                        break;
                    }
                }
                // The command closed stdout or exited before printing a URL.
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading {} output: {e}", self.program),
                Err(_) => {} // timeout on this line, keep trying
            }
        }

        let public_url = match public_url {
            Some(url) => url,
            None => {
                child.kill().await.ok();
                child.wait().await.ok();
                // Never a local or constructed address: the gateway reports
                // only what the provider printed.
                bail!(
                    "{} ({subcommand_port}) did not print its public URL within {} s — \
                     is Tailscale running and signed in?",
                    self.program,
                    URL_TIMEOUT.as_secs()
                );
            }
        };

        // Confirmation only: a warning, never a correction, because the
        // printed URL is the provider's own answer.
        let reported_host = url_host(&public_url);
        if !reported_host.eq_ignore_ascii_case(&expected_host) {
            tracing::warn!(
                expected = %expected_host,
                reported = %public_url,
                "tailscale's published URL does not match the configured or reported hostname"
            );
        }

        {
            let mut guard = self.proc.lock().await;
            *guard = Some(TunnelProcess {
                child,
                public_url: Some(public_url.clone()),
            });
        }
        // serve|funnel keeps running in the foreground: both pipes stay
        // drained for the child's whole life.
        spawn_drain("tailscale stdout", lines);
        spawn_drain(
            "tailscale stderr",
            tokio::io::BufReader::new(stderr).lines(),
        );

        Ok(Some(public_url))
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
        // `try_wait` reports a child that exited and was polled as gone; `id()`
        // kept reporting a dead child as alive until it was reaped.
        let mut guard = self.proc.lock().await;
        match guard.as_mut() {
            Some(tp) => matches!(tp.child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn public_url(&self) -> Option<String> {
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
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n[ \"$2\" = \"reset\" ] && exit 0\necho 'Available within your tailnet:'\necho 'https://rantaiclaw_node.tailnet.ts.net'\nwhile :; do sleep 0.2; done\n",
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
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n[ \"$2\" = \"reset\" ] && exit 0\necho 'Available on the internet:'\necho 'https://rantaiclaw_node.tailnet.ts.net'\nwhile :; do sleep 0.2; done\n",
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

    /// The URL is the one the serve command prints, with the port trimmed
    /// (serve publishes on 443, not the local port): never
    /// `https://<name>:<local port>`, never `localhost`.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_reports_the_url_tailscale_prints_without_the_local_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            "#!/bin/sh\n\
             if [ \"$1\" = \"status\" ]; then echo '{\"Self\":{\"DNSName\":\"rantaiclaw_node.tailnet.ts.net.\"}}'; exit 0; fi\n\
             if [ \"$2\" = \"reset\" ]; then exit 0; fi\n\
             echo 'Available within your tailnet:'\n\
             echo 'https://rantaiclaw_node.tailnet.ts.net'\n\
             while :; do sleep 0.2; done\n",
        );
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));

        let url = tunnel
            .start("127.0.0.1", 8443)
            .await
            .expect("the fake publishes a URL");

        assert_eq!(
            url.as_deref(),
            Some("https://rantaiclaw_node.tailnet.ts.net"),
            "the URL is exactly what tailscale printed: no local port, no localhost"
        );
        assert_eq!(
            tunnel.public_url().as_deref(),
            Some("https://rantaiclaw_node.tailnet.ts.net")
        );
        assert!(tunnel.health_check().await);

        tunnel.stop().await.expect("stop is ok");
    }

    /// Without a configured hostname, `status --json` must supply the DNSName
    /// used to confirm the URL. A missing one is an error that names the
    /// program — never a `localhost` fallback.
    #[cfg(unix)]
    #[tokio::test]
    async fn status_without_a_dns_name_is_an_error_that_never_names_localhost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            "#!/bin/sh\n\
             if [ \"$1\" = \"status\" ]; then echo '{\"Self\":{}}'; exit 0; fi\n\
             echo 'Available within your tailnet:'\n\
             echo 'https://rantaiclaw_node.tailnet.ts.net'\n\
             while :; do sleep 0.2; done\n",
        );
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a status with no DNSName cannot be confirmed");

        let message = format!("{err:#}");
        assert!(
            message.contains("DNSName"),
            "the error must say what was missing: {message}"
        );
        assert!(
            !message.contains("localhost"),
            "a missing hostname is an error, never a local address: {message}"
        );
        assert!(
            !message.contains("https://"),
            "the start must not report a URL it could not confirm: {message}"
        );
    }

    /// A serve that exits without printing a URL is an error naming the
    /// program and the subcommand, and it never mentions a local address.
    #[cfg(unix)]
    #[tokio::test]
    async fn serve_without_a_url_names_the_program_and_never_localhost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            "#!/bin/sh\n\
             if [ \"$1\" = \"status\" ]; then echo '{\"Self\":{\"DNSName\":\"rantaiclaw_node.tailnet.ts.net.\"}}'; exit 0; fi\n\
             exit 1\n",
        );
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a serve that prints no URL cannot be reported");

        let message = format!("{err:#}");
        assert!(
            message.contains(fake.to_str().expect("utf-8 temp path")),
            "the error must name the program it ran: {message}"
        );
        assert!(
            message.contains("serve 8080"),
            "the error must name the subcommand: {message}"
        );
        assert!(
            !message.contains("localhost"),
            "the error must never suggest a local URL: {message}"
        );
    }
}
