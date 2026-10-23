//! Daemon checks — the daemon's own scheduler component, and the service
//! registration of the installation being diagnosed.

use std::path::Path;

use async_trait::async_trait;

use crate::daemon::SchedulerVerdict;
use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};
use crate::service::{is_dev_build, DEV_BUILD_SKIP_MESSAGE};

/// The daemon's scheduler component, read from the daemon's own state file.
///
/// The capability existed only in the TUI (`scheduler_diag`), which is how
/// `doctor`'s help could promise a scheduler diagnosis with no check behind
/// it. Both surfaces now read one verdict ([`crate::daemon::scheduler_verdict`]),
/// so they cannot disagree about what a state file means.
pub struct SchedulerCheck;

#[async_trait]
impl DoctorCheck for SchedulerCheck {
    fn name(&self) -> &'static str {
        "daemon.scheduler"
    }
    fn category(&self) -> &'static str {
        // Not `live`: the verdict comes from a local file, and brief mode —
        // the console's mode — skips every `live` check. A gap the console
        // keeps is a gap.
        "system"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let cat = self.category();
        let state_file = crate::daemon::state_file_path(&ctx.config);
        // A file read is blocking work; keep it off the async runtime so a
        // doctor poll cannot stall an SSE chat worker.
        let verdict =
            tokio::task::spawn_blocking(move || crate::daemon::scheduler_verdict(&state_file))
                .await
                .unwrap_or(SchedulerVerdict::DaemonNotRunning);
        render_scheduler(self.name(), cat, &verdict)
    }
}

/// Turn a scheduler verdict into the check's line.
///
/// The wording is [`SchedulerVerdict::detail`]'s — the same words the TUI's
/// row carries — so the two surfaces read alike.
fn render_scheduler(name: &str, category: &'static str, verdict: &SchedulerVerdict) -> CheckResult {
    match verdict {
        SchedulerVerdict::Healthy => {
            CheckResult::ok(name, verdict.detail()).with_category(category)
        }
        SchedulerVerdict::Unhealthy => CheckResult::fail(name, verdict.detail())
            .with_category(category)
            .with_hint("see the daemon log for the scheduler component"),
        SchedulerVerdict::DaemonNotRunning
        | SchedulerVerdict::NotTracked
        | SchedulerVerdict::Other(_) => {
            CheckResult::warn(name, verdict.detail()).with_category(category)
        }
    }
}

/// The daemon's service registration, as the host service manager sees it.
pub struct DaemonRegistrationCheck;

#[async_trait]
impl DoctorCheck for DaemonRegistrationCheck {
    fn name(&self) -> &'static str {
        "daemon.registration"
    }
    fn category(&self) -> &'static str {
        "system"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let cat = self.category();
        // `detect_registration_detailed` shells out to systemctl/launchctl and
        // scans the PATH — blocking work that must not run on the async runtime
        // (it would stall the worker other SSE chat streams share). Move it to
        // a blocking pool.
        let profile = ctx.profile.name.clone();
        let registration =
            tokio::task::spawn_blocking(move || detect_registration_detailed(&profile))
                .await
                .unwrap_or(Registration::Unsupported);
        render_registration(self.name(), cat, &registration)
    }
}

/// What the host service manager reported about one candidate unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnitReport {
    /// The unit name that was asked for.
    pub(crate) unit: String,
    /// Whether the unit is active.
    pub(crate) active: bool,
    /// The unit's `ExecStart` property, when the manager reported one.
    pub(crate) exec_start: Option<String>,
    /// The unit file the manager loaded the unit from.
    pub(crate) fragment_path: Option<String>,
}

/// What one query of the service manager answered.
///
/// The three answers are the three things a host can say, and they are not
/// the same thing: "the unit is not installed", "the unit is installed" and
/// "the manager did not answer" are different facts, and only the first two
/// are about the unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The manager answered: the unit is not installed.
    Absent,
    /// The manager answered and reported the unit.
    Present(UnitReport),
    /// The manager did not answer — nothing about the unit can be proven.
    Indeterminate,
}

/// What the doctor learned about the daemon service of the installation it is
/// diagnosing.
///
/// Every variant names the manager that was asked and the unit it was asked
/// for: a check that queries the host must report on the installation it is
/// diagnosing, not on whatever unit the host happens to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Registration {
    /// A unit is installed and runs the binary running doctor.
    Registered {
        manager: &'static str,
        unit: String,
        /// The executable the unit runs, as the unit file names it.
        binary: String,
        fragment_path: Option<String>,
    },
    /// A unit is installed but not running.
    Inactive {
        manager: &'static str,
        unit: String,
        fragment_path: Option<String>,
    },
    /// A unit is installed, but it runs a different binary — another
    /// installation's unit, not this one's.
    ForeignBinary {
        manager: &'static str,
        unit: String,
        found: String,
        expected: String,
    },
    /// A unit is installed, but the binary it runs could not be verified:
    /// the manager named no executable that can be read, or this process's
    /// own path could not be resolved. The unit may well be this
    /// installation's — but the check cannot say so, and must not say so.
    Unverified {
        manager: &'static str,
        unit: String,
        /// Whether the unit is running.
        active: bool,
        fragment_path: Option<String>,
    },
    /// The service manager did not answer, so the registration status is
    /// unknown: neither registered nor absent.
    ManagerUnreachable { manager: &'static str, unit: String },
    /// No unit of that name is installed.
    NotRegistered {
        manager: &'static str,
        candidates: Vec<String>,
    },
    /// No init system was detected.
    Unsupported,
    /// A debug or test build — never queried the service manager.
    #[cfg(any(test, debug_assertions))]
    DevBuild,
}

/// Whether the unit's `ExecStart` names the binary running doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BinaryMatch {
    /// The unit runs this binary. Carries the executable as the unit file
    /// names it, so the line can name it.
    Match { binary: String },
    /// The unit runs a different binary than the one running doctor.
    Mismatch { found: String, expected: String },
    /// Nothing could be proven: the manager named no executable, or this
    /// process's own path could not be resolved.
    Unknown,
}

/// The unit names this installation's daemon may be registered under.
///
/// `rantaiclaw.service` is the only name the installer creates
/// (`src/service/mod.rs`). The templated `rantaiclaw@<profile>.service` is the
/// daemon's own convention ([`crate::daemon::handoff::default_unit_name`]),
/// used when the daemon sentinel carries no unit name — a host that follows
/// that convention would otherwise be reported as having no unit at all.
#[cfg(target_os = "linux")]
pub(crate) fn candidate_units(profile: &str) -> Vec<String> {
    vec![
        "rantaiclaw.service".to_string(),
        crate::daemon::handoff::default_unit_name(profile),
    ]
}

/// The executable an `ExecStart=` property names.
///
/// `systemctl show` prints the parsed command:
/// `{ path=/usr/local/bin/rantaiclaw ; argv[]=/usr/local/bin/rantaiclaw daemon ; ... }`.
/// The `path=` field is the executable; `argv[]` is the fallback for a unit
/// that reports no `path=` — and only when it names a single word, because
/// whitespace alone cannot tell the executable from its arguments.
pub(crate) fn exec_start_binary(exec_start: &str) -> Option<&str> {
    let inner = exec_start
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}');
    let mut argv: Option<&str> = None;
    for part in inner.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("path=") {
            let value = unquote(value.trim());
            if !value.is_empty() {
                return Some(value);
            }
        } else if let Some(value) = part.strip_prefix("argv[]=") {
            argv = Some(value.trim());
        }
    }
    let argv = argv?;
    // A value with more than one word is ambiguous: the executable cannot be
    // separated from its arguments by whitespace alone, and a path that
    // contains a space (`/opt/my dir/rantaiclaw`) would split into two words
    // and name a binary that does not exist. Better no executable than a
    // wrong one.
    if argv.chars().any(char::is_whitespace) {
        return None;
    }
    let first = unquote(argv);
    (!first.is_empty()).then_some(first)
}

/// Strip the quotes systemd puts around a path that contains a space.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

/// One `Key=Value` line out of `systemctl show` output.
fn property_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(|value| value.trim().to_string())
}

/// Compare the unit's executable with the binary running doctor.
///
/// `expected` is this process's own path. `None` means it could not be
/// resolved — then nothing can be proven either way.
pub(crate) fn match_exec_start(exec_start: Option<&str>, expected: Option<&Path>) -> BinaryMatch {
    let Some(expected) = expected else {
        // This process's own path could not be resolved: a unit running a
        // plainly different binary cannot be told apart from this one.
        return BinaryMatch::Unknown;
    };
    let Some(raw) = exec_start else {
        // The manager named no executable at all.
        return BinaryMatch::Unknown;
    };
    match exec_start_binary(raw) {
        Some(binary) if same_file(Path::new(binary), expected) => BinaryMatch::Match {
            binary: binary.to_string(),
        },
        Some(binary) => BinaryMatch::Mismatch {
            found: binary.to_string(),
            expected: expected.display().to_string(),
        },
        // The unit names no executable that can be read, so it can be proven
        // neither to be running this binary nor to run a different one.
        None => BinaryMatch::Unknown,
    }
}

/// Whether two paths name the same file — symlinks and `..` resolved.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Decide what the unit the manager reported means for the installation being
/// diagnosed.
///
/// Pure: it spawns nothing and reads nothing outside the report it is handed,
/// so a test can drive it with a synthetic unit — and never with a real
/// service manager.
pub(crate) fn classify_unit(
    manager: &'static str,
    report: &UnitReport,
    expected_binary: Option<&Path>,
) -> Registration {
    match match_exec_start(report.exec_start.as_deref(), expected_binary) {
        BinaryMatch::Mismatch { found, expected } => Registration::ForeignBinary {
            manager,
            unit: report.unit.clone(),
            found,
            expected,
        },
        BinaryMatch::Match { binary } => {
            if report.active {
                Registration::Registered {
                    manager,
                    unit: report.unit.clone(),
                    binary,
                    fragment_path: report.fragment_path.clone(),
                }
            } else {
                Registration::Inactive {
                    manager,
                    unit: report.unit.clone(),
                    fragment_path: report.fragment_path.clone(),
                }
            }
        }
        // Nothing can be proven about the binary the unit runs — so the
        // registration cannot be claimed either. "Not running" is a fact the
        // manager did report; it is not a registration claim.
        BinaryMatch::Unknown => Registration::Unverified {
            manager,
            unit: report.unit.clone(),
            active: report.active,
            fragment_path: report.fragment_path.clone(),
        },
    }
}

/// Turn a registration verdict into the check's line.
///
/// Every line names the manager that was asked and the unit it was asked for.
pub(crate) fn render_registration(
    name: &str,
    category: &'static str,
    registration: &Registration,
) -> CheckResult {
    match registration {
        Registration::Registered {
            manager,
            unit,
            binary,
            fragment_path,
        } => {
            let message = format!("daemon registered via {manager} as unit {unit} (runs {binary})");
            let mut result = CheckResult::ok(name, message).with_category(category);
            if let Some(path) = fragment_path {
                result = result.with_hint(format!("unit file: {path}"));
            }
            result
        }
        Registration::Inactive { manager, unit, .. } => CheckResult::info(
            name,
            format!("daemon registered with {manager} as unit {unit} but not running"),
        )
        .with_category(category)
        .with_hint("run: rantaiclaw service start"),
        Registration::ForeignBinary {
            manager,
            unit,
            found,
            expected,
        } => CheckResult::warn(
            name,
            format!(
                "unit {unit} is registered with {manager} but runs {found} — \
                 not this binary ({expected})"
            ),
        )
        .with_category(category)
        .with_hint("run: rantaiclaw service install to point the unit at this binary"),
        Registration::Unverified {
            manager,
            unit,
            active,
            ..
        } => {
            let message = if *active {
                format!(
                    "unit {unit} is registered with {manager} and is active, but the binary it \
                     runs could not be verified — not confirmed as this installation"
                )
            } else {
                format!(
                    "unit {unit} is registered with {manager} but is not running, and the binary \
                     it runs could not be verified — not confirmed as this installation"
                )
            };
            CheckResult::warn(name, message)
                .with_category(category)
                .with_hint("check the unit's ExecStart to see which binary it runs")
        }
        Registration::ManagerUnreachable { manager, unit } => CheckResult::warn(
            name,
            format!(
                "could not query {manager} for unit {unit} — the user service manager did not \
                 answer; registration status unknown"
            ),
        )
        .with_category(category),
        Registration::NotRegistered {
            manager,
            candidates,
        } => CheckResult::info(
            name,
            format!(
                "daemon not registered with {manager} (optional) — looked for {}",
                candidates.join(", ")
            ),
        )
        .with_category(category)
        .with_hint("run: rantaiclaw service install"),
        Registration::Unsupported => {
            CheckResult::info(name, "init system not detected (skipped)").with_category(category)
        }
        #[cfg(any(test, debug_assertions))]
        Registration::DevBuild => {
            CheckResult::info(name, DEV_BUILD_SKIP_MESSAGE).with_category(category)
        }
    }
}

/// Fold the answers about the candidate units into one verdict.
///
/// Pure: the spawns happen in [`probe_systemd`]; this decides what their
/// answers mean. A manager that did not answer ends the search — the other
/// candidates cannot be asked either, and the installation's registration
/// status is unknown, not absent.
fn registration_from_probes(
    manager: &'static str,
    candidates: &[String],
    probe: impl Fn(&str) -> ProbeOutcome,
    expected: Option<&Path>,
) -> Registration {
    for unit in candidates {
        match probe(unit) {
            ProbeOutcome::Absent => {}
            ProbeOutcome::Present(report) => return classify_unit(manager, &report, expected),
            ProbeOutcome::Indeterminate => {
                return Registration::ManagerUnreachable {
                    manager,
                    unit: unit.clone(),
                };
            }
        }
    }
    Registration::NotRegistered {
        manager,
        candidates: candidates.to_vec(),
    }
}

/// Ask the host service manager about this installation's daemon unit.
///
/// A debug or test build answers [`Registration::DevBuild`] without spawning
/// anything: that guard is what keeps a developer's build and the test suite
/// away from the operator's systemd.
pub(crate) fn detect_registration_detailed(profile: &str) -> Registration {
    #[cfg(any(test, debug_assertions))]
    {
        if is_dev_build() {
            return Registration::DevBuild;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if which::which("systemctl").is_ok() {
            let expected = std::env::current_exe().ok();
            let candidates = candidate_units(profile);
            return registration_from_probes(
                "systemd (user)",
                &candidates,
                probe_systemd,
                expected.as_deref(),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        if which::which("launchctl").is_ok() {
            let unit = LAUNCHD_LABEL.to_string();
            let candidates = vec![unit.clone()];
            let outcome = match std::process::Command::new("launchctl").arg("list").output() {
                Ok(out) if out.status.success() => {
                    let listed = String::from_utf8_lossy(&out.stdout);
                    if listed.contains(&unit) {
                        // launchd reports no executable, so the binary cannot
                        // be compared here — the unit is named, the binary is
                        // not verified. `launchctl list` naming the label is
                        // what `rantaiclaw service status` reads as loaded.
                        ProbeOutcome::Present(UnitReport {
                            unit: unit.clone(),
                            active: true,
                            exec_start: None,
                            fragment_path: None,
                        })
                    } else {
                        ProbeOutcome::Absent
                    }
                }
                // launchctl is installed but did not answer.
                _ => ProbeOutcome::Indeterminate,
            };
            return registration_from_probes("launchd", &candidates, |_| outcome.clone(), None);
        }
    }
    Registration::Unsupported
}

/// The launchd label the installer writes (`SERVICE_LABEL` in
/// `src/service/mod.rs`).
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "com.rantaiclaw.daemon";

/// What one `systemctl --user status` exit code says about the unit.
///
/// systemd's exit codes for `status`: `0` = the unit is running, `3` = the
/// unit is loaded but not running, `4` = no such unit. `1` is the generic
/// failure — including "Failed to connect to bus", which is what a host
/// without a reachable user manager answers — and `2` is invalid usage.
/// Neither `1` nor `2` establishes presence, and a probe that was killed
/// reports no exit code at all, so all three are
/// [`ProbeOutcome::Indeterminate`].
#[cfg(target_os = "linux")]
fn classify_systemctl(code: Option<i32>, unit: &str) -> ProbeOutcome {
    match code {
        Some(0) => ProbeOutcome::Present(UnitReport {
            unit: unit.to_string(),
            active: true,
            exec_start: None,
            fragment_path: None,
        }),
        Some(3) => ProbeOutcome::Present(UnitReport {
            unit: unit.to_string(),
            active: false,
            exec_start: None,
            fragment_path: None,
        }),
        Some(4) => ProbeOutcome::Absent,
        _ => ProbeOutcome::Indeterminate,
    }
}

/// Ask systemd about one unit: whether it exists, whether it is active, and —
/// when it exists — which binary it runs.
///
/// Two queries, because they answer two questions: `status` says whether the
/// unit exists and is running, `show` says what it runs. A manager that does
/// not answer yields [`ProbeOutcome::Indeterminate`] and the second query is
/// not made: a unit that was never found has no `ExecStart` to read.
///
/// The dev-build guard sits here, at the spawn, so this helper is safe on its
/// own rather than only through its caller. A refused probe proves nothing
/// about the unit, so it is [`ProbeOutcome::Indeterminate`] — the caller reads
/// that as "registration status unknown", not as "not registered".
#[cfg(target_os = "linux")]
fn probe_systemd(unit: &str) -> ProbeOutcome {
    if is_dev_build() {
        return ProbeOutcome::Indeterminate;
    }
    let outcome = match std::process::Command::new("systemctl")
        .args(["--user", "status", unit])
        .output()
    {
        Ok(out) => classify_systemctl(out.status.code(), unit),
        // The manager could not be run at all: nothing can be proven.
        Err(_) => ProbeOutcome::Indeterminate,
    };
    match outcome {
        ProbeOutcome::Present(mut report) => {
            if let Ok(out) = std::process::Command::new("systemctl")
                .args([
                    "--user",
                    "show",
                    "--property=ExecStart",
                    "--property=FragmentPath",
                    unit,
                ])
                .output()
            {
                if out.status.success() {
                    let text = String::from_utf8_lossy(&out.stdout);
                    report.exec_start = property_value(&text, "ExecStart");
                    report.fragment_path = property_value(&text, "FragmentPath");
                }
            }
            ProbeOutcome::Present(report)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Severity;

    /// Exit 3 (installed but stopped) and exit 4 (not installed) are distinct
    /// states — verified against real synthetic units — and must not both map
    /// to NotRegistered, which hints at a re-install that is a no-op.
    ///
    /// The codes that carry no verdict are distinct too: 3 = loaded but
    /// inactive, 1 = the generic failure (including "Failed to connect to
    /// bus"), 2 = invalid usage, and no code at all = the probe was killed.
    /// None of the last three says the unit exists, and none of them says it
    /// does not.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemctl_exit_codes_map_to_distinct_states() {
        let unit = "rantaiclaw.service";
        let present = |active: bool| {
            ProbeOutcome::Present(UnitReport {
                unit: unit.to_string(),
                active,
                exec_start: None,
                fragment_path: None,
            })
        };
        assert_eq!(classify_systemctl(Some(0), unit), present(true), "active");
        assert_eq!(
            classify_systemctl(Some(3), unit),
            present(false),
            "installed but stopped"
        );
        assert_eq!(
            classify_systemctl(Some(4), unit),
            ProbeOutcome::Absent,
            "no such unit"
        );
        // A generic failure says nothing about the unit — not even that it is
        // installed.
        assert_eq!(
            classify_systemctl(Some(1), unit),
            ProbeOutcome::Indeterminate,
            "generic failure"
        );
        assert_eq!(
            classify_systemctl(Some(2), unit),
            ProbeOutcome::Indeterminate,
            "invalid usage"
        );
        // A signal-killed probe reports no code at all, and must not read as
        // 'installed' or as 'not installed'.
        assert_eq!(
            classify_systemctl(None, unit),
            ProbeOutcome::Indeterminate,
            "no exit code"
        );
    }

    #[tokio::test]
    async fn check_returns_a_category_of_system() {
        use crate::config::Config;
        use crate::profile::Profile;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let ctx = DoctorContext {
            profile: Profile {
                name: "test".into(),
                root: tmp.path().to_path_buf(),
            },
            config: Config::default(),
            offline: false,
        };
        let r = DaemonRegistrationCheck.run(&ctx).await;
        assert_eq!(r.category, "system");
    }

    /// A debug or test build must short-circuit the detection to a state
    /// distinct from `Unsupported` so the rendered line points at the build
    /// mode, not at the init-system detection. The fake on PATH answers
    /// `active` and would otherwise fold into `Registered` — the guard must
    /// prevent the spawn.
    #[test]
    fn detect_registration_dev_build_skips_the_service_manager() {
        use crate::service::is_dev_build;
        assert!(
            is_dev_build(),
            "this test only proves its point under a debug/test build"
        );
        let _lock = crate::test_env::ENV_LOCK.blocking_lock();
        let _fake = crate::test_env::FakeServiceManager::new();
        let registration = detect_registration_detailed("default");
        assert_eq!(registration, Registration::DevBuild);
    }

    /// The probe helper must be safe on its own, not only because its caller
    /// short-circuits first: a debug or test build must not spawn the service
    /// manager from `probe_systemd`, and a refused probe must not read as
    /// "absent" — the operator would be told to install a unit that may
    /// already be there. It proves nothing, so it is indeterminate.
    #[cfg(target_os = "linux")]
    #[test]
    fn probe_systemd_refuses_to_spawn_in_a_dev_build() {
        use crate::service::is_dev_build;
        assert!(
            is_dev_build(),
            "this test only proves its point under a debug/test build"
        );
        let _lock = crate::test_env::ENV_LOCK.blocking_lock();
        let fake = crate::test_env::FakeServiceManager::new();
        let outcome = probe_systemd("rantaiclaw.service");
        assert_eq!(
            outcome,
            ProbeOutcome::Indeterminate,
            "a refused probe proves nothing about the unit"
        );
        let log = fake.dir.path().join("systemctl.log");
        assert!(
            !log.exists(),
            "fake systemctl must not be reached; call log at {}",
            log.display()
        );
    }

    // ── The scheduler check ─────────────────────────────────────────

    /// A `DoctorContext` whose config path sits in a temp dir, so the state
    /// file the check reads is the one this test writes — never the
    /// operator's own.
    fn scheduler_ctx(tmp: &tempfile::TempDir) -> DoctorContext {
        use crate::config::Config;
        use crate::profile::Profile;

        let mut cfg = Config::default();
        cfg.config_path = tmp.path().join("config.toml");
        DoctorContext {
            profile: Profile {
                name: "test".into(),
                root: tmp.path().to_path_buf(),
            },
            config: cfg,
            offline: false,
        }
    }

    /// A state file body stamped the way the daemon's state writer stamps it.
    fn scheduler_state_body(status: &str, written_age_seconds: i64) -> String {
        serde_json::json!({
            "components": {"scheduler": {"status": status}},
            "written_at": (chrono::Utc::now()
                - chrono::Duration::seconds(written_age_seconds))
                .to_rfc3339(),
        })
        .to_string()
    }

    /// No state file: the daemon is not running, so the scheduler cannot be
    /// healthy. A warn, not a fail — a machine without a daemon is a normal
    /// install, and `doctor` must not fail there.
    #[tokio::test]
    async fn scheduler_check_reports_the_daemon_as_not_running_without_a_state_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = scheduler_ctx(&tmp);

        let result = SchedulerCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        assert!(
            result.message.contains("daemon not running"),
            "the line must say the daemon is not running: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn scheduler_check_reports_ok_for_a_healthy_scheduler() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = scheduler_ctx(&tmp);
        std::fs::write(
            tmp.path().join("daemon_state.json"),
            scheduler_state_body("ok", 0),
        )
        .unwrap();

        let result = SchedulerCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
    }

    /// A scheduler component in `error` is a failure the operator has to see,
    /// and `doctor` exits non-zero on a fail.
    #[tokio::test]
    async fn scheduler_check_fails_for_an_unhealthy_scheduler() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = scheduler_ctx(&tmp);
        std::fs::write(
            tmp.path().join("daemon_state.json"),
            scheduler_state_body("error", 0),
        )
        .unwrap();

        let result = SchedulerCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "msg: {}", result.message);
    }

    /// A state file the daemon has stopped refreshing belongs to a daemon that
    /// is gone. It must not read as a healthy scheduler.
    #[tokio::test]
    async fn scheduler_check_does_not_report_a_stale_state_file_as_healthy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = scheduler_ctx(&tmp);
        std::fs::write(
            tmp.path().join("daemon_state.json"),
            scheduler_state_body("ok", 60),
        )
        .unwrap();

        let result = SchedulerCheck.run(&ctx).await;
        assert_ne!(
            result.severity,
            Severity::Ok,
            "a stale state file must not read as healthy: {}",
            result.message
        );
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
    }

    #[tokio::test]
    async fn scheduler_check_does_not_report_an_unparseable_state_file_as_healthy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = scheduler_ctx(&tmp);
        std::fs::write(tmp.path().join("daemon_state.json"), "{ not json").unwrap();

        let result = SchedulerCheck.run(&ctx).await;
        assert_ne!(
            result.severity,
            Severity::Ok,
            "an unparseable state file must not read as healthy: {}",
            result.message
        );
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
    }

    /// The console runs doctor in brief mode, and brief mode skips every
    /// `live` check. The scheduler verdict comes from a local file, so the
    /// check must not be `live` — otherwise the console keeps the gap this
    /// check closes.
    #[tokio::test]
    async fn scheduler_check_is_not_a_live_check() {
        assert_ne!(SchedulerCheck.category(), "live");
    }

    // ── The registration check names what it looked at ──────────────

    /// A unit that runs a different binary belongs to another installation.
    /// Reporting it as this one's registration is the defect the check must
    /// not have.
    #[test]
    fn a_unit_that_runs_another_binary_is_not_reported_as_registered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp.path().join("bin/rantaiclaw");
        let other = tmp.path().join("other/rantaiclaw");
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&expected, b"").unwrap();
        std::fs::write(&other, b"").unwrap();

        let report = UnitReport {
            unit: "rantaiclaw.service".to_string(),
            active: true,
            exec_start: Some(format!(
                "{{ path={} ; argv[]={} daemon ; ignore_errors=no ; }}",
                other.display(),
                other.display()
            )),
            fragment_path: Some("/home/operator/.config/systemd/user/rantaiclaw.service".into()),
        };

        let registration = classify_unit("systemd (user)", &report, Some(&expected));
        assert!(
            matches!(registration, Registration::ForeignBinary { .. }),
            "another installation's unit must not read as registered: {registration:?}"
        );

        let result = render_registration("daemon.registration", "system", &registration);
        assert_ne!(
            result.severity,
            Severity::Ok,
            "the line must not read as registered: {}",
            result.message
        );
        assert!(
            result.message.contains("rantaiclaw.service"),
            "the line must name the unit it looked at: {}",
            result.message
        );
        assert!(
            result.message.contains(&other.display().to_string()),
            "the line must name the binary the unit runs: {}",
            result.message
        );
    }

    /// The same unit, running the binary that is running doctor, is this
    /// installation's registration — and the line says so.
    #[test]
    fn a_unit_that_runs_this_binary_is_reported_as_registered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp.path().join("bin/rantaiclaw");
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, b"").unwrap();

        let report = UnitReport {
            unit: "rantaiclaw.service".to_string(),
            active: true,
            exec_start: Some(format!(
                "{{ path={} ; argv[]={} daemon ; ignore_errors=no ; }}",
                expected.display(),
                expected.display()
            )),
            fragment_path: Some("/home/operator/.config/systemd/user/rantaiclaw.service".into()),
        };

        let registration = classify_unit("systemd (user)", &report, Some(&expected));
        assert!(
            matches!(registration, Registration::Registered { .. }),
            "this installation's unit must read as registered: {registration:?}"
        );

        let result = render_registration("daemon.registration", "system", &registration);
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(
            result.message.contains("rantaiclaw.service"),
            "the line must name the unit it looked at: {}",
            result.message
        );
        assert!(
            result.message.contains("systemd (user)"),
            "the line must name the manager it asked: {}",
            result.message
        );
        assert!(
            result.message.contains(&expected.display().to_string()),
            "the line must name the binary the unit runs: {}",
            result.message
        );
    }

    /// A unit whose binary cannot be verified — the manager named no
    /// executable, or this process's own path could not be resolved — is not
    /// a registration. The check learned that the unit exists, and nothing
    /// more; saying "daemon registered" would claim what it did not learn.
    #[test]
    fn a_unit_whose_binary_cannot_be_verified_is_not_reported_as_registered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp.path().join("bin/rantaiclaw");
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, b"").unwrap();

        // The manager named no executable at all.
        let report = UnitReport {
            unit: "rantaiclaw.service".to_string(),
            active: true,
            exec_start: None,
            fragment_path: Some("/home/operator/.config/systemd/user/rantaiclaw.service".into()),
        };

        let registration = classify_unit("systemd (user)", &report, Some(&expected));
        assert!(
            matches!(registration, Registration::Unverified { .. }),
            "an unverifiable binary is not a registration: {registration:?}"
        );

        let result = render_registration("daemon.registration", "system", &registration);
        assert_ne!(
            result.severity,
            Severity::Ok,
            "the line must not read as registered: {}",
            result.message
        );
        assert!(
            result.message.contains("could not be verified"),
            "the line must say the binary was not verified: {}",
            result.message
        );
        assert!(
            !result.message.contains("daemon registered"),
            "the line must not claim a registration: {}",
            result.message
        );
        assert_eq!(
            result.message,
            "unit rantaiclaw.service is registered with systemd (user) and is active, but the \
             binary it runs could not be verified — not confirmed as this installation"
        );
        assert_eq!(
            result.hint.as_deref(),
            Some("check the unit's ExecStart to see which binary it runs")
        );

        // The same unit, stopped rather than running: "not running" is a fact
        // the manager reported, but it is not a registration either.
        let report = UnitReport {
            unit: "rantaiclaw.service".to_string(),
            active: false,
            exec_start: None,
            fragment_path: None,
        };
        let registration = classify_unit("systemd (user)", &report, Some(&expected));
        assert!(
            matches!(registration, Registration::Unverified { .. }),
            "a stopped unit whose binary cannot be verified is not a registration: {registration:?}"
        );
        let result = render_registration("daemon.registration", "system", &registration);
        assert_ne!(
            result.severity,
            Severity::Ok,
            "the line must not read as registered: {}",
            result.message
        );
        assert_eq!(
            result.message,
            "unit rantaiclaw.service is registered with systemd (user) but is not running, and the \
             binary it runs could not be verified — not confirmed as this installation"
        );

        // This process's own path could not be resolved: the unit runs a
        // different binary, and nothing can be proven about it either way.
        let other = tmp.path().join("other/rantaiclaw");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"").unwrap();
        let report = UnitReport {
            unit: "rantaiclaw.service".to_string(),
            active: true,
            exec_start: Some(format!(
                "{{ path={} ; argv[]={} daemon ; ignore_errors=no ; }}",
                other.display(),
                other.display()
            )),
            fragment_path: None,
        };

        let registration = classify_unit("systemd (user)", &report, None);
        assert!(
            !matches!(registration, Registration::Registered { .. }),
            "an unverifiable binary must not read as registered: {registration:?}"
        );
    }

    /// A user service manager that does not answer — "Failed to connect to
    /// bus" on a host without a reachable user manager — establishes neither
    /// presence nor absence. The line must claim neither a registration nor
    /// its absence.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unreachable_user_manager_is_not_reported_as_registered() {
        let unit = "rantaiclaw.service";
        for code in [Some(1), Some(2), None] {
            let outcome = classify_systemctl(code, unit);
            assert_eq!(
                outcome,
                ProbeOutcome::Indeterminate,
                "exit code {code:?} says nothing about the unit"
            );

            let registration = registration_from_probes(
                "systemd (user)",
                &[unit.to_string()],
                |_| outcome.clone(),
                None,
            );
            assert!(
                matches!(registration, Registration::ManagerUnreachable { .. }),
                "an unanswered manager is not a registration: {registration:?}"
            );

            let result = render_registration("daemon.registration", "system", &registration);
            assert_ne!(
                result.severity,
                Severity::Ok,
                "the line must not read as registered: {}",
                result.message
            );
            assert!(
                !result.message.contains("daemon registered"),
                "the line must not claim a registration: {}",
                result.message
            );
            assert!(
                !result.message.contains("not registered"),
                "the line must not claim the unit is absent: {}",
                result.message
            );
            assert_eq!(
                result.message,
                "could not query systemd (user) for unit rantaiclaw.service — the user service \
                 manager did not answer; registration status unknown"
            );
        }
    }

    /// The unit names the installer creates, and the templated name the
    /// daemon's own convention uses. A host that follows either convention
    /// must be found.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_candidate_units_cover_the_installed_name_and_the_template() {
        let units = candidate_units("default");
        assert!(
            units.iter().any(|u| u == "rantaiclaw.service"),
            "the installed unit name must be asked for: {units:?}"
        );
        assert!(
            units.iter().any(|u| u == "rantaiclaw@default.service"),
            "the templated unit name must be asked for: {units:?}"
        );
    }

    /// A host with no unit of that name is not a failure, but the line still
    /// has to say which manager was asked and which units were looked for —
    /// otherwise "not registered" reads as "nothing is registered", which is
    /// a different claim.
    #[test]
    fn the_not_registered_line_names_the_manager_and_the_units_it_looked_for() {
        let candidates = vec![
            "rantaiclaw.service".to_string(),
            "rantaiclaw@default.service".to_string(),
        ];
        let registration = registration_from_probes(
            "systemd (user)",
            &candidates,
            |_| ProbeOutcome::Absent,
            None,
        );
        let result = render_registration("daemon.registration", "system", &registration);
        assert!(
            result.message.contains("systemd (user)"),
            "the line must name the manager it asked: {}",
            result.message
        );
        assert!(
            result.message.contains("rantaiclaw.service"),
            "the line must name the unit it looked for: {}",
            result.message
        );
    }

    /// `systemctl show` prints the parsed command; the executable is the
    /// `path=` field.
    #[test]
    fn the_exec_start_binary_is_read_from_the_show_output() {
        let shown = "{ path=/usr/local/bin/rantaiclaw ; argv[]=/usr/local/bin/rantaiclaw daemon ; \
                     ignore_errors=no ; start_time=[0] ; stop_time=[0] ; }";
        assert_eq!(exec_start_binary(shown), Some("/usr/local/bin/rantaiclaw"));
        // A single-word `argv[]` names the executable.
        let argv_only = "{ argv[]=/usr/local/bin/rantaiclaw ; ignore_errors=no ; }";
        assert_eq!(
            exec_start_binary(argv_only),
            Some("/usr/local/bin/rantaiclaw")
        );
        // An `argv[]` with arguments does not: whitespace alone cannot tell
        // the executable from its arguments, and a path that contains a space
        // would be split into a binary that does not exist.
        let argv_with_args = "{ argv[]=/usr/local/bin/rantaiclaw daemon ; ignore_errors=no ; }";
        assert_eq!(exec_start_binary(argv_with_args), None);
        let path_with_space = "{ path=/opt/my dir/rantaiclaw ; ignore_errors=no ; }";
        assert_eq!(
            exec_start_binary(path_with_space),
            Some("/opt/my dir/rantaiclaw")
        );
        assert_eq!(exec_start_binary(""), None);
    }
}
