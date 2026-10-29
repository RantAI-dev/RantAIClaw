//! The CLI door for a persona timezone refuses a name this build cannot use.
//!
//! The HTTP door is covered by a unit test in `src/gateway/api_v1.rs`; this
//! file drives the built binary, so the exit code the operator sees is
//! covered too.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rantaiclaw"))
}

/// Run `rantaiclaw personality …` against a sandbox home and return
/// `(exit code, stdout, stderr)`.
fn run_personality(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(binary_path())
        .arg("personality")
        .args(args)
        // The profile directory is derived from HOME, so a sandbox home keeps
        // the run away from the operator's own tree.
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        // An override inherited from the parent shell would shadow the
        // default resolution under the sandbox home.
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE")
        .env_remove("RANTAICLAW_PROFILE")
        .env("RUST_LOG", "warn")
        .output()
        .expect("failed to spawn the rantaiclaw binary");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The persona file of the active (default) profile inside a sandbox home.
fn persona_file(home: &Path) -> PathBuf {
    home.join(".rantaiclaw")
        .join("profiles")
        .join("default")
        .join("persona")
        .join("persona.toml")
}

/// A timezone this build cannot use is refused: the command fails, it names
/// the bad value, and the persona file is not written.
#[test]
fn personality_set_refuses_an_unknown_timezone() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    let (code, stdout, stderr) = run_personality(home, &["set", "--timezone", "Not/AZone"]);

    assert_ne!(code, 0, "the command must fail: {stdout}{stderr}");
    let said = format!("{stdout}{stderr}");
    assert!(
        said.contains("Not/AZone"),
        "the failure must name the bad value, got: {said}"
    );
    assert!(
        !persona_file(home).exists(),
        "the refused command must not write a persona file"
    );
}

/// An IANA name is accepted and stored.
#[test]
fn personality_set_stores_an_iana_timezone() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    let (code, stdout, stderr) = run_personality(home, &["set", "--timezone", "Asia/Jakarta"]);

    assert_eq!(code, 0, "the command must succeed: {stdout}{stderr}");
    let body = std::fs::read_to_string(persona_file(home)).expect("the persona file");
    assert!(
        body.contains("Asia/Jakarta"),
        "the timezone must be stored, got: {body}"
    );
}

/// The empty value means "unset" and stays allowed.
#[test]
fn personality_set_accepts_an_empty_timezone() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    let (code, stdout, stderr) = run_personality(home, &["set", "--timezone", ""]);

    assert_eq!(code, 0, "the command must succeed: {stdout}{stderr}");
    let body = std::fs::read_to_string(persona_file(home)).expect("the persona file");
    assert!(
        body.contains("timezone = \"\""),
        "the timezone must be stored as unset, got: {body}"
    );
}

/// The preset stays the positional argument.
#[test]
fn personality_set_still_takes_the_preset_positionally() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    let (code, stdout, stderr) = run_personality(home, &["set", "concise_pro"]);

    assert_eq!(code, 0, "the command must succeed: {stdout}{stderr}");
    let body = std::fs::read_to_string(persona_file(home)).expect("the persona file");
    assert!(
        body.contains("concise_pro"),
        "the preset must be stored, got: {body}"
    );
}

/// The name is settable next to the preset.
#[test]
fn personality_set_stores_a_name() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();

    let (code, stdout, stderr) = run_personality(home, &["set", "concise_pro", "--name", "Ranti"]);

    assert_eq!(code, 0, "the command must succeed: {stdout}{stderr}");
    let body = std::fs::read_to_string(persona_file(home)).expect("the persona file");
    assert!(
        body.contains("Ranti"),
        "the name must be stored, got: {body}"
    );
}
