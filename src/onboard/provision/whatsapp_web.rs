//! WhatsApp Web provisioner — implements [`TuiProvisioner`] for in-TUI QR pairing.
//!
//! Mirrors the legacy dialoguer flow in `src/onboard/section/channels.rs`
//! for WhatsApp Web: prompt for session path + optional pair phone, run
//! the QR/pair-code handshake, prompt for allowed numbers on success,
//! then write `config.channels_config.whatsapp` and save.

use super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, ProvisionerCategory, Severity, TuiProvisioner,
};
use crate::channels::whatsapp_web::{pair_once, PairEvent, PairOptions};
use crate::config::schema::WhatsAppConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use std::path::PathBuf;

pub const WHATSAPP_WEB_NAME: &str = "whatsapp-web";
pub const WHATSAPP_WEB_DESC: &str =
    "WhatsApp Web — link your phone to receive messages in RantaiClaw";

#[derive(Debug, Clone, Default)]
pub struct WhatsAppWebProvisioner {
    pub phone: Option<String>,
}

impl WhatsAppWebProvisioner {
    pub fn new(phone: Option<String>) -> Self {
        Self { phone }
    }
}

#[async_trait]
impl TuiProvisioner for WhatsAppWebProvisioner {
    fn name(&self) -> &'static str {
        WHATSAPP_WEB_NAME
    }

    fn description(&self) -> &'static str {
        WHATSAPP_WEB_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Channel
    }

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        // ── 1. Prompt for session DB path ──────────────────────────
        let default_session: PathBuf = directories::ProjectDirs::from("", "", "rantaiclaw")
            .map(|d| d.data_dir().join("whatsapp.db"))
            .unwrap_or_else(|| PathBuf::from("whatsapp.db"));
        events
            .send(ProvisionEvent::Prompt {
                id: "session_path".into(),
                label: "Session DB path".into(),
                default: Some(default_session.display().to_string()),
                secret: false,
            })
            .await
            .ok();
        let session_path = match responses.recv().await {
            Some(ProvisionResponse::Text(s)) if !s.is_empty() => PathBuf::from(s),
            Some(ProvisionResponse::Text(_)) => default_session.clone(),
            _ => {
                events
                    .send(ProvisionEvent::Failed {
                        error: "Cancelled.".into(),
                    })
                    .await
                    .ok();
                return Ok(());
            }
        };

        // ── 2. Prompt for optional pair-code phone ─────────────────
        events
            .send(ProvisionEvent::Prompt {
                id: "pair_phone".into(),
                label: "Phone for pair-code linking (blank = QR only)".into(),
                default: self.phone.clone(),
                secret: false,
            })
            .await
            .ok();
        let pair_phone: Option<String> = match responses.recv().await {
            Some(ProvisionResponse::Text(s)) if !s.is_empty() => Some(s),
            Some(ProvisionResponse::Text(_)) => None,
            _ => {
                events
                    .send(ProvisionEvent::Failed {
                        error: "Cancelled.".into(),
                    })
                    .await
                    .ok();
                return Ok(());
            }
        };

        // ── 3. Run pair_once and forward events ────────────────────
        events
            .send(ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Starting WhatsApp Web pairing…".into(),
            })
            .await
            .ok();

        let opts = PairOptions {
            session_path: session_path.clone(),
            pair_phone: pair_phone.clone(),
            timeout: std::time::Duration::from_secs(120),
        };
        let mut stream = pair_once(opts);
        let mut paired = false;
        while let Some(ev) = stream.next().await {
            match ev {
                PairEvent::Qr(code) => {
                    events
                        .send(ProvisionEvent::QrCode {
                            payload: code,
                            caption: "Scan with WhatsApp > Linked Devices > Link a Device".into(),
                        })
                        .await
                        .ok();
                }
                PairEvent::PairCode(code) => {
                    events
                        .send(ProvisionEvent::Message {
                            severity: Severity::Info,
                            text: format!("Pair code: {code}  (enter on your phone)"),
                        })
                        .await
                        .ok();
                }
                PairEvent::Connected => {
                    events
                        .send(ProvisionEvent::Message {
                            severity: Severity::Success,
                            text: "Linked successfully!".into(),
                        })
                        .await
                        .ok();
                    paired = true;
                    break;
                }
                PairEvent::Timeout => {
                    events
                        .send(ProvisionEvent::Failed {
                            error: "Pairing timed out (120s). Try again.".into(),
                        })
                        .await
                        .ok();
                    return Ok(());
                }
                PairEvent::Failed(e) => {
                    events
                        .send(ProvisionEvent::Failed {
                            error: format!("Pairing failed: {e}"),
                        })
                        .await
                        .ok();
                    return Ok(());
                }
            }
        }
        if !paired {
            events
                .send(ProvisionEvent::Failed {
                    error: "Pair stream ended without Connected event.".into(),
                })
                .await
                .ok();
            return Ok(());
        }

        // ── 4. Prompt for allowed numbers ──────────────────────────
        events
            .send(ProvisionEvent::Prompt {
                id: "allowed_numbers".into(),
                label: "Allowed numbers (comma-separated E.164, or * for any)".into(),
                default: Some("*".into()),
                secret: false,
            })
            .await
            .ok();
        let allowed_numbers: Vec<String> = match responses.recv().await {
            Some(ProvisionResponse::Text(s)) if !s.trim().is_empty() => s
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
            _ => vec!["*".to_string()],
        };

        // ── 5. Write config and save ───────────────────────────────
        let existing = config.channels_config.whatsapp.clone();
        config.channels_config.whatsapp = Some(WhatsAppConfig {
            access_token: existing.as_ref().and_then(|c| c.access_token.clone()),
            phone_number_id: existing.as_ref().and_then(|c| c.phone_number_id.clone()),
            verify_token: existing.as_ref().and_then(|c| c.verify_token.clone()),
            app_secret: existing.as_ref().and_then(|c| c.app_secret.clone()),
            session_path: Some(session_path.to_string_lossy().into_owned()),
            pair_phone: pair_phone.clone(),
            pair_code: existing.as_ref().and_then(|c| c.pair_code.clone()),
            allowed_numbers: allowed_numbers.clone(),
        });

        match config.save().await {
            Ok(_) => {
                events
                    .send(ProvisionEvent::Message {
                        severity: Severity::Success,
                        text: format!(
                            "Config saved: session={}, allowed={} entries",
                            session_path.display(),
                            allowed_numbers.len()
                        ),
                    })
                    .await
                    .ok();
            }
            Err(e) => {
                events
                    .send(ProvisionEvent::Failed {
                        error: format!("Failed to save config: {e}"),
                    })
                    .await
                    .ok();
                return Ok(());
            }
        }

        events
            .send(ProvisionEvent::Done {
                summary: "WhatsApp Web setup complete.".into(),
            })
            .await
            .ok();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_whatsapp_web() {
        let p = WhatsAppWebProvisioner::default();
        assert_eq!(p.name(), "whatsapp-web");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = WhatsAppWebProvisioner::default();
        assert!(!p.description().is_empty());
    }
}
