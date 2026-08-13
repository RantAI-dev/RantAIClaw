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
#[cfg(test)]
pub mod test_support;
pub mod traits;
pub mod validate;
#[cfg(feature = "whatsapp-web")]
pub mod whatsapp_web;
#[allow(unused_imports)]
pub use registry::{available, provisioner_for};
#[allow(unused_imports)]
pub use traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, ProvisionResponse, ProvisionerCategory,
    Severity, TuiProvisioner,
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

/// Everything a channel provisioner's caller must do once it reports
/// [`ProvisionOutcome::Configured`]: install the core skill, and hand back the
/// owner-claim guidance if the operator still needs it.
///
/// The guidance is returned rather than emitted because the two drivers render
/// it differently — the TUI overlay takes a [`ProvisionEvent::Message`], the
/// headless driver writes to stderr — and by the time this runs, `run` has
/// already consumed the event sender it was given. Keeping the *policy* here
/// and the rendering at each driver is what makes both paths say the same
/// thing.
pub fn finalize_channel(
    category: ProvisionerCategory,
    profile: &crate::profile::Profile,
    config: &crate::config::Config,
) -> Option<String> {
    install_core_skills_after_channel(category, profile);
    owner_claim_guidance(category, config)
}

/// The note a freshly configured channel needs when nobody can approve on it
/// yet, or `None` when an owner is already set.
///
/// An empty `approval_owners` is the correct default — it is what keeps a
/// gated tool from being approvable by a stranger who found the bot. But the
/// TUI provisioning path, the one the module doc calls "the one users take",
/// never said so. A channel came up with every gated tool auto-denying and no
/// explanation, which reads as broken rather than as secure.
///
/// This deliberately does **not** seed an owner. Only the explanation was
/// missing.
pub fn owner_claim_guidance(
    category: ProvisionerCategory,
    config: &crate::config::Config,
) -> Option<String> {
    if category != ProvisionerCategory::Channel {
        return None;
    }
    if !config.channels_config.approval_owners.is_empty() {
        return None;
    }
    Some(
        "No approval owner is set, so any tool needing approval will be \
         auto-denied over this channel.\n\
         Claim ownership from chat (captures your real id):\n  \
         1. Start the channel runtime: `rantaiclaw channels` — with an empty \
         allowed_users it prints a one-time pairing code.\n  \
         2. DM your bot: `/claim <code>` — registers you as an approval owner.\n\
         Or set it by hand: [channels_config] approval_owners = [\"<your id>\"]"
            .to_string(),
    )
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

    /// A TUI-provisioned channel used to come up with every gated tool
    /// auto-denying and no explanation, because this path never printed the
    /// claim guidance the CLI section path does. That reads as broken rather
    /// than as the secure default it is.
    #[test]
    fn owner_guidance_is_emitted_when_approval_owners_is_empty() {
        let config = crate::config::Config::default();
        assert!(config.channels_config.approval_owners.is_empty());

        let guidance = owner_claim_guidance(ProvisionerCategory::Channel, &config)
            .expect("an unowned channel must be explained");
        assert!(guidance.contains("/claim"), "{guidance}");
        assert!(guidance.contains("approval_owners"), "{guidance}");
    }

    /// The corollary: once an owner exists there is nothing to explain, and
    /// this must never seed one itself.
    #[test]
    fn owner_guidance_is_silent_once_an_owner_exists() {
        let mut config = crate::config::Config::default();
        config
            .channels_config
            .approval_owners
            .push("rantaiclaw_operator".to_string());

        assert!(owner_claim_guidance(ProvisionerCategory::Channel, &config).is_none());
        assert_eq!(
            config.channels_config.approval_owners,
            vec!["rantaiclaw_operator".to_string()],
            "reading the guidance must not mutate the owner list"
        );
    }

    #[test]
    fn owner_guidance_is_only_for_channels() {
        let config = crate::config::Config::default();
        for category in [
            ProvisionerCategory::Core,
            ProvisionerCategory::Integration,
            ProvisionerCategory::Runtime,
            ProvisionerCategory::Hardware,
            ProvisionerCategory::Routing,
        ] {
            assert!(
                owner_claim_guidance(category, &config).is_none(),
                "{category:?} has no channel for anyone to own"
            );
        }
    }
}
