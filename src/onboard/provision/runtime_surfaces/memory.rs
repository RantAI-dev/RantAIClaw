//! Memory provisioner — implements [`TuiProvisioner`] for in-TUI memory backend setup.

use super::super::traits::{
    ProvisionEvent, ProvisionIo, ProvisionOutcome, Severity, TuiProvisioner,
};
use crate::config::schema::MemoryConfig;
use crate::config::Config;
use crate::onboard::provision::io::{recv_selection, recv_text, send};
use crate::onboard::provision::ProvisionerCategory;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MEMORY_NAME: &str = "memory";
pub const MEMORY_DESC: &str = "Memory backend — sqlite or none";

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
                text: "Let's configure memory backend.".into(),
            },
        )
        .await?;

        // Backend selection — `postgres` was retired together with the
        // `[storage]` section. Only `sqlite` and `none`
        // are offered here; an operator who had a Postgres profile must
        // migrate the notes themselves or run against a sqlite store.
        send(
            &events,
            ProvisionEvent::Choose {
                id: "backend".into(),
                label: "Memory backend".into(),
                options: vec![
                    "sqlite (default, embedded)".to_string(),
                    "none (no memory)".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let sel = recv_selection(&mut responses).await?;
        let backend = match sel.first().copied().unwrap_or(0) {
            0 => "sqlite",
            _ => "none",
        }
        .to_string();

        let memory_cfg = MemoryConfig {
            backend: backend.clone(),
            hygiene_enabled: true,
            archive_after_days: 30,
            purge_after_days: 90,
            conversation_retention_days: 90,
            embedding_provider: "none".into(),
            embedding_model: "text-embedding-3-small".into(),
            embedding_dimensions: 1536,
            vector_weight: 0.5,
            keyword_weight: 0.5,
            min_relevance_score: MemoryConfig::default().min_relevance_score,
            embedding_cache_size: 10000,
            snapshot_enabled: false,
            snapshot_on_hygiene: true,
            auto_hydrate: true,
            sqlite_open_timeout_secs: None,
        };

        // Backend-specific prompts — only sqlite asks for the db path.
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
        }

        config.memory = memory_cfg;

        send(
            &events,
            ProvisionEvent::Done {
                summary: format!("Memory backend set to {}.", backend),
            },
        )
        .await?;

        Ok(ProvisionOutcome::Configured)
    }
}
