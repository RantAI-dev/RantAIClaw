//! WhatsApp Web provisioner — implements [`TuiProvisioner`] for in-TUI QR pairing.
//!
//! Mirrors the legacy dialoguer flow in `src/onboard/section/channels.rs`
//! for WhatsApp Web: prompt for session path + optional pair phone, run
//! the QR/pair-code handshake, prompt for allowed numbers on success,
//! then write `config.channels_config.whatsapp` and save.

use super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, ProvisionResponse, ProvisionerCategory,
    Severity, TuiProvisioner,
};
use crate::channels::whatsapp_web::{pair_once, PairEvent, PairOptions};
use crate::config::schema::WhatsAppWebConfig;
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

    async fn run(
        &self,
        config: &mut Config,
        profile: &Profile,
        io: ProvisionIo,
    ) -> Result<ProvisionOutcome> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        // The Web table from the running config, if there is one. F-9 used
        // to read only `pair_code` from it on a re-run; the session path and
        // the allowlist defaulted to the workspace and `[]`, so a re-run
        // pointed the channel at a fresh, unlinked session file and wiped the
        // allowlist to deny-all. Carry the whole Web table forward and use its
        // values as the prompt defaults.
        let existing = config.channels_config.whatsapp_web.clone();

        // ── 1. Prompt for session DB path ──────────────────────────
        // Default under the active profile's workspace so each profile links
        // its own WhatsApp device instead of sharing one global session store.
        // On a re-run, keep the path the channel already talks to — otherwise
        // the new link goes to a fresh, unlinked file.
        let default_session: PathBuf = existing
            .as_ref()
            .filter(|c| !c.session_path.is_empty())
            .map(|c| PathBuf::from(&c.session_path))
            .unwrap_or_else(|| profile.workspace_dir().join("whatsapp.db"));
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
                return Ok(ProvisionOutcome::Aborted("Cancelled.".into()));
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
                return Ok(ProvisionOutcome::Aborted("Cancelled.".into()));
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

        // The window is `PairOptions::new`'s, the one the console uses too.
        let opts = PairOptions {
            pair_phone: pair_phone.clone(),
            ..PairOptions::new(session_path.clone())
        };
        let timed_out = format!(
            "Pairing timed out ({}s). Try again.",
            opts.timeout.as_secs()
        );
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
                            error: timed_out.clone(),
                        })
                        .await
                        .ok();
                    return Ok(ProvisionOutcome::Aborted(timed_out));
                }
                PairEvent::Failed(e) => {
                    events
                        .send(ProvisionEvent::Failed {
                            error: format!("Pairing failed: {e}"),
                        })
                        .await
                        .ok();
                    return Ok(ProvisionOutcome::Aborted(format!("Pairing failed: {e}")));
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
            return Ok(ProvisionOutcome::Aborted(
                "Pair stream ended without Connected event.".into(),
            ));
        }

        // ── 4. Prompt for allowed numbers ──────────────────────────
        // F-11: the runtime compares `allowed_numbers` against the `+E.164`
        // form every sender is rewritten to. The provisioner used to store
        // whatever the operator typed — `0812…`, `62812…`, `+62 877-9800-…`
        // all save and never match. Run every entry through the helper the
        // gateway already uses; refuse one that would never match with the
        // helper's own sentence. On a re-run, pre-fill with the entries the
        // config already holds so Enter keeps them byte-identical.
        let existing_default = existing
            .as_ref()
            .filter(|c| !c.allowed_numbers.is_empty())
            .map(|c| c.allowed_numbers.join(","));
        events
            .send(ProvisionEvent::Prompt {
                id: "allowed_numbers".into(),
                label: "Allowed numbers (comma-separated E.164, e.g. +6281234567890, or * for any)"
                    .into(),
                default: existing_default,
                secret: false,
            })
            .await
            .ok();
        let raw_entries: Vec<String> = match responses.recv().await {
            Some(ProvisionResponse::Text(s)) => s
                .split(',')
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .collect(),
            _ => Vec::new(),
        };
        // Validate every entry through the same helper the gateway uses to
        // save operator edits (`config_api.rs`), so the runtime gets nothing
        // it cannot match. A bad entry fails the provisioner with the
        // helper's own sentence — the render loop surfaces it in both the
        // TUI overlay and headless stderr.
        let mut allowed_numbers: Vec<String> = Vec::with_capacity(raw_entries.len());
        for entry in &raw_entries {
            match WhatsAppWebConfig::allowlist_entry(entry) {
                Ok(normalised) => allowed_numbers.push(normalised),
                Err(sentence) => {
                    events
                        .send(ProvisionEvent::Failed {
                            error: sentence.clone(),
                        })
                        .await
                        .ok();
                    return Ok(ProvisionOutcome::Aborted(sentence));
                }
            }
        }
        crate::onboard::provision::validate::allowlist::warn_on_reach(
            &events,
            &allowed_numbers,
            "Allowed numbers",
        )
        .await?;

        // ── 5. Write config and save ───────────────────────────────
        // Its own table since schema v32. The Cloud table is not read and not
        // written here: setting up Web mode must not disturb Cloud keys, and
        // it no longer has to reach across to preserve them.
        config.channels_config.whatsapp_web = Some(WhatsAppWebConfig {
            session_path: session_path.to_string_lossy().into_owned(),
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

                // Apply the new channel config to the running daemon so the bot
                // answers immediately — no manual restart needed. Quiet (no
                // stdout) so it is safe inside the TUI overlay; the result is
                // surfaced via the provision event stream.
                //
                // `apply_channel_config` shells out `systemctl daemon-reload` +
                // `restart` synchronously (blocks up to TimeoutStopSec=30). This
                // runs on the TUI's async worker, so wrap it in `spawn_blocking`
                // — otherwise the whole runtime (every other in-flight task)
                // freezes for the duration of the restart.
                let cfg = config.clone();
                let (severity, text) = match tokio::task::spawn_blocking(move || {
                    crate::service::apply_channel_config(&cfg, crate::service::InitSystem::Auto)
                })
                .await
                {
                    Ok(Ok(msg)) => (Severity::Success, format!("↳ {msg}")),
                    Ok(Err(e)) => (
                        Severity::Warn,
                        format!(
                            "config saved, but auto-apply failed: {e}. Run `rantaiclaw service restart`."
                        ),
                    ),
                    Err(e) => (
                        Severity::Warn,
                        format!(
                            "config saved, but auto-apply thread panicked: {e}. Run `rantaiclaw service restart`."
                        ),
                    ),
                };
                events
                    .send(ProvisionEvent::Message { severity, text })
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
                return Ok(ProvisionOutcome::Aborted(format!(
                    "Failed to save config: {e}"
                )));
            }
        }

        events
            .send(ProvisionEvent::Done {
                summary: "WhatsApp Web setup complete.".into(),
            })
            .await
            .ok();
        Ok(ProvisionOutcome::Configured)
    }
}

#[cfg(test)]
mod tests {
    /// F-9, F-11. Every `allowed_numbers` entry the provisioner is about to
    /// save has to pass through `WhatsAppWebConfig::allowlist_entry`. The
    /// helper then writes the entry in the form the runtime compares
    /// (`+E.164`), so `6281…` becomes `+6281…` and `0812…` is refused
    /// outright. A provisioner that bypassed the helper would silently
    /// re-introduce F-9 / F-11, so a source guard binds the call site.
    #[test]
    fn allowed_numbers_run_through_allowlist_entry() {
        let production = production_half(include_str!("whatsapp_web.rs"));
        let body =
            fn_body(production, "async fn run(").expect("`run` not found in production code");
        assert!(
            body.contains("WhatsAppWebConfig::allowlist_entry("),
            "the whatsapp web provisioner no longer runs every entry through \
             `WhatsAppWebConfig::allowlist_entry` — a typed entry will be saved \
             in a form the runtime cannot match"
        );
    }

    fn production_half(src: &str) -> &str {
        let cut = ["\n#[cfg(test)]\nmod "]
            .iter()
            .flat_map(|marker| src.match_indices(marker))
            .map(|(at, _)| at)
            .min()
            .unwrap_or(src.len());
        &src[..cut]
    }

    fn fn_body<'a>(production: &'a str, header: &str) -> Option<&'a str> {
        let after = production.split(header).nth(1)?;
        let end = [
            "\n    async fn ",
            "\n    fn ",
            "\n    pub fn ",
            "\n    pub async fn ",
            "\n    pub(crate) fn ",
            "\n    pub(crate) async fn ",
        ]
        .iter()
        .filter_map(|marker| after.find(marker))
        .min()
        .unwrap_or(after.len());
        Some(&after[..end])
    }
}
