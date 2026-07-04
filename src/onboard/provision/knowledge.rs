#![cfg(feature = "kb")]
//! Knowledge Base provisioner — implements [`TuiProvisioner`] for in-TUI KB setup.
//!
//! Configures the Knowledge Base credentials the agent uses to search ingested
//! documents:
//!   1. Enable / skip
//!   2. Embedding API key (reuse main provider key / enter / skip)
//!   3. Optional OCR/vision key (reuse embedding key / enter / skip)
//!
//! Mirrors [`super::persona`]. The provisioner only mutates
//! `config.knowledge.*`; the driver persists the config afterward.

use super::traits::{ProvisionEvent, ProvisionIo, Severity, TuiProvisioner};
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const KNOWLEDGE_NAME: &str = "knowledge";
pub const KNOWLEDGE_DESC: &str =
    "Knowledge Base — document search (embedding) + optional OCR/vision";

#[derive(Debug, Clone)]
pub struct KnowledgeProvisioner;

impl KnowledgeProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for KnowledgeProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for KnowledgeProvisioner {
    fn name(&self) -> &'static str {
        KNOWLEDGE_NAME
    }

    fn description(&self) -> &'static str {
        KNOWLEDGE_DESC
    }

    async fn run(&self, config: &mut Config, _profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's set up the Knowledge Base.".into(),
            },
        )
        .await?;

        // Step 1 — enable / skip
        send(
            &events,
            ProvisionEvent::Choose {
                id: "enable".into(),
                label: "Enable Knowledge Base? (agent can search documents you ingest)".into(),
                options: vec!["Enable".into(), "Skip (leave disabled)".into()],
                multi: false,
            },
        )
        .await?;

        let selection = recv_selection(&mut responses).await?;
        if selection.first().copied().unwrap_or(0) == 1 {
            send(
                &events,
                ProvisionEvent::Done {
                    summary: "Knowledge Base left disabled.".into(),
                },
            )
            .await?;
            return Ok(());
        }

        // Step 2 — embedding key
        let main_reusable = config.default_provider.as_deref() == Some("openrouter")
            && config
                .api_key
                .as_deref()
                .map(|k| !k.is_empty())
                .unwrap_or(false);

        let embedding_key: Option<String> = if main_reusable {
            send(
                &events,
                ProvisionEvent::Choose {
                    id: "emb_src".into(),
                    label: "Embedding API key source".into(),
                    options: vec![
                        "Use the main provider key".into(),
                        "Enter a key".into(),
                        "Skip (leave disabled)".into(),
                    ],
                    multi: false,
                },
            )
            .await?;
            let sel = recv_selection(&mut responses).await?;
            match sel.first().copied().unwrap_or(0) {
                0 => config.api_key.clone(),
                1 => prompt_embedding_key(&events, &mut responses).await?,
                _ => None,
            }
        } else {
            send(
                &events,
                ProvisionEvent::Choose {
                    id: "emb_src".into(),
                    label: "Embedding API key source".into(),
                    options: vec!["Enter a key".into(), "Skip (leave disabled)".into()],
                    multi: false,
                },
            )
            .await?;
            let sel = recv_selection(&mut responses).await?;
            match sel.first().copied().unwrap_or(0) {
                0 => prompt_embedding_key(&events, &mut responses).await?,
                _ => None,
            }
        };

        let embedding_key = match embedding_key {
            Some(k) => k,
            None => {
                send(
                    &events,
                    ProvisionEvent::Message {
                        severity: Severity::Info,
                        text: "Knowledge Base left disabled (no embedding key).".into(),
                    },
                )
                .await?;
                send(
                    &events,
                    ProvisionEvent::Done {
                        summary: "Knowledge Base left disabled.".into(),
                    },
                )
                .await?;
                return Ok(());
            }
        };

        config.knowledge.embedding_api_key = Some(embedding_key.clone());

        // Step 3 — optional OCR/vision key
        send(
            &events,
            ProvisionEvent::Choose {
                id: "vis_src".into(),
                label: "OCR / vision key (for scanned or image documents)".into(),
                options: vec![
                    "Use the embedding key".into(),
                    "Enter a different key".into(),
                    "Skip OCR".into(),
                ],
                multi: false,
            },
        )
        .await?;

        let vis_sel = recv_selection(&mut responses).await?;
        let vision_key: Option<String> = match vis_sel.first().copied().unwrap_or(2) {
            0 => Some(embedding_key.clone()),
            1 => {
                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: "vis_key".into(),
                        label: "Paste your OCR/vision API key".into(),
                        default: None,
                        secret: true,
                    },
                )
                .await?;
                let raw = recv_text(&mut responses).await?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            _ => None,
        };
        config.knowledge.vision_api_key = vision_key;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "Knowledge Base configured (embedding{}).",
                    if config.knowledge.vision_api_key.is_some() {
                        " + OCR"
                    } else {
                        ""
                    }
                ),
            },
        )
        .await?;

        Ok(())
    }
}

async fn prompt_embedding_key(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<Option<String>> {
    send(
        events,
        ProvisionEvent::Prompt {
            id: "emb_key".into(),
            label: "Paste your embedding API key (OpenRouter)".into(),
            default: None,
            secret: true,
        },
    )
    .await?;
    let raw = recv_text(responses).await?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("failed to send provision event: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(super::traits::ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(super::traits::ProvisionResponse::Cancelled) => {
            anyhow::bail!("knowledge setup cancelled")
        }
        Some(_) => anyhow::bail!("unexpected response type"),
        None => anyhow::bail!("response channel closed unexpectedly"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<super::traits::ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(super::traits::ProvisionResponse::Text(t)) => Ok(t),
        Some(super::traits::ProvisionResponse::Cancelled) => {
            anyhow::bail!("knowledge setup cancelled")
        }
        Some(_) => anyhow::bail!("unexpected response type"),
        None => anyhow::bail!("response channel closed unexpectedly"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_knowledge() {
        let p = KnowledgeProvisioner::new();
        assert_eq!(p.name(), "knowledge");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = KnowledgeProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
