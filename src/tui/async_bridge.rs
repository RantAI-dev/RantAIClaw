//! Actor that owns the Agent and serves the TUI's turn requests.

use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::agent::Agent;
use crate::agent::events::AgentEventSender;

#[derive(Debug)]
pub enum TurnRequest {
    Submit(String),
    Cancel,
}

#[allow(dead_code)]
pub struct TuiAgentActor {
    agent: Agent,
    req_rx: mpsc::Receiver<TurnRequest>,
    events_tx: AgentEventSender,
    queue: VecDeque<String>,
    current: Option<CancellationToken>,
}

impl TuiAgentActor {
    pub fn new(
        agent: Agent,
        req_rx: mpsc::Receiver<TurnRequest>,
        events_tx: AgentEventSender,
    ) -> Self {
        Self {
            agent,
            req_rx,
            events_tx,
            queue: VecDeque::new(),
            current: None,
        }
    }

    /// Run the actor loop. Placeholder — will be implemented in Task 9.
    pub async fn run(mut self) {
        // Task 9 will replace this with select!-based submit/cancel/queue handling.
        // For now: drain requests so the channel doesn't deadlock in unit tests.
        while let Some(_req) = self.req_rx.recv().await {
            // noop
        }
        let _ = self.agent;
        let _ = &self.events_tx;
        let _ = &self.queue;
        let _ = &self.current;
    }
}
