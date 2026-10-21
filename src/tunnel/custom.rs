use super::{
    kill_shared, new_shared_process, program_context, spawn_drain, url_in, SharedProcess, Tunnel,
    TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// Custom Tunnel — bring your own tunnel binary.
///
/// Provide a `start_command` with `{port}` and `{host}` placeholders.
/// Optionally provide a `url_pattern` substring that the output line carrying
/// the public URL must contain — without one the tunnel reports no URL, and a
/// match found before the deadline is the only URL ever reported. Also
/// optionally provide a `health_url` to poll for liveness.
///
/// Examples:
/// - `bore local {port} --to bore.pub`
/// - `frp -c /etc/frp/frpc.ini`
/// - `ssh -R 80:localhost:{port} serveo.net`
pub struct CustomTunnel {
    start_command: String,
    health_url: Option<String>,
    url_pattern: Option<String>,
    proc: SharedProcess,
}

impl CustomTunnel {
    pub fn new(
        start_command: String,
        health_url: Option<String>,
        url_pattern: Option<String>,
    ) -> Self {
        Self {
            start_command,
            health_url,
            url_pattern,
            proc: new_shared_process(),
        }
    }
}

#[async_trait::async_trait]
impl Tunnel for CustomTunnel {
    fn name(&self) -> &str {
        "custom"
    }

    async fn start(&self, local_host: &str, local_port: u16) -> Result<Option<String>> {
        let cmd = self
            .start_command
            .replace("{port}", &local_port.to_string())
            .replace("{host}", local_host);

        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.is_empty() {
            bail!("[tunnel.custom] start_command is empty — nothing to run");
        }

        // Only the program is named in an error: the rest of `start_command`
        // is the operator's argv, which may carry credentials.
        let program = parts[0];
        let mut child = Command::new(program)
            .args(&parts[1..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| program_context("start", program, ""))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture custom tunnel stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture custom tunnel stderr"))?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();

        // The public URL is whatever the operator's command printed and the
        // pattern matched. Without a pattern there is nothing to match, so no
        // wait and no URL: the local address is never reported as public.
        let mut public_url = None;
        if let Some(ref pattern) = self.url_pattern {
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
            while tokio::time::Instant::now() < deadline {
                let line =
                    tokio::time::timeout(tokio::time::Duration::from_secs(3), lines.next_line())
                        .await;

                match line {
                    Ok(Ok(Some(l))) => {
                        tracing::debug!("custom-tunnel: {l}");
                        // `url_pattern` is the substring the operator chose to
                        // pick the line that carries the public URL (a command
                        // may print a local health URL first). The URL itself
                        // is still read off that line; it is never built here.
                        if let Some(url) = url_in(&l, "https://").or_else(|| url_in(&l, "http://"))
                        {
                            if l.contains(pattern) {
                                public_url = Some(url);
                                break;
                            }
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => bail!("Error reading custom tunnel output: {e}"),
                    Err(_) => {}
                }
            }
        }

        {
            let mut guard = self.proc.lock().await;
            *guard = Some(TunnelProcess {
                child,
                public_url: public_url.clone(),
            });
        }
        // Both streams stay drained for the child's whole life: the URL wait
        // above only read part of stdout, and a dropped reader would stop the
        // child (SIGPIPE) or block it on a full pipe.
        spawn_drain("custom stdout", lines);
        spawn_drain("custom stderr", tokio::io::BufReader::new(stderr).lines());

        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        // If a health URL is configured, try to reach it
        if let Some(ref url) = self.health_url {
            return crate::config::build_runtime_proxy_client("tunnel.custom")
                .get(url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .is_ok();
        }

        // Otherwise check if the process is still alive. `try_wait` reports a
        // child that exited and was polled as gone; `id()` never does.
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

    #[tokio::test]
    async fn start_with_empty_command_returns_error() {
        let tunnel = CustomTunnel::new("   ".into(), None, None);
        let result = tunnel.start("127.0.0.1", 8080).await;

        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("[tunnel.custom] start_command is empty"),
            "the error must name the config key: {message}"
        );
    }

    /// A missing custom binary must be diagnosable: the error names the
    /// program it tried to run.
    #[tokio::test]
    async fn missing_program_error_names_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("rantaiclaw_tunnel");
        let tunnel = CustomTunnel::new(format!("{} {{port}}", missing.display()), None, None);

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a program that does not exist cannot start");

        let message = format!("{err:#}");
        assert!(
            message.contains(missing.to_str().expect("utf-8 temp path")),
            "the error must name the program it tried to run: {message}"
        );
    }

    /// Without a `url_pattern` there is nothing to match, so the tunnel is
    /// started but reports no URL: the local address is never shown as public.
    #[tokio::test]
    async fn start_without_pattern_reports_no_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "rantaiclaw_tunnel",
            "#!/bin/sh\nsleep 30\n",
        );
        let tunnel = CustomTunnel::new(format!("{} {{port}}", fake.display()), None, None);

        let url = tunnel.start("127.0.0.1", 4455).await.unwrap();
        assert_eq!(url, None, "no pattern means no URL to report");
        assert_eq!(tunnel.public_url(), None);

        tunnel.stop().await.unwrap();
    }

    /// With a pattern but no matching URL line, the tunnel reports no URL: it
    /// never falls back to the local address, whatever the local bind was.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_with_pattern_and_no_match_reports_no_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(
            dir.path(),
            "rantaiclaw_tunnel",
            "#!/bin/sh\necho 'rantaiclaw_tunnel listening on a local port'\nexit 0\n",
        );
        let tunnel = CustomTunnel::new(
            format!("{} {{port}}", fake.display()),
            None,
            Some("https://".into()),
        );

        let url = tunnel
            .start("127.0.0.1", 4455)
            .await
            .expect("the fake starts");

        assert_eq!(url, None, "a missing match is no URL, not a local one");
        assert_eq!(tunnel.public_url(), None);

        tunnel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_with_pattern_extracts_url() {
        let tunnel = CustomTunnel::new(
            "echo https://public.example".into(),
            None,
            Some("public.example".into()),
        );

        let url = tunnel.start("localhost", 9999).await.unwrap();

        assert_eq!(url.as_deref(), Some("https://public.example"));
        assert_eq!(
            tunnel.public_url().as_deref(),
            Some("https://public.example")
        );

        tunnel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_replaces_host_and_port_placeholders() {
        let tunnel = CustomTunnel::new(
            "echo http://{host}:{port}".into(),
            None,
            Some("http://".into()),
        );

        let url = tunnel.start("10.1.2.3", 4321).await.unwrap();

        assert_eq!(url.as_deref(), Some("http://10.1.2.3:4321"));
        tunnel.stop().await.unwrap();
    }

    #[tokio::test]
    async fn health_check_with_unreachable_health_url_returns_false() {
        let tunnel = CustomTunnel::new(
            "sleep 1".into(),
            Some("http://127.0.0.1:9/healthz".into()),
            None,
        );

        assert!(!tunnel.health_check().await);
    }
}
