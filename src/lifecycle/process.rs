//! Best-effort process discovery + graceful stop, used to find and shut down a
//! stale gateway squatting on the console's gateway port. Callers MUST confirm
//! identity with `cmdline_contains` before calling `stop_process_graceful`, so
//! a reused PID never causes an unrelated process to be killed.

#[cfg(unix)]
use std::time::Duration;

/// Is a process with this PID alive? `kill(pid, 0)` probes existence.
pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// True when the process's command line contains every needle. Guards against
/// signalling the wrong process after PID reuse. False on non-unix / unreadable.
pub fn cmdline_contains(pid: u32, needles: &[&str]) -> bool {
    #[cfg(target_os = "linux")]
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| String::from_utf8_lossy(&raw).into_owned());
    #[cfg(all(unix, not(target_os = "linux")))]
    let cmd = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    #[cfg(not(unix))]
    let cmd: Option<String> = {
        let _ = pid;
        None
    };
    match cmd {
        Some(s) => needles.iter().all(|n| s.contains(n)),
        None => false,
    }
}

/// Find the PID listening on `port` (best-effort, via `ss`). `None` if the tool
/// is unavailable or nothing matches — callers must treat `None` as "unknown".
pub fn pid_listening_on_port(port: u16) -> Option<u32> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ss")
            .args(["-H", "-ltnp", &format!("sport = :{port}")])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        // ss prints `... users:(("proc",pid=1234,fd=7))`
        for line in text.lines() {
            if let Some(i) = line.find("pid=") {
                let digits: String = line[i + 4..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(pid) = digits.parse::<u32>() {
                    return Some(pid);
                }
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        let _ = port;
        None
    }
}

/// SIGTERM, wait up to ~2s for graceful exit, then SIGKILL. True when the
/// process is confirmed gone. Callers MUST identity-check first.
pub fn stop_process_graceful(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        for _ in 0..20 {
            if !process_is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        std::thread::sleep(Duration::from_millis(100));
        !process_is_alive(pid)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        assert!(process_is_alive(std::process::id()));
    }

    #[test]
    fn dead_pid_is_not_alive() {
        assert!(!process_is_alive(0x7fff_fffe));
    }

    #[test]
    fn cmdline_contains_positive_and_negative() {
        let me = std::process::id();
        // The cargo test binary's argv always contains its own path; assert the
        // POSITIVE path with a needle that must be present, and the negative.
        #[cfg(target_os = "linux")]
        {
            let raw = std::fs::read(format!("/proc/{me}/cmdline")).unwrap();
            let cmd = String::from_utf8_lossy(&raw);
            // pick the first path segment as a guaranteed-present needle
            let present = cmd.split('\0').next().unwrap_or("");
            assert!(!present.is_empty());
            let last = present.rsplit('/').next().unwrap();
            assert!(
                cmdline_contains(me, &[last]),
                "should match own binary name"
            );
        }
        assert!(!cmdline_contains(me, &["definitely-not-in-argv-zzz"]));
    }
}
