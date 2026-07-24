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
    ) -> Result<()>;
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
            ) -> Result<()> {
                Ok(())
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
