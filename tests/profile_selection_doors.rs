//! The doors that switch or select a profile.
//!
//! A profile named by `--profile` or by `RANTAICLAW_PROFILE` passes one door:
//! an override that pins the config directory refuses a profile it does not
//! keep in effect (the run would read the config from one profile and the
//! per-profile stores from another), and a name that names nothing is refused
//! before anything is created. The profile commands stay available under an
//! override — they are the way out of a selection that cannot be used.
//!
//! The unit tests in `src/profile/mod.rs` cover the rules themselves; this
//! file drives the built binary, so the exit code and the message the
//! operator sees are covered too. A daemon refused at a door exits 78
//! (`EX_CONFIG`), the code the installed unit's `RestartPreventExitStatus=78`
//! stops on; every other command exits 1.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rantaiclaw"))
}

/// A sandbox: a home, a config home inside it, and a scratch config
/// directory that stands in for an override.
struct Sandbox {
    home: TempDir,
    config_dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let home = TempDir::new().expect("temp home");
        let config_dir = TempDir::new().expect("temp config dir");
        std::fs::create_dir_all(home.path().join("xdg")).expect("the config home");
        Self { home, config_dir }
    }

    /// The profile directory of `name` inside the sandbox home.
    fn profile_dir(&self, name: &str) -> PathBuf {
        self.home
            .path()
            .join(".rantaiclaw")
            .join("profiles")
            .join(name)
    }

    /// Run the binary in the sandbox and return its exit code and output.
    ///
    /// `profile` is the `RANTAICLAW_PROFILE` value (empty: unset).
    /// `under_override` puts the scratch config directory in effect.
    fn run(&self, args: &[&str], profile: &str, under_override: bool) -> (i32, String) {
        let mut command = Command::new(binary_path());
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("xdg"))
            // An override inherited from the parent shell would change what
            // these runs exercise.
            .env_remove("RANTAICLAW_CONFIG_DIR")
            .env_remove("RANTAICLAW_WORKSPACE")
            .env_remove("RANTAICLAW_PROFILE")
            .env("RUST_LOG", "warn");
        if under_override {
            command.env("RANTAICLAW_CONFIG_DIR", self.config_dir.path());
        }
        if !profile.is_empty() {
            command.env("RANTAICLAW_PROFILE", profile);
        }
        let output = command.output().expect("the binary runs");
        (
            output.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )
    }

    /// Run the binary in the sandbox and wait for it with a deadline.
    ///
    /// A daemon that starts instead of being refused keeps running, so the
    /// wait has a deadline and the child is killed when it passes. The
    /// output goes to a file, not a pipe: a daemon that logs more than a
    /// pipe buffer holds would block on a full pipe instead of exiting.
    fn run_with_deadline(&self, args: &[&str], profile: &str) -> (i32, String) {
        let log = self.home.path().join("run.log");
        let stdout = std::fs::File::create(&log).expect("the log file");
        let stderr = stdout.try_clone().expect("the log file clone");
        let mut command = Command::new(binary_path());
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("xdg"))
            .env_remove("RANTAICLAW_CONFIG_DIR")
            .env_remove("RANTAICLAW_WORKSPACE")
            .env_remove("RANTAICLAW_PROFILE")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if !profile.is_empty() {
            command.env("RANTAICLAW_PROFILE", profile);
        }
        let mut child = command.spawn().expect("the binary starts");

        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            match child.try_wait().expect("the child is waited for") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "the daemon did not exit within 60 seconds: {}",
                        std::fs::read_to_string(&log).unwrap_or_default()
                    );
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };

        (
            status.code().unwrap_or(-1),
            std::fs::read_to_string(&log).unwrap_or_default(),
        )
    }
}

/// A profile selected by the flag while an override keeps another profile in
/// effect is refused, with the override and the remedy in the message.
///
/// The profile exists, so the override is the only thing that can refuse
/// it: a name that names nothing would be refused for a different reason.
#[test]
fn the_flag_door_refuses_a_profile_the_override_does_not_keep_in_effect() {
    let sandbox = Sandbox::new();
    let (code, said) = sandbox.run(&["profile", "create", "work"], "", false);
    assert_eq!(code, 0, "the profile is created: {said}");

    let (code, said) = sandbox.run(&["--profile", "work", "session", "list"], "", true);

    assert_ne!(code, 0, "the selection must be refused: {said}");
    assert!(
        said.contains("RANTAICLAW_CONFIG_DIR"),
        "the refusal must name the override: {said}"
    );
    assert!(
        said.contains("unset RANTAICLAW_CONFIG_DIR"),
        "the refusal must say how to clear the override: {said}"
    );
    assert!(
        said.contains("work"),
        "the refusal must name the profile: {said}"
    );
}

/// The environment door gives the same one sentence: `RANTAICLAW_PROFILE`
/// names the profile for this invocation exactly as `--profile` does.
///
/// The profile exists, so the override is the only thing that can refuse
/// it: a name that names nothing would be refused for a different reason.
#[test]
fn the_environment_door_refuses_a_profile_the_override_does_not_keep_in_effect() {
    let sandbox = Sandbox::new();
    let (code, said) = sandbox.run(&["profile", "create", "work"], "", false);
    assert_eq!(code, 0, "the profile is created: {said}");

    let (code, said) = sandbox.run(&["session", "list"], "work", true);

    assert_ne!(code, 0, "the selection must be refused: {said}");
    assert!(
        said.contains("RANTAICLAW_CONFIG_DIR"),
        "the refusal must name the override: {said}"
    );
    assert!(
        said.contains("work"),
        "the refusal must name the profile: {said}"
    );
}

/// A switch under an override is refused as well: it would move the
/// per-profile stores and leave the config behind.
#[test]
fn the_switch_door_refuses_a_profile_the_override_does_not_keep_in_effect() {
    let sandbox = Sandbox::new();
    let (code, said) = sandbox.run(&["profile", "create", "work"], "", false);
    assert_eq!(code, 0, "the profile is created: {said}");

    let (code, said) = sandbox.run(&["profile", "use", "work"], "", true);

    assert_ne!(code, 0, "the switch must be refused: {said}");
    assert!(
        said.contains("RANTAICLAW_CONFIG_DIR"),
        "the refusal must name the override: {said}"
    );
    assert!(
        !sandbox
            .home
            .path()
            .join(".rantaiclaw/active_profile")
            .exists(),
        "a refused switch must not move the marker"
    );
}

/// The profile commands stay available under an override: they are the way
/// out of a selection that cannot be used. The override must not be what
/// refuses them.
#[test]
fn the_profile_commands_stay_available_under_an_override() {
    let sandbox = Sandbox::new();
    let (code, said) = sandbox.run(&["profile", "create", "work"], "", false);
    assert_eq!(code, 0, "the profile is created: {said}");

    let (code, said) = sandbox.run(&["profile", "list"], "work", true);
    assert_eq!(code, 0, "the profile commands must work: {said}");
    assert!(
        !said.contains("RANTAICLAW_CONFIG_DIR"),
        "the override must not refuse the profile commands: {said}"
    );

    let (code, said) = sandbox.run(&["profile", "current"], "work", true);
    assert_eq!(code, 0, "the profile commands must work: {said}");
    assert!(
        said.contains("work"),
        "the current profile is the one the environment names: {said}"
    );
    assert!(
        !said.contains("RANTAICLAW_CONFIG_DIR"),
        "the override must not refuse the profile commands: {said}"
    );
}

/// A profile that does not exist is refused where the name enters, and
/// nothing is created for it.
#[test]
fn a_name_that_names_nothing_is_refused_at_the_door() {
    let sandbox = Sandbox::new();

    let (code, said) = sandbox.run(&["session", "list"], "ghost", false);
    assert_ne!(code, 0, "the name must be refused: {said}");
    assert!(
        said.contains("ghost") && said.contains("profile create ghost"),
        "the refusal must name the profile and the remedy: {said}"
    );
    assert!(
        !sandbox.profile_dir("ghost").exists(),
        "a refused name must not create the directory"
    );

    let (code, said) = sandbox.run(&["--profile", "ghost", "session", "list"], "", false);
    assert_ne!(code, 0, "the name must be refused: {said}");
    assert!(
        said.contains("ghost") && said.contains("profile create ghost"),
        "the refusal must name the profile and the remedy: {said}"
    );
    assert!(
        !sandbox.profile_dir("ghost").exists(),
        "a refused name must not create the directory"
    );

    // A command whose config resolution has a flat fallback that predates
    // the door would go through without it, so the door is what refuses
    // this run.
    let (code, said) = sandbox.run(&["memory", "list"], "ghost", false);
    assert_ne!(code, 0, "the name must be refused: {said}");
    assert!(
        said.contains("ghost") && said.contains("profile create ghost"),
        "the refusal must name the profile and the remedy: {said}"
    );
    assert!(
        !sandbox.profile_dir("ghost").exists(),
        "a refused name must not create the directory"
    );
}

/// A daemon refused at a profile door exits EX_CONFIG (78): the installed
/// unit carries `RestartPreventExitStatus=78`, so it stops instead of
/// being restarted every few seconds, and the journal keeps the sentence
/// that says why.
#[test]
fn the_daemon_path_exits_ex_config_when_the_profile_door_refuses() {
    let sandbox = Sandbox::new();
    let (code, said) = sandbox.run(&["profile", "create", "work"], "", false);
    assert_eq!(code, 0, "the profile is created: {said}");

    let config_dir = sandbox
        .config_dir
        .path()
        .to_str()
        .expect("the config dir path");
    let (code, said) = sandbox.run_with_deadline(&["--config-dir", config_dir, "daemon"], "work");

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("RANTAICLAW_CONFIG_DIR"),
        "the refusal must name the override: {said}"
    );
    assert!(
        said.contains("work"),
        "the refusal must name the profile: {said}"
    );
}

/// Every other command keeps exit 1 at the profile door: only the daemon
/// is supervised by a unit that restarts it.
#[test]
fn a_non_daemon_command_does_not_exit_ex_config_at_the_profile_door() {
    let sandbox = Sandbox::new();
    let (code, said) = sandbox.run(&["profile", "create", "work"], "", false);
    assert_eq!(code, 0, "the profile is created: {said}");

    let (code, said) = sandbox.run(&["session", "list"], "work", true);

    assert_ne!(code, 0, "the selection must be refused: {said}");
    assert_ne!(code, 78, "only the daemon path exits EX_CONFIG: {said}");
}
