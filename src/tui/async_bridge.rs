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
    /// Replace the actor's `Agent` with one built from the supplied
    /// config. Used after the first-run wizard or `/setup` saves new
    /// provider/api_key/model so the running session picks up the
    /// new credentials without a `/quit` + relaunch.
    Reload(Box<crate::config::Config>),
}

pub struct TuiAgentActor {
    agent: Agent,
    req_rx: mpsc::Receiver<TurnRequest>,
    events_tx: AgentEventSender,
    queue: VecDeque<String>,
    current: Option<CancellationToken>,
    /// Reload deferred until the in-flight turn completes — replacing
    /// `self.agent` mid-turn would invalidate the borrow.
    pending_reload: Option<Box<crate::config::Config>>,
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
            pending_reload: None,
        }
    }

    /// Run the actor loop. Consumes `TurnRequest`s and drives `Agent` turns.
    ///
    /// Semantics:
    ///   * `Submit` while idle — start a turn immediately.
    ///   * `Submit` while busy — enqueue; runs after the current turn finishes.
    ///   * `Cancel` while busy — cancels the current turn via its token.
    ///   * `Cancel` while idle — no-op.
    ///   * Channel closed (all senders dropped) — drain current turn (if any)
    ///     and exit. Queued submits after the last in-flight turn are dropped.
    pub async fn run(mut self) {
        loop {
            // Idle path: block on the next request.
            if self.current.is_none() && self.queue.is_empty() {
                match self.req_rx.recv().await {
                    Some(TurnRequest::Submit(text)) => self.queue.push_back(text),
                    Some(TurnRequest::Cancel) => { /* no-op while idle */ }
                    Some(TurnRequest::Reload(config)) => {
                        match crate::agent::Agent::from_config(&config) {
                            Ok(new_agent) => {
                                self.agent = new_agent;
                                tracing::info!("agent reloaded with new config");
                            }
                            Err(e) => {
                                tracing::error!("failed to reload agent: {e}");
                            }
                        }
                    }
                    None => return, // channel closed
                }
            }

            // Start the next queued turn if idle.
            if self.current.is_none() {
                if let Some(text) = self.queue.pop_front() {
                    let token = CancellationToken::new();
                    self.current = Some(token.clone());
                    let events = self.events_tx.clone();

                    // Drain incoming requests while the turn runs. On channel
                    // close, stop draining but still let the turn finish.
                    let mut senders_dropped = false;
                    {
                        // Pin the turn future so we can poll it alongside
                        // req_rx. turn_streaming takes &mut self, so the
                        // future borrows self.agent exclusively for its
                        // lifetime — confined to this inner block so
                        // self.agent is free for post-turn reload.
                        let mut turn_fut = Box::pin(self.agent.turn_streaming(
                            &text,
                            Some(events),
                            Some(token.clone()),
                        ));
                        loop {
                            tokio::select! {
                                biased;
                                maybe_req = self.req_rx.recv(), if !senders_dropped => {
                                    match maybe_req {
                                        Some(TurnRequest::Submit(more)) => {
                                            self.queue.push_back(more);
                                        }
                                        Some(TurnRequest::Cancel) => token.cancel(),
                                        Some(TurnRequest::Reload(config)) => {
                                            // Defer until the active turn
                                            // ends — replacing self.agent
                                            // mid-turn would invalidate
                                            // turn_fut's &mut self.agent borrow.
                                            self.pending_reload = Some(config);
                                        }
                                        None => {
                                            senders_dropped = true;
                                        }
                                    }
                                }
                                res = &mut turn_fut => {
                                    let _ = res;
                                    self.current = None;
                                    break;
                                }
                            }
                        }
                    } // turn_fut dropped here — self.agent no longer borrowed.

                    // Apply any reload that arrived during the turn.
                    if let Some(config) = self.pending_reload.take() {
                        match crate::agent::Agent::from_config(&config) {
                            Ok(new_agent) => {
                                self.agent = new_agent;
                                tracing::info!("agent reloaded with new config (post-turn)");
                            }
                            Err(e) => {
                                tracing::error!("failed to reload agent post-turn: {e}");
                            }
                        }
                    }

                    // If senders dropped, exit after the current turn completes.
                    if senders_dropped {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent::Agent;
    use crate::agent::dispatcher::XmlToolDispatcher;
    use crate::agent::events::AgentEvent;
    use crate::memory::Memory;
    use crate::observability::Observer;
    use crate::providers::{ChatRequest, ChatResponse, Provider};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    struct EchoProvider(&'static str);

    #[async_trait]
    impl Provider for EchoProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some(self.0.to_string()),
                tool_calls: vec![],
            })
        }
    }

    fn build_test_agent_with_provider(provider: Box<dyn Provider>) -> Agent {
        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});

        Agent::builder()
            .provider(provider)
            .tools(vec![])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config")
    }

    fn build_test_agent(response_text: &'static str) -> Agent {
        build_test_agent_with_provider(Box::new(EchoProvider(response_text)))
    }

    #[tokio::test]
    async fn actor_processes_single_submit_and_emits_done() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(build_test_agent("reply"), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx.send(TurnRequest::Submit("hi".into())).await.unwrap();

        let mut got_done = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), events_rx.recv()).await {
            if let AgentEvent::Done {
                final_text,
                cancelled,
            } = ev
            {
                assert_eq!(final_text, "reply");
                assert!(!cancelled);
                got_done = true;
                break;
            }
        }
        assert!(got_done, "expected Done event");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_processes_queued_submit_after_first_completes() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(build_test_agent("r"), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit("first".into()))
            .await
            .unwrap();
        req_tx
            .send(TurnRequest::Submit("second".into()))
            .await
            .unwrap();

        let mut done_count = 0;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(3), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                done_count += 1;
                if done_count == 2 {
                    break;
                }
            }
        }
        assert_eq!(done_count, 2, "both turns should complete, in order");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_cancel_while_idle_is_a_noop() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(build_test_agent("x"), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx.send(TurnRequest::Cancel).await.unwrap();
        let result = timeout(Duration::from_millis(150), events_rx.recv()).await;
        assert!(result.is_err(), "no event expected from idle Cancel");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_cancel_while_streaming_yields_done_cancelled() {
        use tokio::time::sleep;

        struct SlowProvider;

        #[async_trait]
        impl Provider for SlowProvider {
            async fn chat_with_system(
                &self,
                _system_prompt: Option<&str>,
                _message: &str,
                _model: &str,
                _temperature: f64,
            ) -> anyhow::Result<String> {
                Ok("x".into())
            }

            async fn chat(
                &self,
                _request: ChatRequest<'_>,
                _model: &str,
                _temperature: f64,
            ) -> anyhow::Result<ChatResponse> {
                sleep(Duration::from_millis(300)).await;
                Ok(ChatResponse {
                    text: Some("late".into()),
                    tool_calls: vec![],
                })
            }
        }

        let agent = build_test_agent_with_provider(Box::new(SlowProvider));
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(agent, req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit("start".into()))
            .await
            .unwrap();
        sleep(Duration::from_millis(50)).await;
        req_tx.send(TurnRequest::Cancel).await.unwrap();

        let mut cancelled_done = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), events_rx.recv()).await {
            if let AgentEvent::Done {
                cancelled: true, ..
            } = ev
            {
                cancelled_done = true;
                break;
            }
        }
        assert!(cancelled_done, "expected Done {{ cancelled: true }}");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }
}
