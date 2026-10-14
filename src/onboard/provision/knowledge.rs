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

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner};
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
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

    fn category(&self) -> crate::onboard::provision::traits::ProvisionerCategory {
        // Must match the first-run wizard, which lists `knowledge` under
        // Integrations — without this override the trait default (Core) made
        // the same item appear in different groups depending on the entry
        // point (plan 107). Pinned by category_matches_first_run_wizard.
        crate::onboard::provision::traits::ProvisionerCategory::Integration
    }

    async fn run(
        &self,
        config: &mut Config,
        _profile: &Profile,
        io: ProvisionIo,
    ) -> Result<ProvisionOutcome> {
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
                filterable: false,
            },
        )
        .await?;

        let selection = recv_selection(&mut responses).await?;
        // Only an explicit "Enable" (row 0) continues; an empty answer takes the
        // skip branch, leaving the Knowledge Base disabled.
        if selection.first().copied() != Some(0) {
            send(
                &events,
                ProvisionEvent::Done {
                    summary: "Knowledge Base left disabled.".into(),
                },
            )
            .await?;
            return Ok(ProvisionOutcome::Configured);
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
                    filterable: false,
                },
            )
            .await?;
            let sel = recv_selection(&mut responses).await?;
            // An empty answer is "skip" (`_`), never the stored-key row.
            match sel.first().copied() {
                Some(0) => config.api_key.clone(),
                Some(1) => prompt_embedding_key(&events, &mut responses).await?,
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
                    filterable: false,
                },
            )
            .await?;
            let sel = recv_selection(&mut responses).await?;
            // An empty answer is "skip" (`_`), never the enter-a-key row.
            match sel.first().copied() {
                Some(0) => prompt_embedding_key(&events, &mut responses).await?,
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
                return Ok(ProvisionOutcome::Configured);
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
                filterable: false,
            },
        )
        .await?;

        // An empty answer is "Skip OCR" (row 2): no key is stored, so nothing unseen is written.
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

        Ok(ProvisionOutcome::Configured)
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

#[cfg(test)]
mod tests {
    use crate::onboard::provision::test_support::{drive, scratch_profile, Answer};
    use crate::onboard::provision::ProvisionEvent;

    /// An empty answer at "Enable Knowledge Base?" must leave it disabled and
    /// must not ask for an embedding key.
    #[tokio::test]
    async fn empty_enable_answer_leaves_the_knowledge_base_disabled() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let profile = scratch_profile(tmp.path());
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_path_buf();

        let t = drive(
            &super::KnowledgeProvisioner,
            &mut config,
            &profile,
            vec![Answer::Empty],
        )
        .await;

        // The embedding-key flow asks through the `emb_src` choose and the
        // `emb_key` prompt; neither may appear for an empty enable answer.
        let asked_for_key = t.events.iter().any(|e| {
            matches!(e, ProvisionEvent::Choose { id, .. } if id == "emb_src")
                || matches!(e, ProvisionEvent::Prompt { id, .. } if id == "emb_key")
        });
        assert!(
            !asked_for_key,
            "an empty enable answer must not reach the embedding-key prompts"
        );
        assert!(t.configured(), "the skip branch completes: {:?}", t.outcome);
    }
}
