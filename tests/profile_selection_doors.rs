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

    /// Remove the environment a run must not inherit.
    ///
    /// The door variables would move the config away from the sandbox, and
    /// the credentials and the provider/model variables are folded onto the
    /// config at load: an inherited `API_KEY` or a `PROVIDER` would change
    /// what the run reads, so every run starts from a clean slate.
    fn clear_inherited_env(command: &mut Command) {
        for name in [
            "RANTAICLAW_CONFIG_DIR",
            "RANTAICLAW_WORKSPACE",
            "RANTAICLAW_PROFILE",
            "RANTAICLAW_API_KEY",
            "API_KEY",
            "RANTAICLAW_PROVIDER",
            "PROVIDER",
            "RANTAICLAW_MODEL",
            "MODEL",
        ] {
            command.env_remove(name);
        }
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
            .env("RUST_LOG", "warn");
        Self::clear_inherited_env(&mut command);
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
        self.run_with_deadline_and_env(args, profile, &[])
    }

    /// Run the binary in the sandbox with extra environment variables and
    /// wait for it with a deadline.
    ///
    /// The extra variables are set AFTER the inherited ones are removed, so
    /// a run can name exactly the environment it exercises.
    fn run_with_deadline_and_env(
        &self,
        args: &[&str],
        profile: &str,
        env: &[(&str, &str)],
    ) -> (i32, String) {
        let log = self.home.path().join("run.log");
        let stdout = std::fs::File::create(&log).expect("the log file");
        let stderr = stdout.try_clone().expect("the log file clone");
        let mut command = Command::new(binary_path());
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("xdg"))
            .env("RUST_LOG", "warn")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        Self::clear_inherited_env(&mut command);
        for (name, value) in env {
            command.env(name, value);
        }
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

/// The flag door refuses a profile that names nothing, and the daemon path
/// exits EX_CONFIG (78) there too: the same refusal, the same exit code.
#[test]
fn the_daemon_path_exits_ex_config_when_the_profile_flag_refuses() {
    let sandbox = Sandbox::new();

    let (code, said) = sandbox.run_with_deadline(&["--profile", "ghost", "daemon"], "");

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("ghost"),
        "the refusal must name the profile: {said}"
    );
}

/// A daemon whose active-profile marker names a profile directory that is
/// gone cannot load its config. The marker is a fact on disk, so the same
/// load fails the same way on every restart: the daemon exits EX_CONFIG
/// (78) and the unit stays stopped instead of being restarted every few
/// seconds.
#[test]
fn the_daemon_path_exits_ex_config_when_the_config_cannot_be_loaded() {
    let sandbox = Sandbox::new();
    // The marker names a profile that does not exist. No flag and no
    // environment name a profile, so the doors do not run and the failure
    // surfaces at the config load.
    std::fs::create_dir_all(sandbox.home.path().join(".rantaiclaw")).expect("the config root");
    std::fs::write(
        sandbox.home.path().join(".rantaiclaw/active_profile"),
        "ghost\n",
    )
    .expect("the marker names a profile that does not exist");
    assert!(
        !sandbox.profile_dir("ghost").exists(),
        "premise: the profile the marker names does not exist"
    );

    let (code, said) = sandbox.run_with_deadline(&["daemon"], "");

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("ghost"),
        "the refusal must name the profile: {said}"
    );
}

/// Every other command keeps exit 1 for the same broken config: only the
/// daemon is supervised by a unit that restarts it.
#[test]
fn a_non_daemon_command_keeps_exit_one_when_the_config_cannot_be_loaded() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.path().join(".rantaiclaw")).expect("the config root");
    std::fs::write(
        sandbox.home.path().join(".rantaiclaw/active_profile"),
        "ghost\n",
    )
    .expect("the marker names a profile that does not exist");

    let (code, said) = sandbox.run(&["status"], "", false);

    assert_eq!(code, 1, "every other command keeps exit 1: {said}");
    assert!(
        said.contains("ghost"),
        "the refusal must name the profile: {said}"
    );
}

/// A config that is valid TOML but does not fit the schema is a fact on
/// disk too: the same file fails the same way on every start, so the daemon
/// exits EX_CONFIG (78) and the unit stays stopped instead of being
/// restarted every few seconds.
#[test]
fn the_daemon_path_exits_ex_config_when_the_config_does_not_fit_the_schema() {
    let sandbox = Sandbox::new();
    let profile_dir = sandbox.profile_dir("default");
    std::fs::create_dir_all(&profile_dir).expect("the profile dir");
    std::fs::write(
        profile_dir.join("config.toml"),
        // The current schema version, so the failure is the shape of the
        // file and not a migration: `default_temperature` is a number, and
        // a string does not fit it.
        "schema_version = 40\ndefault_temperature = \"hot\"\n",
    )
    .expect("the config does not fit the schema");

    let (code, said) = sandbox.run_with_deadline(&["daemon"], "");

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("default_temperature"),
        "the refusal must name the field that does not fit: {said}"
    );
}

/// A config that parses and deserialises but holds a value the schema
/// refuses is a fact on disk too: the same file fails the same way on every
/// start, so the daemon exits EX_CONFIG (78) and the unit stays stopped
/// instead of being restarted every few seconds.
#[test]
fn the_daemon_path_exits_ex_config_when_the_config_holds_an_invalid_value() {
    let sandbox = Sandbox::new();
    let profile_dir = sandbox.profile_dir("default");
    std::fs::create_dir_all(&profile_dir).expect("the profile dir");
    std::fs::write(
        profile_dir.join("config.toml"),
        // The current schema version, so the failure is the value and not a
        // migration: `default_temperature` must be between 0.0 and 2.0, and
        // 9.0 is a number the schema refuses.
        "schema_version = 40\ndefault_temperature = 9.0\n",
    )
    .expect("the config holds a value that does not fit");

    let (code, said) = sandbox.run_with_deadline(&["daemon"], "");

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("default_temperature"),
        "the refusal must name the field that does not fit: {said}"
    );
}

/// A ciphertext the key on disk cannot decrypt is a fact on disk too: the
/// same value and the same key fail the same way on every start, so the
/// daemon exits EX_CONFIG (78) and the unit stays stopped instead of being
/// restarted every few seconds.
#[test]
fn the_daemon_path_exits_ex_config_when_the_config_holds_an_undecryptable_secret() {
    let sandbox = Sandbox::new();
    let profile_dir = sandbox.profile_dir("default");
    std::fs::create_dir_all(&profile_dir).expect("the profile dir");
    std::fs::write(
        profile_dir.join("config.toml"),
        // The current schema version, so the failure is the secret and not a
        // migration. `enc2:` is the encrypted prefix, and the blob behind it
        // is too short to hold a nonce, so the decrypt fails before any key
        // is read.
        "schema_version = 40\napi_key = \"enc2:00\"\n",
    )
    .expect("the config holds a secret that cannot be decrypted");

    let (code, said) = sandbox.run_with_deadline(&["daemon"], "");

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("api_key"),
        "the refusal must name the field that cannot be decrypted: {said}"
    );
}

/// Every other command keeps exit 1 for a config that holds a value the
/// schema refuses: only the daemon is supervised by a unit that restarts it.
#[test]
fn a_non_daemon_command_keeps_exit_one_when_the_config_holds_an_invalid_value() {
    let sandbox = Sandbox::new();
    let profile_dir = sandbox.profile_dir("default");
    std::fs::create_dir_all(&profile_dir).expect("the profile dir");
    std::fs::write(
        profile_dir.join("config.toml"),
        "schema_version = 40\ndefault_temperature = 9.0\n",
    )
    .expect("the config holds a value that does not fit");

    let (code, said) = sandbox.run(&["status"], "", false);

    assert_eq!(code, 1, "every other command keeps exit 1: {said}");
    assert!(
        said.contains("default_temperature"),
        "the refusal must name the field that does not fit: {said}"
    );
}

/// Every other command keeps exit 1 for a config that holds a secret the key
/// cannot decrypt: only the daemon is supervised by a unit that restarts it.
#[test]
fn a_non_daemon_command_keeps_exit_one_when_the_config_holds_an_undecryptable_secret() {
    let sandbox = Sandbox::new();
    let profile_dir = sandbox.profile_dir("default");
    std::fs::create_dir_all(&profile_dir).expect("the profile dir");
    std::fs::write(
        profile_dir.join("config.toml"),
        "schema_version = 40\napi_key = \"enc2:00\"\n",
    )
    .expect("the config holds a secret that cannot be decrypted");

    let (code, said) = sandbox.run(&["status"], "", false);

    assert_eq!(code, 1, "every other command keeps exit 1: {said}");
    assert!(
        said.contains("api_key"),
        "the refusal must name the field that cannot be decrypted: {said}"
    );
}

/// A config that does not exist yet is written from the defaults, and the
/// environment can fold a value onto it that the schema refuses. The
/// environment belongs to the unit, so the same failure repeats on every
/// restart: the daemon exits EX_CONFIG (78) and the unit stays stopped
/// instead of being restarted every few seconds.
#[test]
fn the_daemon_path_exits_ex_config_when_the_environment_holds_an_invalid_value() {
    let sandbox = Sandbox::new();
    // No config file exists, so the daemon writes one from the defaults and
    // then folds the environment onto it: a ':cloud' model needs a remote
    // endpoint, and the default `api_url` is unset.
    let (code, said) = sandbox.run_with_deadline_and_env(
        &["daemon"],
        "",
        &[
            ("RANTAICLAW_PROVIDER", "ollama"),
            ("RANTAICLAW_MODEL", "llama3:cloud"),
        ],
    );

    assert_eq!(
        code, 78,
        "the daemon path must exit EX_CONFIG, so the unit stops: {said}"
    );
    assert!(
        said.contains("default_model"),
        "the refusal must name the field that does not fit: {said}"
    );
}
