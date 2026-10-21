use super::{
    kill_shared, new_shared_process, program_context, spawn_drain, SharedProcess, Tunnel,
    TunnelProcess,
};
use anyhow::{bail, Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// ngrok Tunnel — wraps the `ngrok` binary.
///
/// Requires `ngrok` installed. Optionally set a custom domain
/// (requires ngrok paid plan).
pub struct NgrokTunnel {
    auth_token: String,
    domain: Option<String>,
    /// Executable name or absolute path. A field so a test can point at a fake
    /// binary instead of whatever `ngrok` happens to be on `PATH`.
    program: String,
    proc: SharedProcess,
}

impl NgrokTunnel {
    pub fn new(auth_token: String, domain: Option<String>) -> Self {
        Self {
            auth_token,
            domain,
            program: "ngrok".into(),
            proc: new_shared_process(),
        }
    }

    /// Point the provider at a specific program path instead of looking up
    /// `ngrok` on `PATH`. Test-only, for fake scripts in a tempdir.
    pub(crate) fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }
}

#[async_trait::async_trait]
impl Tunnel for NgrokTunnel {
    fn name(&self) -> &str {
        "ngrok"
    }

    async fn start(&self, _local_host: &str, local_port: u16) -> Result<Option<String>> {
        // Set auth token
        let output = Command::new(&self.program)
            .args(["config", "add-authtoken", &self.auth_token])
            .output()
            .await
            .with_context(|| program_context("run", &self.program, "config add-authtoken"))?;

        // The status used to be ignored: a rejected token meant the next
        // command ran unauthenticated and failed 15 s later with a misleading
        // "did not produce a public URL". Fail here instead, naming only the
        // program and the subcommand — the token argument never goes in.
        if !output.status.success() {
            bail!(
                "{} (config add-authtoken) failed: {}",
                self.program,
                output.status
            );
        }

        // Build command: ngrok http <port> [--domain <domain>]
        let mut args = vec!["http".to_string(), local_port.to_string()];
        if let Some(ref domain) = self.domain {
            args.push("--domain".into());
            args.push(domain.clone());
        }
        // Output log to stdout for URL extraction
        args.push("--log".into());
        args.push("stdout".into());
        args.push("--log-format".into());
        args.push("logfmt".into());

        let mut child = Command::new(&self.program)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| program_context("start", &self.program, "http"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture {} stdout", self.program))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture {} stderr", self.program))?;

        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut public_url = String::new();

        // Wait up to 15s for the tunnel URL. The `url=` logfmt field is the
        // only random-URL mechanism the agent offers; it is UNCONFIRMED from
        // ngrok's own sources (the v3 agent is closed source), so a timeout
        // stays an error that names the program rather than a silent `None`.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            let line =
                tokio::time::timeout(tokio::time::Duration::from_secs(3), reader.next_line()).await;

            match line {
                Ok(Ok(Some(l))) => {
                    tracing::debug!("ngrok: {l}");
                    // ngrok logfmt: url=https://xxxx.ngrok-free.app
                    if let Some(idx) = l.find("url=https://") {
                        let url_start = idx + 4; // skip "url="
                        let url_part = &l[url_start..];
                        let end = url_part
                            .find(|c: char| c.is_whitespace())
                            .unwrap_or(url_part.len());
                        public_url = url_part[..end].to_string();
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading ngrok output: {e}"),
                Err(_) => {}
            }
        }

        if public_url.is_empty() {
            child.kill().await.ok();
            child.wait().await.ok();
            bail!(
                "{} (http) did not produce a public URL within 15s. Is the auth token valid?",
                self.program
            );
        }

        {
            let mut guard = self.proc.lock().await;
            *guard = Some(TunnelProcess {
                child,
                public_url: Some(public_url.clone()),
            });
        }
        // Keep both streams drained for the child's whole life: ngrok is a Go
        // binary and exits with SIGPIPE on the next log write once its reader
        // is gone.
        spawn_drain("ngrok stdout", reader);
        spawn_drain("ngrok stderr", tokio::io::BufReader::new(stderr).lines());

        Ok(Some(public_url))
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
    fn constructor_stores_domain() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), Some("my.ngrok.app".into()));
        assert_eq!(tunnel.domain.as_deref(), Some("my.ngrok.app"));
    }

    #[test]
    fn public_url_is_none_before_start() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn stop_without_started_process_is_ok() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        let result = tunnel.stop().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn health_check_is_false_before_start() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        assert!(!tunnel.health_check().await);
    }

    /// A missing binary must be diagnosable: the error names the program it
    /// tried to run, and never the auth token that rides in argv.
    #[tokio::test]
    async fn missing_program_error_names_the_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("ngrok");
        let tunnel = NgrokTunnel::new("rantaiclaw_ngrok_token".into(), None)
            .with_program(missing.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a program that does not exist cannot store a token");

        let message = format!("{err:#}");
        assert!(
            message.contains(missing.to_str().expect("utf-8 temp path")),
            "the error must name the program it tried to run: {message}"
        );
        assert!(
            !message.contains("rantaiclaw_ngrok_token"),
            "the error must not echo the auth token argument: {message}"
        );
    }

    /// `config add-authtoken` used to have its exit status ignored, so a
    /// rejected token surfaced 15 s later as "did not produce a public URL".
    /// It must fail here, naming the subcommand and never the token.
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_add_authtoken_names_the_subcommand_and_not_the_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = crate::tunnel::write_fake_program(dir.path(), "ngrok", "#!/bin/sh\nexit 1\n");
        let tunnel = NgrokTunnel::new("rantaiclaw_ngrok_token".into(), None)
            .with_program(fake.to_str().expect("utf-8 temp path"));

        let err = tunnel
            .start("127.0.0.1", 8080)
            .await
            .expect_err("a rejected token must stop the start");

        let message = format!("{err:#}");
        assert!(
            message.contains("config add-authtoken"),
            "the error must say which subcommand failed: {message}"
        );
        assert!(
            !message.contains("rantaiclaw_ngrok_token"),
            "the error must not echo the auth token argument: {message}"
        );
    }
}
