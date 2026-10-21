mod cloudflare;
mod custom;
mod ngrok;
mod none;
mod tailscale;

pub use cloudflare::CloudflareTunnel;
pub use custom::CustomTunnel;
pub use ngrok::NgrokTunnel;
#[allow(unused_imports)]
pub use none::NoneTunnel;
pub use tailscale::TailscaleTunnel;

use crate::config::schema::{TailscaleTunnelConfig, TunnelConfig};
use anyhow::{bail, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

// ── Tunnel trait ─────────────────────────────────────────────────

/// Agnostic tunnel abstraction — bring your own tunnel provider.
///
/// Implementations wrap an external tunnel binary (cloudflared, tailscale,
/// ngrok, etc.) or a custom command. The gateway calls `start()` after
/// binding its local port and `stop()` on shutdown.
#[async_trait::async_trait]
pub trait Tunnel: Send + Sync {
    /// Human-readable provider name (e.g. "cloudflare", "tailscale")
    fn name(&self) -> &str;

    /// Start the tunnel, exposing `local_host:local_port` externally.
    ///
    /// Returns the public URL **the provider itself printed**, or `None` when
    /// the provider reported none — `None` is a started tunnel with an unknown
    /// URL, never a failure. The gateway must never report a local or guessed
    /// address as public.
    async fn start(&self, local_host: &str, local_port: u16) -> Result<Option<String>>;

    /// Stop the tunnel process gracefully.
    async fn stop(&self) -> Result<()>;

    /// Check if the tunnel is still alive.
    async fn health_check(&self) -> bool;

    /// Return the public URL if the tunnel is running.
    fn public_url(&self) -> Option<String>;
}

/// Context for a spawn or output failure: the program plus the subcommand
/// words it ran, never the argument values — cloudflared and ngrok receive
/// their auth tokens as arguments, so echoing argv would leak them.
pub(crate) fn program_context(action: &str, program: &str, subcommand: &str) -> String {
    let words = if subcommand.is_empty() {
        String::new()
    } else {
        format!(" ({subcommand})")
    };
    format!("{action} {program}{words} — is {program} installed and on PATH?")
}

/// The first `<scheme>://…` token on one output line: from the scheme to the
/// next whitespace. Providers print their URL inside prose (`Registered…`,
/// `Available within your tailnet:`, `url=https://…`), so this is the one
/// place that turns a line into the URL the provider printed.
pub(crate) fn url_in(line: &str, scheme: &str) -> Option<String> {
    let start = line.find(scheme)?;
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Drain one piped stream of a tunnel child, logging every line at `debug`,
/// until the stream closes.
///
/// Every piped stream must stay drained for the child's whole life: a Go
/// tunnel binary exits with SIGPIPE on its next write to fd 1 or 2 once the
/// reader is gone (pkg.go.dev/os/signal), and a pipe nobody reads fills up and
/// blocks a child that never writes to fd 1 or 2. The task is detached, so it
/// keeps reading until the stream ends, whatever the caller does with its
/// (unused) handle.
pub(crate) fn spawn_drain<R>(label: &'static str, mut lines: tokio::io::Lines<R>)
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(stream = label, "{line}");
        }
    });
}

// ── Shared child-process handle ──────────────────────────────────

/// Wraps a spawned tunnel child process so implementations can share it.
pub(crate) struct TunnelProcess {
    pub child: tokio::process::Child,
    /// What the provider printed, if anything. `None` means the provider does
    /// not report its public URL (cloudflared in token mode, a custom command
    /// without a matching `url_pattern`).
    pub public_url: Option<String>,
}

pub(crate) type SharedProcess = Arc<Mutex<Option<TunnelProcess>>>;

pub(crate) fn new_shared_process() -> SharedProcess {
    Arc::new(Mutex::new(None))
}

/// How long a tunnel child gets to exit after SIGTERM before it is killed.
/// Fits well inside the daemon's `DRAIN_TIMEOUT` (16 s).
#[cfg(unix)]
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Stop a shared tunnel process if running.
///
/// On Unix the child gets SIGTERM first so it can clean up (tailscale's own
/// serve/funnel reset already ran before this), then SIGKILL once
/// [`STOP_GRACE`] passes, then it is reaped. A child that exits on SIGTERM
/// never waits for the bound.
pub(crate) async fn kill_shared(proc: &SharedProcess) -> Result<()> {
    let mut guard = proc.lock().await;
    if let Some(ref mut tp) = *guard {
        #[cfg(unix)]
        {
            let mut exited = false;
            if let Some(pid) = tp.child.id() {
                // Safety: `pid` is a live child of this process; SIGTERM to it
                // is the intended stop signal and carries no memory safety
                // requirement.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }

                let deadline = tokio::time::Instant::now() + STOP_GRACE;
                loop {
                    // `Ok(None)` means still running; any other result means
                    // it is gone (or can no longer be polled).
                    if !matches!(tp.child.try_wait(), Ok(None)) {
                        exited = true;
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }

            if !exited {
                tp.child.start_kill().ok();
            }
        }
        #[cfg(not(unix))]
        tp.child.kill().await.ok();

        tp.child.wait().await.ok();
    }
    *guard = None;
    Ok(())
}

// ── Factory ──────────────────────────────────────────────────────

/// Create a tunnel from config. Returns `None` for provider "none".
pub fn create_tunnel(config: &TunnelConfig) -> Result<Option<Box<dyn Tunnel>>> {
    match config.provider.as_str() {
        "none" | "" => Ok(None),

        "cloudflare" => {
            let cf = config
                .cloudflare
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("tunnel.provider = \"cloudflare\" but [tunnel.cloudflare] section is missing"))?;
            Ok(Some(Box::new(CloudflareTunnel::new(cf.token.clone()))))
        }

        "tailscale" => {
            let ts = config.tailscale.as_ref().unwrap_or(&TailscaleTunnelConfig {
                funnel: false,
                hostname: None,
            });
            Ok(Some(Box::new(TailscaleTunnel::new(
                ts.funnel,
                ts.hostname.clone(),
            ))))
        }

        "ngrok" => {
            let ng = config
                .ngrok
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("tunnel.provider = \"ngrok\" but [tunnel.ngrok] section is missing"))?;
            Ok(Some(Box::new(NgrokTunnel::new(
                ng.auth_token.clone(),
                ng.domain.clone(),
            ))))
        }

        "custom" => {
            let cu = config
                .custom
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("tunnel.provider = \"custom\" but [tunnel.custom] section is missing"))?;
            Ok(Some(Box::new(CustomTunnel::new(
                cu.start_command.clone(),
                cu.health_url.clone(),
                cu.url_pattern.clone(),
            ))))
        }

        other => bail!("Unknown tunnel provider: \"{other}\". Valid: none, cloudflare, tailscale, ngrok, custom"),
    }
}

// ── Tests ────────────────────────────────────────────────────────

/// Write an executable fake program for tests. Unix-only: the fakes are shell
/// scripts and the signal behaviour under test is POSIX.
///
/// The body is written by a helper process, not by this one: tests run in
/// parallel threads, and a `fork` in another thread while this thread holds the
/// file open for writing makes the child inherit that handle, after which the
/// kernel refuses to `exec` the script with `ETXTBSY` ("Text file busy").
#[cfg(all(test, unix))]
pub(crate) fn write_fake_program(
    dir: &std::path::Path,
    name: &str,
    body: &str,
) -> std::path::PathBuf {
    use std::io::Write as _;

    let path = dir.join(name);
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the helper that writes the fake program");
    child
        .stdin
        .as_mut()
        .expect("helper stdin is piped")
        .write_all(body.as_bytes())
        .expect("write the fake program");
    let status = child.wait().expect("wait for the fake program write");
    assert!(
        status.success(),
        "writing the fake program failed: {status}"
    );
    path
}

/// Whether a pid is still running.
///
/// `kill(pid, 0)` alone is not enough: a child that exited but was never
/// reaped is a zombie that still owns its pid, so the signal probe succeeds
/// for a dead child. Read the state field when `/proc` is there and treat a
/// zombie as dead; fall back to the signal probe where it is not.
#[cfg(all(test, unix))]
pub(crate) fn process_is_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .is_some_and(|state| state != 'Z'),
        // Safety: signal 0 carries no memory safety requirement and sends
        // nothing; it only reports whether the pid exists.
        Err(_) => unsafe { libc::kill(pid as libc::pid_t, 0) == 0 },
    }
}

/// Wait until a fake program has written its pid (a shell expands `$$` only
/// once it runs, so the file appearing means the trap is armed and the child
/// is committed to its loop).
#[cfg(all(test, unix))]
pub(crate) async fn wait_for_pid_file(path: &std::path::Path) -> u32 {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("fake program never wrote its pid to {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{
        CloudflareTunnelConfig, CustomTunnelConfig, NgrokTunnelConfig, TunnelConfig,
    };
    use tokio::process::Command;

    /// Helper: assert `create_tunnel` returns an error containing `needle`.
    fn assert_tunnel_err(cfg: &TunnelConfig, needle: &str) {
        match create_tunnel(cfg) {
            Err(e) => assert!(
                e.to_string().contains(needle),
                "Expected error containing \"{needle}\", got: {e}"
            ),
            Ok(_) => panic!("Expected error containing \"{needle}\", but got Ok"),
        }
    }

    #[test]
    fn factory_none_returns_none() {
        let cfg = TunnelConfig::default();
        let t = create_tunnel(&cfg).unwrap();
        assert!(t.is_none());
    }

    #[test]
    fn factory_empty_string_returns_none() {
        let cfg = TunnelConfig {
            provider: String::new(),
            ..TunnelConfig::default()
        };
        let t = create_tunnel(&cfg).unwrap();
        assert!(t.is_none());
    }

    #[test]
    fn factory_unknown_provider_errors() {
        let cfg = TunnelConfig {
            provider: "wireguard".into(),
            ..TunnelConfig::default()
        };
        assert_tunnel_err(&cfg, "Unknown tunnel provider");
    }

    #[test]
    fn factory_cloudflare_missing_config_errors() {
        let cfg = TunnelConfig {
            provider: "cloudflare".into(),
            ..TunnelConfig::default()
        };
        assert_tunnel_err(&cfg, "[tunnel.cloudflare]");
    }

    #[test]
    fn factory_cloudflare_with_config_ok() {
        let cfg = TunnelConfig {
            provider: "cloudflare".into(),
            cloudflare: Some(CloudflareTunnelConfig {
                token: "test-token".into(),
            }),
            ..TunnelConfig::default()
        };
        let t = create_tunnel(&cfg).unwrap();
        assert!(t.is_some());
        assert_eq!(t.unwrap().name(), "cloudflare");
    }

    #[test]
    fn factory_tailscale_defaults_ok() {
        let cfg = TunnelConfig {
            provider: "tailscale".into(),
            ..TunnelConfig::default()
        };
        let t = create_tunnel(&cfg).unwrap();
        assert!(t.is_some());
        assert_eq!(t.unwrap().name(), "tailscale");
    }

    #[test]
    fn factory_ngrok_missing_config_errors() {
        let cfg = TunnelConfig {
            provider: "ngrok".into(),
            ..TunnelConfig::default()
        };
        assert_tunnel_err(&cfg, "[tunnel.ngrok]");
    }

    #[test]
    fn factory_ngrok_with_config_ok() {
        let cfg = TunnelConfig {
            provider: "ngrok".into(),
            ngrok: Some(NgrokTunnelConfig {
                auth_token: "tok".into(),
                domain: None,
            }),
            ..TunnelConfig::default()
        };
        let t = create_tunnel(&cfg).unwrap();
        assert!(t.is_some());
        assert_eq!(t.unwrap().name(), "ngrok");
    }

    #[test]
    fn factory_custom_missing_config_errors() {
        let cfg = TunnelConfig {
            provider: "custom".into(),
            ..TunnelConfig::default()
        };
        assert_tunnel_err(&cfg, "[tunnel.custom]");
    }

    #[test]
    fn factory_custom_with_config_ok() {
        let cfg = TunnelConfig {
            provider: "custom".into(),
            custom: Some(CustomTunnelConfig {
                start_command: "echo tunnel".into(),
                health_url: None,
                url_pattern: None,
            }),
            ..TunnelConfig::default()
        };
        let t = create_tunnel(&cfg).unwrap();
        assert!(t.is_some());
        assert_eq!(t.unwrap().name(), "custom");
    }

    #[test]
    fn none_tunnel_name() {
        let t = NoneTunnel;
        assert_eq!(t.name(), "none");
    }

    #[test]
    fn none_tunnel_public_url_is_none() {
        let t = NoneTunnel;
        assert!(t.public_url().is_none());
    }

    #[tokio::test]
    async fn none_tunnel_health_always_true() {
        let t = NoneTunnel;
        assert!(t.health_check().await);
    }

    #[tokio::test]
    async fn none_tunnel_start_returns_local() {
        let t = NoneTunnel;
        let url = t.start("127.0.0.1", 8080).await.unwrap();
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn cloudflare_tunnel_name() {
        let t = CloudflareTunnel::new("tok".into());
        assert_eq!(t.name(), "cloudflare");
        assert!(t.public_url().is_none());
    }

    #[test]
    fn tailscale_tunnel_name() {
        let t = TailscaleTunnel::new(false, None);
        assert_eq!(t.name(), "tailscale");
        assert!(t.public_url().is_none());
    }

    #[test]
    fn tailscale_funnel_mode() {
        let t = TailscaleTunnel::new(true, Some("myhost".into()));
        assert_eq!(t.name(), "tailscale");
    }

    #[test]
    fn ngrok_tunnel_name() {
        let t = NgrokTunnel::new("tok".into(), None);
        assert_eq!(t.name(), "ngrok");
        assert!(t.public_url().is_none());
    }

    #[test]
    fn ngrok_with_domain() {
        let t = NgrokTunnel::new("tok".into(), Some("my.ngrok.io".into()));
        assert_eq!(t.name(), "ngrok");
    }

    #[test]
    fn custom_tunnel_name() {
        let t = CustomTunnel::new("echo hi".into(), None, None);
        assert_eq!(t.name(), "custom");
        assert!(t.public_url().is_none());
    }

    #[tokio::test]
    async fn kill_shared_no_process_is_ok() {
        let proc = new_shared_process();
        let result = kill_shared(&proc).await;

        assert!(result.is_ok());
        assert!(proc.lock().await.is_none());
    }

    #[tokio::test]
    async fn kill_shared_terminates_and_clears_child() {
        let proc = new_shared_process();

        let child = Command::new("sleep")
            .arg("30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("sleep should spawn for lifecycle test");

        {
            let mut guard = proc.lock().await;
            *guard = Some(TunnelProcess {
                child,
                public_url: Some("https://example.test".into()),
            });
        }

        let started = tokio::time::Instant::now();
        kill_shared(&proc).await.unwrap();
        let elapsed = started.elapsed();

        let guard = proc.lock().await;
        assert!(guard.is_none());
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "a child that exits on SIGTERM must not wait out the grace bound: {elapsed:?}"
        );
    }

    /// A child that ignores SIGTERM must still be dead once the grace bound
    /// passes: SIGTERM first (the trap records it), then SIGKILL, then reap.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_shared_sigkills_a_child_that_ignores_sigterm_within_the_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let term_file = dir.path().join("term");
        let fake = write_fake_program(
            dir.path(),
            "stubborn",
            &format!(
                "#!/bin/sh\ntrap 'echo term > {}' TERM\necho $$ > {}\nwhile :; do sleep 0.2; done\n",
                term_file.display(),
                pid_file.display()
            ),
        );

        let child = Command::new(&fake)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the stubborn fake should spawn");
        let pid = wait_for_pid_file(&pid_file).await;

        let proc = new_shared_process();
        {
            let mut guard = proc.lock().await;
            *guard = Some(TunnelProcess {
                child,
                public_url: Some("https://example.test".into()),
            });
        }

        let started = tokio::time::Instant::now();
        kill_shared(&proc).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            term_file.exists(),
            "SIGTERM must be tried before SIGKILL: the child never saw it"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "SIGKILL must land within the 2 s bound plus margin: {elapsed:?}"
        );
        assert!(
            !process_is_alive(pid),
            "the child must be dead after kill_shared: pid {pid}"
        );
    }

    #[tokio::test]
    async fn cloudflare_health_false_before_start() {
        let tunnel = CloudflareTunnel::new("tok".into());
        assert!(!tunnel.health_check().await);
    }

    #[tokio::test]
    async fn ngrok_health_false_before_start() {
        let tunnel = NgrokTunnel::new("tok".into(), None);
        assert!(!tunnel.health_check().await);
    }

    #[tokio::test]
    async fn tailscale_health_false_before_start() {
        let tunnel = TailscaleTunnel::new(false, None);
        assert!(!tunnel.health_check().await);
    }

    #[tokio::test]
    async fn custom_health_false_before_start_without_health_url() {
        let tunnel = CustomTunnel::new("echo hi".into(), None, Some("https://".into()));
        assert!(!tunnel.health_check().await);
    }
}
