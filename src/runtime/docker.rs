use super::traits::{CancelReaper, PreparedShellCommand, RuntimeAdapter};
use crate::config::DockerRuntimeConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Docker runtime with lightweight container isolation.
#[derive(Debug, Clone)]
pub struct DockerRuntime {
    config: DockerRuntimeConfig,
}

impl DockerRuntime {
    pub fn new(config: DockerRuntimeConfig) -> Self {
        Self { config }
    }

    fn workspace_mount_path(&self, workspace_dir: &Path) -> Result<PathBuf> {
        let resolved = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());

        if !resolved.is_absolute() {
            anyhow::bail!(
                "Docker runtime requires an absolute workspace path, got: {}",
                resolved.display()
            );
        }

        if resolved == Path::new("/") {
            anyhow::bail!("Refusing to mount filesystem root (/) into docker runtime");
        }

        if self.config.allowed_workspace_roots.is_empty() {
            return Ok(resolved);
        }

        let allowed = self.config.allowed_workspace_roots.iter().any(|root| {
            let root_path = Path::new(root)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(root));
            resolved.starts_with(root_path)
        });

        if !allowed {
            anyhow::bail!(
                "Workspace path {} is not in runtime.docker.allowed_workspace_roots",
                resolved.display()
            );
        }

        Ok(resolved)
    }

    /// Shared `docker run` command builder. When `container_name` is set, adds
    /// `--name <name>` so the shell tool can `docker rm -f` the container on
    /// cancel/timeout — the container is managed by dockerd, outside the CLI's
    /// killed process group, so a SIGTERM-ignoring workload would otherwise orphan.
    fn build_docker_run(
        &self,
        command: &str,
        workspace_dir: &Path,
        container_name: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        let mut process = tokio::process::Command::new("docker");
        process
            .arg("run")
            .arg("--rm")
            .arg("--init")
            .arg("--interactive")
            // Forward signals from the `docker run` CLI into the container's PID 1
            // (default in non-TTY mode — we run without `-t` — but set explicitly).
            // This is what lets a SIGTERM'd CLI actually stop the container.
            .arg("--sig-proxy=true");

        if let Some(name) = container_name {
            process.arg("--name").arg(name);
        }

        let network = self.config.network.trim();
        if !network.is_empty() {
            process.arg("--network").arg(network);
        }

        if let Some(memory_limit_mb) = self.config.memory_limit_mb.filter(|mb| *mb > 0) {
            process.arg("--memory").arg(format!("{memory_limit_mb}m"));
        }

        if let Some(cpu_limit) = self.config.cpu_limit.filter(|cpus| *cpus > 0.0) {
            process.arg("--cpus").arg(cpu_limit.to_string());
        }

        if self.config.read_only_rootfs {
            process.arg("--read-only");
        }

        if self.config.mount_workspace {
            let host_workspace = self.workspace_mount_path(workspace_dir).with_context(|| {
                format!(
                    "Failed to validate workspace mount path {}",
                    workspace_dir.display()
                )
            })?;

            process
                .arg("--volume")
                .arg(format!("{}:/workspace:rw", host_workspace.display()))
                .arg("--workdir")
                .arg("/workspace");
        }

        process
            .arg(self.config.image.trim())
            .arg("sh")
            .arg("-c")
            .arg(command);

        // Reap the container on cancel/timeout. On Unix, put the `docker run` CLI
        // in its own process group (mirrors native) so the shell tool can SIGTERM
        // the group; `--sig-proxy` forwards that into the container, which exits
        // and is auto-removed by `--rm`. We deliberately do NOT use `kill_on_drop`
        // on Unix: its SIGKILL would race — and win — against the CLI forwarding
        // the SIGTERM, leaving the container running (the old bug). A container
        // that IGNORES the forwarded SIGTERM is force-removed by the
        // `build_shell_command_with_cleanup` reaper (`docker rm -f <name>`). On
        // non-Unix (no process-group support), fall back to `kill_on_drop`.
        #[cfg(unix)]
        {
            // SAFETY: setsid() only starts a new session in the post-fork child,
            // before exec; nothing else runs there. Mirrors src/webui.rs.
            unsafe {
                process.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        process.kill_on_drop(true);

        Ok(process)
    }
}

impl RuntimeAdapter for DockerRuntime {
    fn name(&self) -> &str {
        "docker"
    }

    fn has_shell_access(&self) -> bool {
        true
    }

    fn has_filesystem_access(&self) -> bool {
        self.config.mount_workspace
    }

    fn storage_path(&self) -> PathBuf {
        if self.config.mount_workspace {
            PathBuf::from("/workspace/.rantaiclaw")
        } else {
            PathBuf::from("/tmp/.rantaiclaw")
        }
    }

    fn supports_long_running(&self) -> bool {
        false
    }

    fn memory_budget(&self) -> u64 {
        self.config
            .memory_limit_mb
            .map_or(0, |mb| mb.saturating_mul(1024 * 1024))
    }

    /// The `docker run` CLI runs in its own process group (via `setsid`, below).
    /// On cancel/timeout the shell tool SIGTERMs that group; `docker run`'s
    /// `--sig-proxy` (non-TTY) then forwards the signal into the container, which
    /// exits and is auto-removed by `--rm`. Unix only.
    fn spawns_process_group(&self) -> bool {
        cfg!(unix)
    }

    fn build_shell_command(
        &self,
        command: &str,
        workspace_dir: &Path,
    ) -> anyhow::Result<tokio::process::Command> {
        self.build_docker_run(command, workspace_dir, None)
    }

    fn build_shell_command_with_cleanup(
        &self,
        command: &str,
        workspace_dir: &Path,
    ) -> anyhow::Result<PreparedShellCommand> {
        // Unique container name so the shell tool can force-remove exactly this
        // container on cancel/timeout without touching any other run.
        let container_name = format!(
            "rantaiclaw-run-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let command = self.build_docker_run(command, workspace_dir, Some(&container_name))?;
        Ok(PreparedShellCommand {
            command,
            cancel_reaper: Some(CancelReaper { container_name }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_runtime_name() {
        let runtime = DockerRuntime::new(DockerRuntimeConfig::default());
        assert_eq!(runtime.name(), "docker");
    }

    #[test]
    fn docker_cleanup_names_container_and_returns_matching_reaper() {
        let runtime = DockerRuntime::new(DockerRuntimeConfig::default());
        let prepared = runtime
            .build_shell_command_with_cleanup("echo hi", &std::env::temp_dir())
            .unwrap();
        let reaper = prepared
            .cancel_reaper
            .expect("docker runtime must provide a cancel reaper");
        assert!(
            reaper.container_name.starts_with("rantaiclaw-run-"),
            "name: {}",
            reaper.container_name
        );
        // The command must carry `--name <that same name>` so the reaper's
        // `docker rm -f` targets exactly this container.
        let args: Vec<String> = prepared
            .command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let name_idx = args
            .iter()
            .position(|a| a == "--name")
            .expect("--name arg present");
        assert_eq!(args[name_idx + 1], reaper.container_name);
    }

    #[test]
    fn docker_plain_build_has_no_container_name() {
        let runtime = DockerRuntime::new(DockerRuntimeConfig::default());
        let cmd = runtime
            .build_shell_command("echo hi", &std::env::temp_dir())
            .unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a == "--name"), "args: {args:?}");
    }

    #[test]
    fn docker_spawns_process_group_on_unix() {
        // Enables the shell tool's group-kill guard: SIGTERM the `docker run`
        // group -> --sig-proxy forwards to the container -> --rm removes it.
        let runtime = DockerRuntime::new(DockerRuntimeConfig::default());
        assert_eq!(runtime.spawns_process_group(), cfg!(unix));
    }

    #[test]
    fn docker_build_shell_command_forwards_signals() {
        let runtime = DockerRuntime::new(DockerRuntimeConfig::default());
        let command = runtime
            .build_shell_command("echo hi", &std::env::temp_dir())
            .unwrap();
        assert!(format!("{command:?}").contains("--sig-proxy=true"));
    }

    #[test]
    fn docker_runtime_memory_budget() {
        let mut cfg = DockerRuntimeConfig::default();
        cfg.memory_limit_mb = Some(256);
        let runtime = DockerRuntime::new(cfg);
        assert_eq!(runtime.memory_budget(), 256 * 1024 * 1024);
    }

    #[test]
    fn docker_build_shell_command_includes_runtime_flags() {
        let cfg = DockerRuntimeConfig {
            image: "alpine:3.20".into(),
            network: "none".into(),
            memory_limit_mb: Some(128),
            cpu_limit: Some(1.5),
            read_only_rootfs: true,
            mount_workspace: true,
            allowed_workspace_roots: Vec::new(),
        };
        let runtime = DockerRuntime::new(cfg);

        let workspace = std::env::temp_dir();
        let command = runtime
            .build_shell_command("echo hello", &workspace)
            .unwrap();
        let debug = format!("{command:?}");

        assert!(debug.contains("docker"));
        assert!(debug.contains("--memory"));
        assert!(debug.contains("128m"));
        assert!(debug.contains("--cpus"));
        assert!(debug.contains("1.5"));
        assert!(debug.contains("--workdir"));
        assert!(debug.contains("echo hello"));
    }

    #[test]
    fn docker_workspace_allowlist_blocks_outside_paths() {
        let cfg = DockerRuntimeConfig {
            allowed_workspace_roots: vec!["/tmp/allowed".into()],
            ..DockerRuntimeConfig::default()
        };
        let runtime = DockerRuntime::new(cfg);

        let outside = PathBuf::from("/tmp/blocked_workspace");
        let result = runtime.build_shell_command("echo test", &outside);

        assert!(result.is_err());
    }

    // ── §3.3 / §3.4 Docker mount & network isolation tests ──

    #[test]
    fn docker_build_shell_command_includes_network_flag() {
        let cfg = DockerRuntimeConfig {
            network: "none".into(),
            ..DockerRuntimeConfig::default()
        };
        let runtime = DockerRuntime::new(cfg);
        let workspace = std::env::temp_dir();
        let cmd = runtime
            .build_shell_command("echo hello", &workspace)
            .unwrap();
        let debug = format!("{cmd:?}");
        assert!(
            debug.contains("--network") && debug.contains("none"),
            "must include --network none for isolation"
        );
    }

    #[test]
    fn docker_build_shell_command_includes_read_only_flag() {
        let cfg = DockerRuntimeConfig {
            read_only_rootfs: true,
            ..DockerRuntimeConfig::default()
        };
        let runtime = DockerRuntime::new(cfg);
        let workspace = std::env::temp_dir();
        let cmd = runtime
            .build_shell_command("echo hello", &workspace)
            .unwrap();
        let debug = format!("{cmd:?}");
        assert!(
            debug.contains("--read-only"),
            "must include --read-only flag when read_only_rootfs is set"
        );
    }

    #[cfg(unix)]
    #[test]
    fn docker_refuses_root_mount() {
        let cfg = DockerRuntimeConfig {
            mount_workspace: true,
            ..DockerRuntimeConfig::default()
        };
        let runtime = DockerRuntime::new(cfg);
        let result = runtime.build_shell_command("echo test", Path::new("/"));
        assert!(
            result.is_err(),
            "mounting filesystem root (/) must be refused"
        );
        let error_chain = format!("{:#}", result.unwrap_err());
        assert!(
            error_chain.contains("root"),
            "expected root-mount error chain, got: {error_chain}"
        );
    }

    #[test]
    fn docker_no_memory_flag_when_not_configured() {
        let cfg = DockerRuntimeConfig {
            memory_limit_mb: None,
            ..DockerRuntimeConfig::default()
        };
        let runtime = DockerRuntime::new(cfg);
        let workspace = std::env::temp_dir();
        let cmd = runtime
            .build_shell_command("echo hello", &workspace)
            .unwrap();
        let debug = format!("{cmd:?}");
        assert!(
            !debug.contains("--memory"),
            "should not include --memory when not configured"
        );
    }
}
