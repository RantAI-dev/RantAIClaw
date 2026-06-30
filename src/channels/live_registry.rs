//! Process-global registry of *live* channel instances.
//!
//! The channel supervisor (`start_channels_with_cancellation`) publishes the
//! live [`Channel`] instances here so the cron scheduler — which runs in the
//! same daemon process — can deliver scheduled output through an
//! already-connected session (e.g. the WhatsApp web `wa-rs` socket) instead of
//! constructing a fresh, unauthenticated channel.
//!
//! This is distinct from [`crate::channels::registry::ChannelRegistry`], which
//! manages per-channel lifecycle (add/remove/restart). This module is a simple
//! name → live-instance lookup, mirroring the file's existing process-global
//! pattern (`runtime_config_store`).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::channels::traits::Channel;

fn registry() -> &'static RwLock<HashMap<String, Arc<dyn Channel>>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, Arc<dyn Channel>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Publish the live channel instances, keyed by [`Channel::name`].
/// Replaces any previously published set. Each value is the same `Arc` the
/// supervised `listen()` task drives, so a `send()` issued through it reuses the
/// already-connected session.
// The caller (the channel supervisor) always uses the default hasher; no need
// to generalize this internal API over `BuildHasher`.
#[allow(clippy::implicit_hasher)]
pub fn publish_live_channels(channels: Arc<HashMap<String, Arc<dyn Channel>>>) {
    let mut guard = registry().write().unwrap_or_else(|e| e.into_inner());
    *guard = channels.as_ref().clone();
}

/// Look up a live channel by name (e.g. `"whatsapp"`, `"telegram"`).
pub fn live_channel(name: &str) -> Option<Arc<dyn Channel>> {
    let guard = registry().read().unwrap_or_else(|e| e.into_inner());
    guard.get(name).map(Arc::clone)
}

/// Remove all published channels (called on channel-supervisor shutdown).
pub fn clear_live_channels() {
    let mut guard = registry().write().unwrap_or_else(|e| e.into_inner());
    guard.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::traits::{ChannelMessage, SendMessage};
    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    /// Serialize tests that touch the process-global registry.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct NoopChannel(&'static str);

    #[async_trait]
    impl Channel for NoopChannel {
        fn name(&self) -> &str {
            self.0
        }
        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            Ok(())
        }
        async fn listen(
            &self,
            _tx: tokio::sync::mpsc::Sender<ChannelMessage>,
            _cancel: CancellationToken,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn map_with(name: &'static str) -> Arc<HashMap<String, Arc<dyn Channel>>> {
        let mut map: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        map.insert(name.to_string(), Arc::new(NoopChannel(name)));
        Arc::new(map)
    }

    #[test]
    fn publish_then_live_channel_returns_instance_by_name() {
        let _g = test_guard();
        clear_live_channels();

        publish_live_channels(map_with("whatsapp"));

        let found = live_channel("whatsapp");
        assert!(found.is_some(), "published channel should be retrievable");
        assert_eq!(found.unwrap().name(), "whatsapp");
        assert!(
            live_channel("telegram").is_none(),
            "unpublished channel should miss"
        );
    }

    #[test]
    fn clear_removes_published_channels() {
        let _g = test_guard();
        clear_live_channels();

        publish_live_channels(map_with("telegram"));
        assert!(live_channel("telegram").is_some());

        clear_live_channels();
        assert!(
            live_channel("telegram").is_none(),
            "clear should empty the registry"
        );
    }
}
