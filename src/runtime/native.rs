use super::traits::RuntimeAdapter;
use std::path::{Path, PathBuf};

/// Native runtime — full access, runs on Mac/Linux/Docker/Raspberry Pi
pub struct NativeRuntime;

impl NativeRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl RuntimeAdapter for NativeRuntime {
    fn name(&self) -> &str {
        "native"
    }

    fn has_shell_access(&self) -> bool {
        true
    }

    fn has_filesystem_access(&self) -> bool {
        true
    }

    fn storage_path(&self) -> PathBuf {
        directories::UserDirs::new().map_or_else(
            || PathBuf::from(".rantaiclaw"),
            |u| u.home_dir().join(".rantaiclaw"),
        )
    }

    fn supports_long_running(&self) -> bool {
        true
    }

    /// Each shell command runs in its own session/process group (via `setsid`
    /// in `build_shell_command`), so the shell tool can reap the whole tree with
    /// `kill(-pid, …)` on cancel/timeout. Unix only; `false` elsewhere.
    fn spawns_process_group(&self) -> bool {
        cfg!(unix)
    }

    fn build_shell_command(
        &self,
        command: &str,
        workspace_dir: &Path,
    ) -> anyhow::Result<tokio::process::Command> {
        // Use the absolute POSIX path so spawn doesn't depend on PATH
        // being intact in the rantaiclaw process env. The shell tool
        // does `env_clear()` then re-adds a `SAFE_ENV_VARS` allowlist
        // (which includes PATH from the parent); if the parent process
        // somehow lost PATH, `Command::new("sh")` would fail with
        // "No such file or directory" at spawn time — the v0.6.50 user
        // report. `/bin/sh` is POSIX-mandated and present on Linux,
        // macOS, BSD, and every container image we support.
        const SH_PATH: &str = if cfg!(target_os = "windows") {
            "sh"
        } else {
            "/bin/sh"
        };
        let mut process = tokio::process::Command::new(SH_PATH);
        process.arg("-c").arg(command).current_dir(workspace_dir);
        // Put the shell in its own session/process group so the shell tool can
        // reap the WHOLE tree (apt/dpkg, ssh-to-VM, pipelines, background jobs)
        // with `kill(-pgid, …)` on cancel/timeout. `kill_on_drop` below only
        // stops this direct `/bin/sh` child, leaving descendants running.
        // Mirrors src/webui.rs. Unix only.
        #[cfg(unix)]
        {
            // SAFETY: setsid() only starts a new session in the post-fork child,
            // before exec; nothing else runs there. `pre_exec` is inherent on
            // tokio's Command (no CommandExt import needed).
            unsafe {
                process.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        // Kill the direct child if the parent future is dropped — covers the
        // Ctrl+C path in the TUI where `execute_tool_call` is cancelled
        // mid-run. Belt-and-suspenders alongside the process-group kill.
        process.kill_on_drop(true);
        Ok(process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_name() {
        assert_eq!(NativeRuntime::new().name(), "native");
    }

    #[test]
    fn native_has_shell_access() {
        assert!(NativeRuntime::new().has_shell_access());
    }

    #[test]
    fn native_has_filesystem_access() {
        assert!(NativeRuntime::new().has_filesystem_access());
    }

    #[test]
    fn native_supports_long_running() {
        assert!(NativeRuntime::new().supports_long_running());
    }

    #[test]
    fn native_memory_budget_unlimited() {
        assert_eq!(NativeRuntime::new().memory_budget(), 0);
    }

    #[test]
    fn native_storage_path_contains_rantaiclaw() {
        let path = NativeRuntime::new().storage_path();
        assert!(path.to_string_lossy().contains("rantaiclaw"));
    }

    #[test]
    fn native_build_with_cleanup_has_no_reaper() {
        // The native runtime's process-group kill fully reaps the child, so it
        // uses the default (no out-of-group container to force-remove).
        let prepared = NativeRuntime::new()
            .build_shell_command_with_cleanup("echo hi", &std::env::temp_dir())
            .unwrap();
        assert!(prepared.cancel_reaper.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_on_drop_terminates_child_when_future_dropped() {
        // Proves the fix for the v0.6.49 "cancel not instant" bug:
        // dropping the future returned by Command::output() must actually
        // kill the running child process. Spawn `sleep 30`, drop after
        // 100ms, and confirm the future resolves in <1s instead of
        // waiting the full 30s.
        let runtime = NativeRuntime::new();
        let mut cmd = runtime
            .build_shell_command("sleep 30", std::env::temp_dir().as_path())
            .expect("build command");
        let start = std::time::Instant::now();
        // Race the 30s shell against a 100ms sleep. tokio::select! drops
        // the loser branch's future, so when the 100ms timer wins we drop
        // the in-flight `sleep 30`. With kill_on_drop(true) the child
        // gets SIGKILL'd and the drop returns immediately; without it,
        // the future would block waiting for the 30s child to exit.
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                _ = cmd.output() => unreachable!("30s sleep can't finish in 100ms"),
            }
        })
        .await;
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "kill_on_drop didn't take effect — select drop blocked for >3s"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "select drop took {elapsed:?}, expected sub-second (kill_on_drop should make it instant)"
        );
    }

    #[test]
    fn native_builds_shell_command() {
        let cwd = std::env::temp_dir();
        let command = NativeRuntime::new()
            .build_shell_command("echo hello", &cwd)
            .unwrap();
        let debug = format!("{command:?}");
        assert!(debug.contains("echo hello"));
    }
}
