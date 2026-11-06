//! The `auth` command is gone.
//!
//! RantaiClaw no longer stores provider subscription logins, so there is
//! nothing for `rantaiclaw auth` to manage. The command survives as a hidden
//! stub: `rantaiclaw auth login` must not print "unrecognized subcommand", it
//! must say what replaced the login and exit NON-ZERO — the operator asked for
//! a login, and no login happened.
//!
//! This file drives the built binary, so the exit code and the message the
//! operator sees are covered too.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rantaiclaw"))
}

/// A sandbox: a home and a config home inside it, so a run never touches the
/// operator's own install.
struct Sandbox {
    home: TempDir,
    config_home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = TempDir::new().expect("temp home");
        let config_home = home.path().join("xdg");
        std::fs::create_dir_all(&config_home).expect("the config home");
        Self { home, config_home }
    }

    /// Run the binary in the sandbox and return its exit code and output.
    fn run(&self, args: &[&str]) -> (i32, String) {
        let mut command = Command::new(binary_path());
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("RUST_LOG", "warn");
        // The door variables would move the config away from the sandbox, and
        // the credentials are folded onto the config at load.
        for name in [
            "RANTAICLAW_CONFIG_DIR",
            "RANTAICLAW_WORKSPACE",
            "RANTAICLAW_PROFILE",
            "RANTAICLAW_API_KEY",
            "API_KEY",
            "ANTHROPIC_API_KEY",
            "RANTAICLAW_PROVIDER",
            "PROVIDER",
            "RANTAICLAW_MODEL",
            "MODEL",
        ] {
            command.env_remove(name);
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
}

/// `rantaiclaw auth login` must reach the stub: the operator typed the old
/// command, and "unrecognized subcommand" would tell them nothing.
///
/// The exit code is non-zero — the login did not happen — and the message
/// names the API key that replaces it.
#[test]
fn the_auth_login_command_is_refused_with_the_replacement() {
    let sandbox = Sandbox::new();

    let (code, said) = sandbox.run(&["auth", "login"]);

    assert_ne!(code, 0, "the command must fail: {said}");
    assert!(
        !said.contains("unrecognized subcommand"),
        "the stub must answer, not clap: {said}"
    );
    assert!(
        said.contains("removed"),
        "the message must say the command is gone: {said}"
    );
    assert!(
        said.contains("ANTHROPIC_API_KEY"),
        "the message must name the API key that replaces the login: {said}"
    );
    assert!(
        said.contains("rantaiclaw setup provider"),
        "the message must name the setup command: {said}"
    );
}

/// The bare `auth` command reaches the same stub: the subcommand is what an
/// operator types most often, and it must fail the same way.
#[test]
fn the_bare_auth_command_is_refused_the_same_way() {
    let sandbox = Sandbox::new();

    let (code, said) = sandbox.run(&["auth"]);

    assert_ne!(code, 0, "the command must fail: {said}");
    assert!(said.contains("removed"), "the message must say it: {said}");
    assert!(
        said.contains("ANTHROPIC_API_KEY"),
        "the message must name the API key: {said}"
    );
}

/// The command is HIDDEN: `--help` must not advertise a feature that no
/// longer exists. The variant stays in the CLI (the command reference still
/// documents it), but the surface does not offer it.
#[test]
fn the_help_output_does_not_advertise_the_removed_command() {
    let sandbox = Sandbox::new();

    let (code, said) = sandbox.run(&["--help"]);

    assert_eq!(code, 0, "the help must print: {said}");
    assert!(
        !said.contains("\n  auth"),
        "the help must not list the removed command: {said}"
    );
}
