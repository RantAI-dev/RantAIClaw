use super::{
    kill_shared, new_shared_process, program_context, spawn_drain, url_in, SharedProcess, Tunnel,
    TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// How long `tailscale serve|funnel` gets to print the URL it publishes.
const URL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The host part of a URL, lowercased and without the root dot, so it can be
/// compared with `Self.DNSName`.
fn url_host(url: &str) -> String {
    url.strip_prefix("https://")
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase()
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
    /// It is the only host whose URL this tunnel may report: the name is
    /// tailscale's own name for this node, so the address that serves this
    /// gateway is the one whose host is that name. Reading it first makes a
    /// machine with no tailnet DNS name fail here, with an error naming the
    /// program, instead of publishing a URL the gateway cannot check. A
    /// missing or empty field, or JSON that does not parse, is an error —
    /// never `localhost`.
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

        // This node's own DNS name, read on every start. It is the only host
        // whose URL this tunnel may report: any other link on the output —
        // the admin enable link tailscale prints when Serve or Funnel is not
        // enabled on the tailnet — is not this tunnel's address.
        let dns_name = self.status_dns_name().await?;
        let expected_host = dns_name.to_ascii_lowercase();

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
        // The first other link tailscale printed, kept for the error message.
        // It is tailscale's own link, never a secret.
        let mut other_url = None;
        let mut closed = false;
        let deadline = tokio::time::Instant::now() + URL_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let line =
                tokio::time::timeout(tokio::time::Duration::from_secs(5), lines.next_line()).await;

            match line {
                Ok(Ok(Some(l))) => {
                    tracing::debug!("{}: {l}", self.program);
                    if let Some(url) = url_in(&l, "https://") {
                        if url_host(&url) == expected_host {
                            public_url = Some(url);
                            break;
                        }
                        // Some other link: the admin enable link tailscale
                        // prints when Serve or Funnel is not enabled yet. It
                        // waits for an admin, so keep reading for this node's
                        // own URL.
                        if other_url.is_none() {
                            other_url = Some(url);
                        }
                    }
                }
                // The command closed stdout or exited before printing a URL.
                Ok(Ok(None)) => {
                    closed = true;
                    break;
                }
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
                let base = if closed {
                    format!(
                        "{} ({subcommand_port}) exited before printing its public URL",
                        self.program
                    )
                } else {
                    format!(
                        "{} ({subcommand_port}) did not print its public URL within {} s",
                        self.program,
                        URL_TIMEOUT.as_secs()
                    )
                };
                let message = match other_url {
                    Some(other) => format!(
                        "{base} — it printed {other} instead, which is tailscale's own link: \
                         Serve or Funnel may need enabling in the Tailscale admin console"
                    ),
                    None => format!("{base} — is Tailscale running and signed in?"),
                };
                bail!("{message}");
            }
        };

        // Confirmation only: a warning, never a correction, because the
        // reported URL is the provider's own answer. The configured hostname
        // never selects or replaces it.
        if let Some(configured) = self.hostname.as_deref() {
            let configured = configured.trim_end_matches('.');
            if !configured.eq_ignore_ascii_case(&dns_name) {
                tracing::warn!(
                    configured = %configured,
                    dns_name = %dns_name,
                    reported = %public_url,
                    "the configured tailscale hostname differs from the DNS name of this node"
                );
            }
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
        // Nothing else to undo. `tailscale serve|funnel` runs in the
        // foreground, and tailscale drops that config when the process ends.
        // A reset writes an empty ServeConfig and removes every serve and
        // funnel handler on the node, including the ones the operator set up
        // for other services, so it is not run here.
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

    /// Captures everything logged while the returned guard is alive.
    #[derive(Clone, Default)]
    struct LogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogBuffer {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer lock").clone()).expect("utf-8 log")
        }
    }

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
        // Never the real `tailscale`: this machine's tailscale is a live
        // daemon, and a stop must never reach outside the gateway.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake =
            crate::tunnel::write_fake_program(dir.path(), "tailscale", "#!/bin/sh\nexit 0\n");
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));
        let result = tunnel.stop().await;
        assert!(result.is_ok());
    }

    /// A missing binary must be diagnosable: the serve spawn error names the
    /// program and the subcommand. The fake answers `status --json` and then
    /// removes itself, so the `serve` spawn that follows finds no program to
    /// run.
    #[tokio::test]
    async fn missing_program_serve_error_names_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            "#!/bin/sh\nif [ \"$1\" = \"status\" ]; then echo '{\"Self\":{\"DNSName\":\"node.tailnet.ts.net.\"}}'; rm -f \"$0\"; exit 0; fi\nexit 0\n",
        );
        let tunnel = TailscaleTunnel::new(false, Some("rantaiclaw_node".into()))
            .with_program(fake.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a program that does not exist cannot serve");

        let message = format!("{err:#}");
        assert!(
            message.contains(fake.to_str().expect("utf-8 temp path")),
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

    /// stop() ends the process the gateway started and nothing else. The
    /// serve and funnel config of the node belongs to the operator: a reset
    /// writes an empty ServeConfig and removes every handler on the node,
    /// including the ones the operator set up for other services. The fake
    /// logs every invocation, so a reset would show up in the log.
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_runs_no_command_beyond_the_serve_it_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("argv.log");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> {}\n\
                 if [ \"$1\" = \"status\" ]; then echo '{{\"Self\":{{\"DNSName\":\"node.tailnet.ts.net.\"}}}}'; exit 0; fi\n\
                 # a reset invocation exits at once, so a reintroduced reset fails the test\n\
                 if [ \"$2\" = \"reset\" ]; then exit 0; fi\n\
                 echo 'Available within your tailnet:'\n\
                 echo 'https://node.tailnet.ts.net'\n\
                 while :; do sleep 0.2; done\n",
                log.display()
            ),
        );
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));

        tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("the fake serves");
        tunnel.stop().await.expect("stop is ok");

        let recorded = std::fs::read_to_string(&log).expect("argv log");
        let invocations: Vec<&str> = recorded.lines().map(str::trim).collect();
        assert_eq!(
            invocations,
            vec!["status --json", "serve 8080"],
            "stop must run no command of its own: {recorded}"
        );
    }

    /// When Serve or Funnel is not enabled on the tailnet, tailscale prints
    /// its admin enable link and waits for an admin. That link is not this
    /// tunnel's address: the URL is the line whose host is this node's DNS
    /// name.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_admin_enable_link_is_never_the_public_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            "#!/bin/sh\n\
             if [ \"$1\" = \"status\" ]; then echo '{\"Self\":{\"DNSName\":\"node.tailnet.ts.net.\"}}'; exit 0; fi\n\
             # a reset invocation exits at once, so a reintroduced reset fails the test\n\
             if [ \"$2\" = \"reset\" ]; then exit 0; fi\n\
             echo 'https://login.tailscale.com/f/serve?node=abc'\n\
             echo 'https://node.tailnet.ts.net'\n\
             while :; do sleep 0.2; done\n",
        );
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));

        let url = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("the fake publishes a URL");

        assert_eq!(
            url.as_deref(),
            Some("https://node.tailnet.ts.net"),
            "the admin enable link is not this tunnel's address"
        );
        assert_eq!(
            tunnel.public_url().as_deref(),
            Some("https://node.tailnet.ts.net")
        );

        tunnel.stop().await.expect("stop is ok");
    }

    /// The admin enable link with no URL of this node's own: the error names
    /// the program and says that Serve or Funnel may need enabling. It comes
    /// from the closed output, not from waiting out the 30 s deadline.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_admin_enable_link_without_a_node_url_names_the_program_and_the_fix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            "#!/bin/sh\n\
             if [ \"$1\" = \"status\" ]; then echo '{\"Self\":{\"DNSName\":\"node.tailnet.ts.net.\"}}'; exit 0; fi\n\
             echo 'https://login.tailscale.com/f/serve?node=abc'\n\
             exit 1\n",
        );
        let tunnel =
            TailscaleTunnel::new(false, None).with_program(fake.to_str().expect("utf-8 temp path"));

        let started = tokio::time::Instant::now();
        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("no URL of this node was printed");
        let elapsed = started.elapsed();

        let message = format!("{err:#}");
        assert!(
            message.contains(fake.to_str().expect("utf-8 temp path")),
            "the error must name the program it ran: {message}"
        );
        assert!(
            message.contains("Serve"),
            "the error must say Serve may need enabling: {message}"
        );
        assert!(
            message.contains("Funnel"),
            "the error must say Funnel may need enabling: {message}"
        );
        assert!(
            message.contains("admin console"),
            "the error must say where to enable it: {message}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the failure must come from the closed output, not from the 30 s deadline: {elapsed:?}"
        );
    }

    /// A configured hostname confirms the URL, it never selects or replaces
    /// it: the URL stays the one whose host is this node's DNS name, and the
    /// difference is logged. The DNS name is read on every start, even with a
    /// hostname configured.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_configured_hostname_only_confirms_the_dns_name_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("argv.log");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "tailscale",
            &format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> {}\n\
                 if [ \"$1\" = \"status\" ]; then echo '{{\"Self\":{{\"DNSName\":\"node.tailnet.ts.net.\"}}}}'; exit 0; fi\n\
                 # a reset invocation exits at once, so a reintroduced reset fails the test\n\
                 if [ \"$2\" = \"reset\" ]; then exit 0; fi\n\
                 echo 'Available within your tailnet:'\n\
                 echo 'https://node.tailnet.ts.net'\n\
                 while :; do sleep 0.2; done\n",
                log.display()
            ),
        );
        let tunnel = TailscaleTunnel::new(false, Some("other.tailnet.ts.net".into()))
            .with_program(fake.to_str().expect("utf-8 temp path"));

        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let url = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect("the fake publishes a URL");

        drop(guard);
        let captured = buffer.contents();

        assert_eq!(
            url.as_deref(),
            Some("https://node.tailnet.ts.net"),
            "the configured hostname never replaces the URL of this node"
        );
        let recorded = std::fs::read_to_string(&log).expect("argv log");
        assert!(
            recorded.lines().any(|line| line.trim() == "status --json"),
            "the DNS name is read on every start: {recorded}"
        );
        assert!(
            captured.to_lowercase().contains("warn"),
            "the difference must be logged as a warning: {captured}"
        );
        assert!(
            captured.contains("other.tailnet.ts.net"),
            "the warning must name the configured hostname: {captured}"
        );
        assert!(
            captured.contains("node.tailnet.ts.net"),
            "the warning must name the DNS name of this node: {captured}"
        );

        tunnel.stop().await.expect("stop is ok");
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
             # a reset invocation exits at once, so a reintroduced reset fails the test\n\
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
