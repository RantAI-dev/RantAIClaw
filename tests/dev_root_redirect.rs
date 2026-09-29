//! A debug build never creates anything under a real-looking home.
//!
//! The library unit tests see the crate's `cfg(test)` guards. An integration
//! test links the library as a plain debug build, so it is the honest check that
//! the isolation holds for `cargo run` and for a binary a test spawns.

use std::path::PathBuf;

use rantaiclaw::profile::{paths, ProfileManager};
use rantaiclaw::Config;
use tokio::sync::Mutex;

/// Both tests mutate process-global env vars, and a test binary runs its tests
/// on separate threads.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// A home that is deliberately outside the OS temp dir: a scratch spot in this
/// crate's `target/`, standing in for the operator's real home.
fn home_outside_the_temp_dir() -> PathBuf {
    let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "rantaiclaw_dev_root_redirect_{}",
            std::process::id()
        ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("scratch home");
    assert!(
        !home.canonicalize().expect("canonical home").starts_with(
            std::env::temp_dir()
                .canonicalize()
                .expect("canonical temp dir")
        ),
        "test setup bug: a target dir under the OS temp dir would defeat this test"
    );
    home
}

#[test]
fn the_active_profile_creates_nothing_under_a_home_outside_the_temp_dir() {
    let _env = ENV_LOCK.blocking_lock();
    let home = home_outside_the_temp_dir();
    let prev_home = std::env::var_os("HOME");
    let prev_allow = std::env::var_os("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    std::env::set_var("HOME", &home);
    std::env::remove_var("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");
    std::env::remove_var("RANTAICLAW_PROFILE");

    let profile = ProfileManager::active();
    let redirected_root = paths::rantaiclaw_root();

    for (key, prev) in [
        ("HOME", prev_home),
        ("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", prev_allow),
        ("RANTAICLAW_PROFILE", prev_profile),
    ] {
        match prev {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    let profile = profile.expect("the active profile resolves");
    let created_under_home = std::fs::read_dir(&home).expect("read scratch home").count();
    let profile_is_in_the_temp_dir = profile
        .root
        .canonicalize()
        .expect("profile dir exists")
        .starts_with(
            std::env::temp_dir()
                .canonicalize()
                .expect("canonical temp dir"),
        );

    let _ = std::fs::remove_dir_all(&home);
    if let Some(dev_home) = redirected_root.parent() {
        let _ = std::fs::remove_dir_all(dev_home);
    }

    assert_eq!(
        created_under_home, 0,
        "ProfileManager::active() wrote under the real-looking home"
    );
    assert!(
        profile_is_in_the_temp_dir,
        "the profile must live in the temp dir, got {:?}",
        profile.root
    );
}

/// `Config::save` refuses a path outside the temp dir in a plain debug build,
/// not only in the crate's own unit-test binary. This is the one config guard
/// an integration test can observe: the resolver and legacy-layout guards sit
/// behind the root redirect, which already moves their paths.
#[tokio::test]
async fn save_refuses_a_config_path_outside_the_temp_dir() {
    let _env = ENV_LOCK.lock().await;
    let prev_allow = std::env::var_os("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");
    std::env::remove_var("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR");

    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("rantaiclaw_save_guard_{}", std::process::id()));
    let config_path = scratch.join("config.toml");
    let config = Config {
        config_path: config_path.clone(),
        ..Config::default()
    };

    let result = config.save().await;

    match prev_allow {
        Some(value) => std::env::set_var("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR", value),
        None => std::env::remove_var("RANTAICLAW_TEST_ALLOW_REAL_CONFIG_DIR"),
    }
    let created = scratch.exists();
    let _ = std::fs::remove_dir_all(&scratch);

    let err = result.expect_err("a config_path outside the temp dir must be refused");
    assert!(
        format!("{err:#}").contains("test config isolation"),
        "expected the write-site isolation guard message, got: {err:#}"
    );
    assert!(!created, "the guard must refuse before creating anything");
}
