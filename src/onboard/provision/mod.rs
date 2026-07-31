pub mod approvals;
pub mod channels;
#[cfg(feature = "kb")]
pub mod knowledge;
pub mod login;
pub mod mcp;
pub mod persona;
pub mod provider;
pub mod registry;
pub mod runtime_surfaces;
pub mod skills;
pub mod smoke;
pub mod traits;
pub mod validate;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_web;
#[allow(unused_imports)]
pub use registry::{available, provisioner_for};
#[allow(unused_imports)]
pub use traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, ProvisionerCategory, Severity, TuiProvisioner,
};

/// After a channel provisioner succeeds, install the always-on core skill.
///
/// `owner-permissions` teaches the agent the owner/guest model behind
/// `manage_permissions` and `issue_pairing_code`. Both tools are registered
/// unconditionally, so without the skill they exist but the agent has no
/// manual for them — and a multi-user channel is exactly where that matters.
///
/// `section/channels.rs` has always done this on the headless section path,
/// with the same reasoning ("now that a multi-user channel exists, even if the
/// skills section was skipped"). None of the fifteen channel provisioners did,
/// so the TUI path — the one users take — missed it.
///
/// Lives here rather than inside each provisioner so there is one place to
/// change, and it runs *after* a successful `run` so a channel that failed to
/// configure does not leave the skill behind as a false signal.
///
/// Idempotent and non-fatal: a failure is logged, never propagated, because
/// the channel itself is already configured by this point.
pub fn install_core_skills_after_channel(
    category: ProvisionerCategory,
    profile: &crate::profile::Profile,
) {
    if category != ProvisionerCategory::Channel {
        return;
    }
    match crate::skills::bundled::install_core_skills(profile) {
        Ok(installed) if !installed.is_empty() => {
            tracing::info!(
                skills = installed.join(", "),
                "installed core skill(s) after channel setup"
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("could not install the owner-permissions skill: {e}");
        }
    }
}

#[cfg(test)]
mod core_skill_hook_tests {
    use super::*;
    use crate::profile::Profile;

    /// `Profile::skills_dir()` resolves through the global home on every call
    /// and ignores `Profile::root`, so `HOME` is the only lever that isolates
    /// this — and it must outlive the assertions.
    struct HomeGuard(Option<std::ffi::OsString>);

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", path);
            Self(prev)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(prev) => std::env::set_var("HOME", prev),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn scratch_profile(tmp: &std::path::Path) -> Profile {
        Profile {
            name: "channel-core-skill-test".to_string(),
            root: tmp.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn a_configured_channel_gets_the_owner_permissions_skill() {
        // The gap this closes: all fifteen channel provisioners are reachable
        // from the TUI, none installed this, and a multi-user channel is
        // exactly where managing permissions from chat matters.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = tempfile::tempdir().expect("temp home");
        let _home = HomeGuard::set(tmp.path());
        let profile = scratch_profile(tmp.path());

        install_core_skills_after_channel(ProvisionerCategory::Channel, &profile);

        let core = crate::skills::bundled::CORE_PACK[0].slug;
        assert!(
            profile.skills_dir().join(core).join("SKILL.md").exists(),
            "`{core}` must be installed after a channel provisioner succeeds"
        );
    }

    #[tokio::test]
    async fn non_channel_categories_are_left_alone() {
        // Guards the assertion above from passing for the wrong reason: if the
        // helper ignored its category it would install for everything, and the
        // check above would prove nothing about channels.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let core = crate::skills::bundled::CORE_PACK[0].slug;

        for category in [
            ProvisionerCategory::Core,
            ProvisionerCategory::Integration,
            ProvisionerCategory::Runtime,
            ProvisionerCategory::Hardware,
            ProvisionerCategory::Routing,
        ] {
            let tmp = tempfile::tempdir().expect("temp home");
            let _home = HomeGuard::set(tmp.path());
            let profile = scratch_profile(tmp.path());

            install_core_skills_after_channel(category, &profile);

            assert!(
                !profile.skills_dir().join(core).exists(),
                "{category:?} must not trigger a core-skill install"
            );
        }
    }
}
