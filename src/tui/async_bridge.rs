//! Actor that owns the Agent and serves the TUI's turn requests.

use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::agent::Agent;
use crate::agent::events::AgentEventSender;

#[derive(Debug)]
pub enum TurnRequest {
    Submit {
        text: String,
        /// Memory scope for this turn's reads and writes
        /// (`tui:<session_id>`). `None` recalls and stores globally —
        /// the pre-scoping behaviour, kept for unbound test agents.
        conversation_id: Option<String>,
    },
    Cancel,
    /// Replace the actor's `Agent` with one built from the supplied
    /// config. Used after the first-run wizard or `/setup` saves new
    /// provider/api_key/model so the running session picks up the
    /// new credentials without a `/quit` + relaunch.
    Reload(Box<crate::config::Config>),
    /// Compact older messages into a summary. Streams the summary
    /// through the normal event channel (Chunk / Done), then emits
    /// `AgentEvent::CompactionComplete` so the TUI can replace its
    /// in-memory + persisted history with `[system, summary, ...recent]`.
    ///
    /// `keep_last` is the count of trailing chat turns (user +
    /// assistant pairs) to preserve verbatim, default `10`.
    Compact {
        keep_last: usize,
    },
}

pub struct TuiAgentActor {
    /// `None` when the initial `Agent::from_config` failed at boot (e.g. a
    /// provider that cannot construct without an API key). The TUI still
    /// runs so the operator can repair the config via `/setup provider`;
    /// a successful `Reload` sets this to `Some` and heals the session
    /// in place. Turn requests while `None` get an actionable error event.
    agent: Option<Agent>,
    req_rx: mpsc::Receiver<TurnRequest>,
    events_tx: AgentEventSender,
    queue: VecDeque<(String, Option<String>)>,
    current: Option<CancellationToken>,
    /// Reload deferred until the in-flight turn completes — replacing
    /// `self.agent` mid-turn would invalidate the borrow.
    pending_reload: Option<Box<crate::config::Config>>,
}

/// Error event sent for a turn request that arrives while no working
/// agent exists (initial provider construction failed at boot).
const NO_AGENT_ERROR: &str =
    "No working provider — the agent failed to start with the current config. \
     Fix it via /setup provider (or edit config.toml), then retry.";

impl TuiAgentActor {
    pub fn new(
        agent: Option<Agent>,
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
                    Some(TurnRequest::Submit {
                        text,
                        conversation_id,
                    }) => self.queue.push_back((text, conversation_id)),
                    Some(TurnRequest::Cancel) => { /* no-op while idle */ }
                    Some(TurnRequest::Reload(config)) => {
                        match crate::agent::Agent::from_config(&config).await {
                            Ok(new_agent) => {
                                let mcp_tools_by_server = new_agent.mcp_tools_by_server();
                                let mcp_servers_configured: Vec<String> =
                                    config.mcp_servers.keys().cloned().collect();
                                let security = new_agent.security();
                                self.agent = Some(new_agent);
                                tracing::info!("agent reloaded with new config");
                                let _ = self
                                    .events_tx
                                    .send(crate::agent::events::AgentEvent::ReloadComplete {
                                        mcp_servers_configured,
                                        mcp_tools_by_server,
                                        security,
                                    })
                                    .await;
                            }
                            Err(e) => {
                                tracing::error!("failed to reload agent: {e}");
                            }
                        }
                    }
                    Some(TurnRequest::Compact { keep_last }) => {
                        // Tools-disabled side call to summarize older
                        // history. The agent emits `CompactionStart` /
                        // `Chunk*` / `CompactionComplete` itself — we
                        // just await the future. Errors surface as
                        // `AgentEvent::Error` (also emitted by the
                        // agent) so the TUI can render them.
                        let Some(agent) = self.agent.as_mut() else {
                            let _ = self
                                .events_tx
                                .send(crate::agent::events::AgentEvent::Error(
                                    NO_AGENT_ERROR.into(),
                                ))
                                .await;
                            continue;
                        };
                        if let Err(e) = agent
                            .compact_streaming(keep_last, Some(self.events_tx.clone()))
                            .await
                        {
                            tracing::warn!("compaction failed: {e}");
                            let _ = self
                                .events_tx
                                .send(crate::agent::events::AgentEvent::Error(e.to_string()))
                                .await;
                        }
                    }
                    None => return, // channel closed
                }
            }

            // Start the next queued turn if idle.
            if self.current.is_none() {
                if let Some((text, conversation_id)) = self.queue.pop_front() {
                    // No agent (boot-time provider failure): answer the turn
                    // with an actionable error and keep draining the queue —
                    // a later Reload restores normal service.
                    let Some(agent) = self.agent.as_mut() else {
                        let _ = self
                            .events_tx
                            .send(crate::agent::events::AgentEvent::Error(
                                NO_AGENT_ERROR.into(),
                            ))
                            .await;
                        continue;
                    };
                    // Scope this turn's memory to the conversation that
                    // submitted it — per request, like the gateway, so a
                    // session switch between queued turns takes effect on
                    // the turn it belongs to.
                    agent.set_conversation_id(conversation_id);
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
                        let mut turn_fut = Box::pin(agent.turn_streaming(
                            &text,
                            Some(events),
                            Some(token.clone()),
                        ));
                        loop {
                            tokio::select! {
                                biased;
                                maybe_req = self.req_rx.recv(), if !senders_dropped => {
                                    match maybe_req {
                                        Some(TurnRequest::Submit {
                                            text: more,
                                            conversation_id,
                                        }) => {
                                            self.queue.push_back((more, conversation_id));
                                        }
                                        Some(TurnRequest::Cancel) => token.cancel(),
                                        Some(TurnRequest::Reload(config)) => {
                                            // Defer until the active turn
                                            // ends — replacing self.agent
                                            // mid-turn would invalidate
                                            // turn_fut's &mut self.agent borrow.
                                            self.pending_reload = Some(config);
                                        }
                                        Some(TurnRequest::Compact { .. }) => {
                                            // Compaction mutates self.agent.history,
                                            // which turn_fut borrows mutably right
                                            // now. Reject with an actionable error
                                            // so the user can re-fire after the
                                            // turn finishes.
                                            let _ = self
                                                .events_tx
                                                .send(crate::agent::events::AgentEvent::Error(
                                                    "Cannot /compress while a turn is running — \
                                                     wait for the response to finish or /stop first."
                                                        .into(),
                                                ))
                                                .await;
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
                        match crate::agent::Agent::from_config(&config).await {
                            Ok(new_agent) => {
                                let mcp_tools_by_server = new_agent.mcp_tools_by_server();
                                let mcp_servers_configured: Vec<String> =
                                    config.mcp_servers.keys().cloned().collect();
                                let security = new_agent.security();
                                self.agent = Some(new_agent);
                                tracing::info!("agent reloaded with new config (post-turn)");
                                // Same ReloadComplete shape as the idle
                                // path so the TUI re-subscribes to the
                                // fresh PendingApprovals registry.
                                let _ = self
                                    .events_tx
                                    .send(crate::agent::events::AgentEvent::ReloadComplete {
                                        mcp_servers_configured,
                                        mcp_tools_by_server,
                                        security,
                                    })
                                    .await;
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
        let actor = TuiAgentActor::new(Some(build_test_agent("reply")), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit {
                text: "hi".into(),
                conversation_id: None,
            })
            .await
            .unwrap();

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
        let actor = TuiAgentActor::new(Some(build_test_agent("r")), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit {
                text: "first".into(),
                conversation_id: None,
            })
            .await
            .unwrap();
        req_tx
            .send(TurnRequest::Submit {
                text: "second".into(),
                conversation_id: None,
            })
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
        let actor = TuiAgentActor::new(Some(build_test_agent("x")), req_rx, events_tx);
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
        let actor = TuiAgentActor::new(Some(agent), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit {
                text: "start".into(),
                conversation_id: None,
            })
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

    #[tokio::test]
    async fn actor_without_agent_answers_submit_with_error_event() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(None, req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit {
                text: "hi".into(),
                conversation_id: None,
            })
            .await
            .unwrap();

        let ev = timeout(Duration::from_secs(2), events_rx.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        match ev {
            AgentEvent::Error(msg) => {
                assert!(
                    msg.contains("/setup provider"),
                    "error must point at the repair path, got: {msg}"
                );
            }
            other => panic!("expected Error event, got {other:?}"),
        }
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_without_agent_drains_every_queued_submit() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(None, req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit {
                text: "a".into(),
                conversation_id: None,
            })
            .await
            .unwrap();
        req_tx
            .send(TurnRequest::Submit {
                text: "b".into(),
                conversation_id: None,
            })
            .await
            .unwrap();

        let mut errors = 0;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Error(_)) {
                errors += 1;
                if errors == 2 {
                    break;
                }
            }
        }
        assert_eq!(errors, 2, "each queued submit gets its own error event");
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn actor_without_agent_answers_compact_with_error_event() {
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(None, req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Compact { keep_last: 10 })
            .await
            .unwrap();

        let ev = timeout(Duration::from_secs(2), events_rx.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        assert!(
            matches!(ev, AgentEvent::Error(_)),
            "expected Error event for /compress without an agent"
        );
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    /// Records the `session_id` every `store` receives, so a test can prove
    /// which conversation scope a turn's memory writes ran under.
    struct ScopeRecordingMemory {
        scopes: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl Memory for ScopeRecordingMemory {
        fn name(&self) -> &str {
            "scope-recording"
        }
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: crate::memory::MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.scopes
                .lock()
                .expect("scope mutex")
                .push(session_id.map(str::to_string));
            Ok(())
        }
        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    /// The seam this module owns: a `Submit` carrying a conversation id must
    /// scope the agent's turn memory to it. Auto-save is the observable —
    /// its `store` runs under the same scope `recall_layered` reads.
    #[tokio::test]
    async fn a_submitted_conversation_id_scopes_the_turns_memory_writes() {
        let scopes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(ScopeRecordingMemory {
            scopes: scopes.clone(),
        });
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let agent = Agent::builder()
            .provider(Box::new(EchoProvider("reply")))
            .tools(vec![])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .auto_save(true)
            .build()
            .expect("agent builder should succeed");

        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let actor = TuiAgentActor::new(Some(agent), req_rx, events_tx);
        let handle = tokio::spawn(actor.run());

        req_tx
            .send(TurnRequest::Submit {
                text: "remember me".into(),
                conversation_id: Some("tui:s1".into()),
            })
            .await
            .unwrap();

        let mut got_done = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(2), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                got_done = true;
                break;
            }
        }
        assert!(got_done, "expected Done event");

        let seen = scopes.lock().expect("scope mutex").clone();
        assert!(
            seen.contains(&Some("tui:s1".to_string())),
            "auto-save must run under the submitted conversation scope, saw {seen:?}"
        );
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }
}
