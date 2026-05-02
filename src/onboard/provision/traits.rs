//! TuiProvisioner trait and async event/response types for TUI-driven setup.
//!
//! Each provisioner implements [`TuiProvisioner`] and communicates with the
//! driver (TUI overlay or headless CLI) via [`ProvisionIo`] channels:
//! - It emits [`ProvisionEvent`]s that the driver renders.
//! - It awaits [`ProvisionResponse`]s for prompts and selections.

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Driver-facing events emitted by a provisioner.
#[derive(Debug, Clone)]
pub enum ProvisionEvent {
    /// Plain status line.
    Message { severity: Severity, text: String },
    /// Render a QR code. `payload` is the raw string to encode.
    QrCode { payload: String, caption: String },
    /// Prompt the user for a text value.
    Prompt {
        id: String,
        label: String,
        default: Option<String>,
        secret: bool,
    },
    /// Multi-select list.
    Choose {
        id: String,
        label: String,
        options: Vec<String>,
        multi: bool,
    },
    /// Provisioner finished successfully.
    Done { summary: String },
    /// Provisioner failed.
    Failed { error: String },
}

#[derive(Debug, Clone, Copy)]
pub enum Severity {
    Info,
    Warn,
    Error,
    Success,
}

/// Responses sent back to a provisioner.
#[derive(Debug, Clone)]
pub enum ProvisionResponse {
    Text(String),
    Selection(Vec<usize>),
    Cancelled,
}

/// Channels handed to a provisioner. It emits events on `events` and
/// awaits responses on `responses`.
pub struct ProvisionIo {
    pub events: mpsc::Sender<ProvisionEvent>,
    pub responses: mpsc::Receiver<ProvisionResponse>,
}

#[async_trait]
pub trait TuiProvisioner: Send {
    /// Stable kebab-case identifier — used for `rantaiclaw setup <name>`.
    fn name(&self) -> &'static str;
    /// One-line description for the picker.
    fn description(&self) -> &'static str;
    /// Run to completion. Mutates `config` on success; caller persists.
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
