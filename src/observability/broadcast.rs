use super::traits::{Observer, ObserverEvent, ObserverMetric};
use std::any::Any;
use tokio::sync::broadcast;

/// Observer wrapper that forwards all events to both an inner observer and a
/// tokio broadcast channel, enabling real-time SSE streaming of agent lifecycle events.
pub struct BroadcastObserver {
    inner: Box<dyn Observer>,
    tx: broadcast::Sender<ObserverEvent>,
}

impl BroadcastObserver {
    /// Create a new `BroadcastObserver` wrapping `inner` with a broadcast channel
    /// of the given `capacity`.
    ///
    /// Returns the observer and a keep-alive receiver (drop it if you only use `subscribe()`).
    pub fn new(inner: Box<dyn Observer>, capacity: usize) -> (Self, broadcast::Receiver<ObserverEvent>) {
        let (tx, rx) = broadcast::channel(capacity);
        (Self { inner, tx }, rx)
    }

    /// Get a new receiver for the broadcast channel.
    /// Each receiver independently tracks its position in the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<ObserverEvent> {
        self.tx.subscribe()
    }
}

impl Observer for BroadcastObserver {
    fn record_event(&self, event: &ObserverEvent) {
        self.inner.record_event(event);
        // Best-effort broadcast — drop events if no receivers or channel full.
        let _ = self.tx.send(event.clone());
    }

    fn record_metric(&self, metric: &ObserverMetric) {
        self.inner.record_metric(metric);
    }

    fn flush(&self) {
        self.inner.flush();
    }

    fn name(&self) -> &str {
        "broadcast"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::noop::NoopObserver;
    use std::time::Duration;

    #[test]
    fn broadcast_observer_name() {
        let (obs, _rx) = BroadcastObserver::new(Box::new(NoopObserver), 16);
        assert_eq!(obs.name(), "broadcast");
    }

    #[test]
    fn broadcast_sends_events_to_subscribers() {
        let (obs, _keep) = BroadcastObserver::new(Box::new(NoopObserver), 16);
        let mut rx = obs.subscribe();

        obs.record_event(&ObserverEvent::HeartbeatTick);
        obs.record_event(&ObserverEvent::ToolCallStart {
            tool: "shell".into(),
            call_id: Some("tc_0_0".into()),
            arguments: None,
        });

        let e1 = rx.try_recv().unwrap();
        assert!(matches!(e1, ObserverEvent::HeartbeatTick));

        let e2 = rx.try_recv().unwrap();
        assert!(matches!(e2, ObserverEvent::ToolCallStart { .. }));
    }

    #[test]
    fn broadcast_does_not_panic_without_receivers() {
        let (obs, rx) = BroadcastObserver::new(Box::new(NoopObserver), 4);
        drop(rx);
        // Should not panic even with no receivers
        obs.record_event(&ObserverEvent::HeartbeatTick);
    }

    #[test]
    fn broadcast_downcast_works() {
        let (obs, _rx) = BroadcastObserver::new(Box::new(NoopObserver), 4);
        let any = obs.as_any();
        assert!(any.downcast_ref::<BroadcastObserver>().is_some());
    }

    #[test]
    fn broadcast_subscribe_returns_independent_receivers() {
        let (obs, _keep) = BroadcastObserver::new(Box::new(NoopObserver), 16);
        let mut rx1 = obs.subscribe();
        let mut rx2 = obs.subscribe();

        obs.record_event(&ObserverEvent::AgentEnd {
            provider: "test".into(),
            model: "test".into(),
            duration: Duration::from_millis(100),
            tokens_used: Some(42),
            cost_usd: None,
        });

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }
}
