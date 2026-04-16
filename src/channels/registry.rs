//! Per-channel lifecycle management with graceful shutdown support.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::channels::traits::{Channel, ChannelMessage};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", content = "error")]
pub enum ChannelStatus {
    Running,
    Stopped,
    Error(String),
}

struct ChannelHandle {
    config: serde_json::Value,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    status: ChannelStatus,
}

pub struct ChannelRegistry {
    channels: HashMap<String, ChannelHandle>,
    message_tx: tokio::sync::mpsc::Sender<ChannelMessage>,
}

impl ChannelRegistry {
    pub fn new(message_tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> Self {
        Self {
            channels: HashMap::new(),
            message_tx,
        }
    }

    /// Add and start a new channel. The `spawn_fn` closure constructs the
    /// channel from config and returns a boxed Channel trait object.
    pub async fn add_channel<F, Fut>(
        &mut self,
        id: String,
        config: serde_json::Value,
        spawn_fn: F,
    ) -> Result<()>
    where
        F: FnOnce(serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<Box<dyn Channel + Send + Sync>>>,
    {
        if self.channels.contains_key(&id) {
            return Err(anyhow!("Channel '{}' already exists", id));
        }

        let channel = spawn_fn(config.clone()).await?;
        let cancel = CancellationToken::new();
        let tx = self.message_tx.clone();
        let cancel_clone = cancel.clone();
        let channel_id = id.clone();

        let task = tokio::spawn(async move {
            if let Err(e) = channel.listen(tx, cancel_clone).await {
                error!("Channel '{}' listen error: {}", channel_id, e);
            }
        });

        self.channels.insert(
            id.clone(),
            ChannelHandle {
                config,
                cancel,
                task,
                status: ChannelStatus::Running,
            },
        );

        info!("Channel '{}' started", id);
        Ok(())
    }

    /// Gracefully stop and remove a channel.
    pub async fn remove_channel(&mut self, id: &str) -> Result<()> {
        let handle = self
            .channels
            .remove(id)
            .ok_or_else(|| anyhow!("Channel '{}' not found", id))?;

        handle.cancel.cancel();

        let result = tokio::time::timeout(SHUTDOWN_TIMEOUT, handle.task).await;
        match result {
            Ok(Ok(())) => info!("Channel '{}' stopped gracefully", id),
            Ok(Err(e)) => warn!("Channel '{}' task panicked: {}", id, e),
            Err(_) => {
                warn!(
                    "Channel '{}' did not stop within {}s, aborting",
                    id,
                    SHUTDOWN_TIMEOUT.as_secs()
                );
            }
        }

        Ok(())
    }

    /// Update a channel by removing and re-adding with new config.
    pub async fn update_channel<F, Fut>(
        &mut self,
        id: String,
        config: serde_json::Value,
        spawn_fn: F,
    ) -> Result<()>
    where
        F: FnOnce(serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<Box<dyn Channel + Send + Sync>>>,
    {
        if let Some(existing) = self.channels.get(&id) {
            if existing.config == config {
                info!("Channel '{}' config unchanged, skipping update", id);
                return Ok(());
            }
        }

        let _ = self.remove_channel(&id).await;
        self.add_channel(id, config, spawn_fn).await
    }

    /// List all channels and their statuses.
    pub fn list_channels(&self) -> HashMap<String, ChannelStatus> {
        self.channels
            .iter()
            .map(|(id, handle)| (id.clone(), handle.status.clone()))
            .collect()
    }

    /// Check if a channel exists.
    pub fn has_channel(&self, id: &str) -> bool {
        self.channels.contains_key(id)
    }

    /// Get the config for a channel.
    pub fn get_config(&self, id: &str) -> Option<&serde_json::Value> {
        self.channels.get(id).map(|h| &h.config)
    }

    /// Shut down all channels.
    pub async fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.channels.keys().cloned().collect();
        for id in ids {
            if let Err(e) = self.remove_channel(&id).await {
                warn!("Error shutting down channel '{}': {}", id, e);
            }
        }
    }
}
