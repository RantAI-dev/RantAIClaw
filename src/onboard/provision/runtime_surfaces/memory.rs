//! Memory provisioner — implements [`TuiProvisioner`] for in-TUI memory backend setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner,
};
use crate::config::schema::MemoryConfig;
use crate::config::Config;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MEMORY_NAME: &str = "memory";
pub const MEMORY_DESC: &str = "Memory backend — sqlite, lucid, postgres, markdown, or none";

#[derive(Debug, Clone)]
pub struct MemoryProvisioner;

impl MemoryProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MemoryProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for MemoryProvisioner {
    fn name(&self) -> &'static str {
        MEMORY_NAME
    }

    fn description(&self) -> &'static str {
        MEMORY_DESC
    }

    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Runtime
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
                text: "Let's configure memory backend.".into(),
            },
        )
        .await?;

        // Backend selection
        send(
            &events,
            ProvisionEvent::Choose {
                id: "backend".into(),
                label: "Memory backend".into(),
                options: vec![
                    "sqlite (default, embedded)".to_string(),
                    "lucid (high-performance)".to_string(),
                    "postgres (server)".to_string(),
                    "markdown (file-based)".to_string(),
                    "none (no memory)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let backend = match sel.first().copied().unwrap_or(0) {
            0 => "sqlite",
            1 => "lucid",
            2 => "postgres",
            3 => "markdown",
            _ => "none",
        }
        .to_string();

        let memory_cfg = MemoryConfig {
            backend: backend.clone(),
            auto_save: true,
            hygiene_enabled: true,
            archive_after_days: 30,
            purge_after_days: 90,
            conversation_retention_days: 90,
            embedding_provider: "none".into(),
            embedding_model: "text-embedding-3-small".into(),
            embedding_dimensions: 1536,
            vector_weight: 0.5,
            keyword_weight: 0.5,
            min_relevance_score: 0.4,
            embedding_cache_size: 10000,
            chunk_max_tokens: 512,
            response_cache_enabled: false,
            response_cache_ttl_minutes: 60,
            response_cache_max_entries: 5000,
            snapshot_enabled: false,
            snapshot_on_hygiene: true,
            auto_hydrate: true,
            sqlite_open_timeout_secs: None,
        };

        // Backend-specific prompts
        if backend == "sqlite" {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "db_path".into(),
                    label: "DB path (Enter for default <profile>/memory.db)".into(),
                    default: Some("<profile>/memory.db".into()),
                    secret: false,
                },
            )
            .await?;
            let _path = recv_text(&mut responses).await?;
            // Path is informational — actual path resolved at runtime
        } else if backend == "postgres" {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "dsn".into(),
                    label: "Postgres DSN (e.g. postgres://user:pass@host:5432/db)".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;
            let dsn = recv_text(&mut responses).await?;
            if dsn.trim().is_empty() {
                send(
                    &events,
                    ProvisionEvent::Failed {
                        error: "Postgres DSN is required.".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            // DSN stored in storage provider config — note for now
            send(
                &events,
                ProvisionEvent::Message {
                    severity: Severity::Info,
                    text: "DSN noted (configure in [storage.provider.config] for full integration)"
                        .to_string(),
                },
            )
            .await?;
        } else if backend == "markdown" {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "dir".into(),
                    label: "Markdown directory (Enter for default <profile>/memory/)".into(),
                    default: Some("<profile>/memory/".into()),
                    secret: false,
                },
            )
            .await?;
            let _dir = recv_text(&mut responses).await?;
        } else if backend == "lucid" {
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "server".into(),
                    label: "Lucid server URL".into(),
                    default: None,
                    secret: false,
                },
            )
            .await?;
            let _server = recv_text(&mut responses).await?;
            send(
                &events,
                ProvisionEvent::Prompt {
                    id: "api_key".into(),
                    label: "Lucid API key (Enter to skip)".into(),
                    default: None,
                    secret: true,
                },
            )
            .await?;
            let _key = recv_text(&mut responses).await?;
        }

        config.memory = memory_cfg;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Memory backend set to {}.", backend),
            },
        )
        .await?;

        Ok(())
    }
}

use crate::onboard::provision::ProvisionerCategory;

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(ProvisionResponse::Text(t)) => Ok(t),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_memory() {
        assert_eq!(MemoryProvisioner::new().name(), "memory");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        assert!(!MemoryProvisioner::new().description().is_empty());
    }
}
