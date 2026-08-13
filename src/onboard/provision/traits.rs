//! TuiProvisioner trait and async event/response types for TUI-driven setup.
//!
//! Each provisioner implements [`TuiProvisioner`] and communicates with the
//! driver (TUI overlay or headless CLI) via [`ProvisionIo`] channels:
//! - It emits [`ProvisionEvent`]s that the driver renders.
//! - It awaits [`ProvisionResponse`]s for prompts and selections.

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionerCategory {
    Core,
    Channel,
    Integration,
    Runtime,
    Hardware,
    Routing,
}

#[derive(Debug, Clone)]
pub enum ProvisionEvent {
    Message {
        severity: Severity,
        text: String,
    },
    QrCode {
        payload: String,
        caption: String,
    },
    Prompt {
        id: String,
        label: String,
        default: Option<String>,
        secret: bool,
    },
    Choose {
        id: String,
        label: String,
        options: Vec<String>,
        multi: bool,
    },
    Done {
        summary: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum Severity {
    Info,
    Warn,
    Error,
    Success,
}

/// What a provisioner actually did, as distinct from whether it crashed.
///
/// `run` used to return `Result<()>`, so a provisioner that emitted
/// `ProvisionEvent::Failed` for a missing required field and then bailed with
/// `Ok(())` was indistinguishable from one that configured a channel. Both
/// drivers read that `Ok` as success and went on to install the core skill and
/// save the config — the exact false "channel is set up" signal that
/// `install_core_skills_after_channel`'s own doc says the ordering prevents.
///
/// A deliberate user skip is not an error, which is why this is a variant of
/// the success type rather than an `Err`: the TUI's overlay-freeze protection
/// treats `Err` as exceptional and surfaces it as a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The provisioner completed and its config mutations should be persisted.
    Configured,
    /// The provisioner stopped early — a missing required field, a rejected
    /// credential, or a user skip. Nothing should be persisted or installed.
    Aborted(String),
}

impl ProvisionOutcome {
    /// Convenience for the common `Aborted` construction from a `&str`.
    pub fn aborted(reason: impl Into<String>) -> Self {
        Self::Aborted(reason.into())
    }

    pub fn is_configured(&self) -> bool {
        matches!(self, Self::Configured)
    }
}

#[derive(Debug, Clone)]
pub enum ProvisionResponse {
    Text(String),
    Selection(Vec<usize>),
    Cancelled,
}

pub struct ProvisionIo {
    pub events: mpsc::Sender<ProvisionEvent>,
    pub responses: mpsc::Receiver<ProvisionResponse>,
}

#[async_trait]
pub trait TuiProvisioner: Send {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Core
    }
    async fn run(
        &self,
        config: &mut crate::config::Config,
        profile: &crate::profile::Profile,
        io: ProvisionIo,
    ) -> Result<ProvisionOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_variants_are_distinct() {
        assert_ne!(ProvisionerCategory::Core, ProvisionerCategory::Channel);
        assert_ne!(
            ProvisionerCategory::Channel,
            ProvisionerCategory::Integration
        );
        assert_ne!(
            ProvisionerCategory::Integration,
            ProvisionerCategory::Runtime
        );
        assert_ne!(ProvisionerCategory::Runtime, ProvisionerCategory::Hardware);
        assert_ne!(ProvisionerCategory::Hardware, ProvisionerCategory::Routing);
    }

    #[test]
    fn default_category_is_core() {
        struct DummyProvisioner;
        #[async_trait]
        impl TuiProvisioner for DummyProvisioner {
            fn name(&self) -> &'static str {
                "dummy"
            }
            fn description(&self) -> &'static str {
                "dummy"
            }
            async fn run(
                &self,
                _: &mut crate::config::Config,
                _: &crate::profile::Profile,
                _: ProvisionIo,
            ) -> Result<ProvisionOutcome> {
                Ok(ProvisionOutcome::Configured)
            }
        }
        assert_eq!(DummyProvisioner.category(), ProvisionerCategory::Core);
    }

    #[test]
    fn provision_event_message_carries_severity() {
        let info = ProvisionEvent::Message {
            severity: Severity::Info,
            text: "starting".into(),
        };
        match info {
            ProvisionEvent::Message {
                severity: Severity::Info,
                ..
            } => {}
            _ => panic!("expected Info Message"),
        }
    }

    #[test]
    fn provision_response_text_round_trips() {
        let r = ProvisionResponse::Text("hello".into());
        assert!(matches!(r, ProvisionResponse::Text(ref s) if s == "hello"));
    }
}
