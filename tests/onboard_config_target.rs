//! `rantaiclaw onboard` writes the config file the next command reads.
//!
//! Before the fix, quick setup built `$HOME/.rantaiclaw/config.toml` itself and
//! ignored `RANTAICLAW_CONFIG_DIR`, so the answers were written to a path the
//! loader never read for that environment.

use std::sync::Mutex;

use assert_cmd::Command;
use tempfile::TempDir;

/// The binary reads and writes a shared home; serialize the runs.
static CMD_LOCK: Mutex<()> = Mutex::new(());

fn onboard_cmd(home: &TempDir, config_dir: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("rantaiclaw").expect("cargo build rantaiclaw");
    c.env("HOME", home.path())
        .env("RANTAICLAW_CONFIG_DIR", config_dir)
        .env_remove("RANTAICLAW_PROFILE")
        .env_remove("RANTAICLAW_HOME")
        .env_remove("RANTAICLAW_WORKSPACE");
    c
}

#[test]
fn onboard_with_config_dir_override_writes_that_dir_and_not_the_flat_home_path() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let config_dir = TempDir::new().expect("tempdir");

    onboard_cmd(&home, config_dir.path())
        .args([
            "onboard",
            "--force",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-fake-target-0001",
            "--model",
            "fake-model",
        ])
        .assert()
        .success();

    let target = config_dir.path().join("config.toml");
    let body = std::fs::read_to_string(&target)
        .unwrap_or_else(|e| panic!("onboard must write {}: {e}", target.display()));
    assert!(
        body.contains("default_provider = \"openrouter\""),
        "the provider must be in the file the loader reads:\n{body}"
    );
    assert!(
        body.contains("default_model = \"fake-model\""),
        "the model must be in the file the loader reads:\n{body}"
    );
    assert!(
        !home.path().join(".rantaiclaw/config.toml").exists(),
        "onboard must not write the flat HOME path when RANTAICLAW_CONFIG_DIR is set"
    );
}

#[test]
fn onboard_without_config_dir_override_writes_the_active_profile_the_loader_reads() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    let mut c = Command::cargo_bin("rantaiclaw").expect("cargo build rantaiclaw");
    c.env("HOME", home.path())
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_PROFILE")
        .env_remove("RANTAICLAW_HOME")
        .env_remove("RANTAICLAW_WORKSPACE")
        .args([
            "onboard",
            "--force",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-fake-target-0002",
            "--model",
            "fake-model",
        ])
        .assert()
        .success();

    let target = home.path().join(".rantaiclaw/profiles/default/config.toml");
    let body = std::fs::read_to_string(&target)
        .unwrap_or_else(|e| panic!("onboard must write {}: {e}", target.display()));
    assert!(
        body.contains("default_model = \"fake-model\""),
        "the model must be in the active profile's config:\n{body}"
    );
    assert!(
        !home.path().join(".rantaiclaw/config.toml").exists(),
        "onboard must not write the flat HOME path"
    );
}

/// A v0.4 flat `$HOME/.rantaiclaw/config.toml` is still an input: onboard moves
/// it into the active profile and refuses to overwrite it without `--force`.
#[test]
fn onboard_refuses_to_overwrite_a_v04_flat_config_without_force() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let flat_dir = home.path().join(".rantaiclaw");
    std::fs::create_dir_all(&flat_dir).expect("flat dir");
    std::fs::write(
        flat_dir.join("config.toml"),
        "default_provider = \"openrouter\"\n",
    )
    .expect("flat config");

    let mut c = Command::cargo_bin("rantaiclaw").expect("cargo build rantaiclaw");
    let assert = c
        .env("HOME", home.path())
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_PROFILE")
        .env_remove("RANTAICLAW_HOME")
        .env_remove("RANTAICLAW_WORKSPACE")
        .args([
            "onboard",
            "--provider",
            "openrouter",
            "--api-key",
            "sk-fake-target-0003",
            "--model",
            "fake-model",
        ])
        .assert()
        .failure();

    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Refusing to overwrite existing config"),
        "the migrated v0.4 config must be seen by the overwrite guard; got:\n{combined}"
    );
    assert!(
        home.path()
            .join(".rantaiclaw/profiles/default/config.toml")
            .exists(),
        "the flat config must have been migrated into the active profile"
    );
}
