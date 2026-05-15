//! MCP process supervisor — monitors server processes and restarts on crash.
//! Exponential backoff: 1s → 2s → 4s → 8s → 16s → 32s → 60s (cap).
//! After 5 consecutive failures, server is marked Error and not restarted.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::McpRegistry;

const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_secs(5);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_mins(1);

fn backoff_delay(consecutive_failures: u32) -> Duration {
    let delay = BACKOFF_BASE * 2u32.saturating_pow(consecutive_failures.saturating_sub(1));
    delay.min(BACKOFF_CAP)
}

pub fn spawn_supervisor(
    registry: Arc<RwLock<McpRegistry>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("MCP supervisor started");
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    info!("MCP supervisor shutting down");
                    break;
                }
                () = tokio::time::sleep(SUPERVISOR_POLL_INTERVAL) => {
                    let server_ids = {
                        let reg = registry.read().await;
                        reg.server_ids()
                    };

                    for id in server_ids {
                        let needs_restart = {
                            let mut reg = registry.write().await;
                            if let Some(handle) = reg.get_server_mut(&id) {
                                if handle.status == super::handle::McpStatus::Stopped || handle.is_failed() {
                                    None
                                } else if !handle.is_running() {
                                    warn!("MCP server '{}' exited unexpectedly", id);
                                    if handle.record_failure() {
                                        Some(handle.consecutive_failures)
                                    } else {
                                        error!("MCP server '{}' permanently failed after 5 attempts", id);
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        };

                        if let Some(failures) = needs_restart {
                            let delay = backoff_delay(failures);
                            info!("MCP server '{}' will restart in {:?} (attempt {}/5)", id, delay, failures);
                            tokio::time::sleep(delay).await;

                            let mut reg = registry.write().await;
                            if let Some(handle) = reg.get_server_mut(&id) {
                                match handle.respawn().await {
                                    Ok(()) => info!("MCP server '{}' restarted successfully", id),
                                    Err(e) => error!("MCP server '{}' restart failed: {}", id, e),
                                }
                            }
                            break; // Only handle one restart per poll cycle
                        }
                    }
                }
            }
        }
    })
}
