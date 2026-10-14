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
    /// Replace the agent's history with the given conversational turns, under
    /// the system prompt the agent builds itself. The screen's session is the
    /// one whose turns these are: `/new` sends none, a session pick or `/undo`
    /// sends what the session still holds. Queued behind a running turn, like
    /// `Reload`, so the next prompt is sent with the new history.
    SetHistory(Vec<(String, String)>),
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
    /// History replacements that arrived during the turn, applied in order
    /// once it ends; the last one leaves the history.
    pending_history: Vec<Vec<(String, String)>>,
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
            pending_history: Vec::new(),
        }
    }

    /// Move the conversation of the agent being replaced into `new_agent`, so a
    /// reload (`/model`, `/setup`, a skills change, an outside config edit)
    /// does not lose the turns the screen still shows. Only the system prompt
    /// is rebuilt, from the new config. With no agent to replace, the new one
    /// starts as it is. If the prompt cannot be rebuilt the turns are left on
    /// the old agent, the failure is logged, and the new agent starts empty.
    fn carry_history_into(&mut self, new_agent: &mut Agent) {
        let Some(previous) = self.agent.as_mut() else {
            return;
        };
        let carried = crate::memory::MEMORY_VIEW.sync_scope(crate::memory::MemoryView::All, || {
            new_agent.adopt_history_from(previous)
        });
        if let Err(e) = carried {
            tracing::warn!("failed to carry the conversation across a reload: {e}");
        }
    }

    /// Replace the agent's history with `turns` under the system prompt the
    /// agent builds. The prompt reads the memory view, so the rebuild runs
    /// under `All`, as the other seeding callers do. A failed build is logged
    /// and leaves the history as it was.
    fn reseed(&mut self, turns: &[(String, String)]) {
        let Some(agent) = self.agent.as_mut() else {
            return;
        };
        let reseeded = crate::memory::MEMORY_VIEW
            .sync_scope(crate::memory::MemoryView::All, || {
                agent.reseed_history(turns)
            });
        if let Err(e) = reseeded {
            tracing::warn!("failed to reseed agent history: {e}");
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
                    Some(TurnRequest::SetHistory(turns)) => {
                        self.reseed(&turns);
                    }
                    Some(TurnRequest::Reload(config)) => {
                        match crate::agent::Agent::from_config(&config).await {
                            Ok(mut new_agent) => {
                                new_agent.set_surface("tui");
                                self.carry_history_into(&mut new_agent);
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
                                let _ = self
                                    .events_tx
                                    .send(crate::agent::events::AgentEvent::ReloadFailed {
                                        error: format!("{e:#}"),
                                    })
                                    .await;
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
                        //
                        // The TUI is the operator's own surface, so its turn
                        // reads all of memory.
                        let mut turn_fut = Box::pin(crate::memory::MEMORY_VIEW.scope(
                            crate::memory::MemoryView::All,
                            agent.turn_streaming(&text, Some(events), Some(token.clone())),
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
                                        Some(TurnRequest::SetHistory(turns)) => {
                                            // Defer, as Reload does: the active turn
                                            // holds the agent's history.
                                            self.pending_history.push(turns);
                                        }
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

                    // Apply history replacements that arrived during the turn,
                    // in order, so the last one is what the next prompt sees.
                    for turns in std::mem::take(&mut self.pending_history) {
                        self.reseed(&turns);
                    }

                    // Apply any reload that arrived during the turn.
                    if let Some(config) = self.pending_reload.take() {
                        match crate::agent::Agent::from_config(&config).await {
                            Ok(mut new_agent) => {
                                new_agent.set_surface("tui");
                                self.carry_history_into(&mut new_agent);
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
                                let _ = self
                                    .events_tx
                                    .send(crate::agent::events::AgentEvent::ReloadFailed {
                                        error: format!("{e:#}"),
                                    })
                                    .await;
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
    use crate::memory::{Memory, SessionScope};
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
                usage: None,
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
                    usage: None,
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
            _scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _category: Option<&crate::memory::MemoryCategory>,
            _scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self, _scope: SessionScope<'_>) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    /// The seam this module owns: a `Submit` carrying a conversation id must
    /// not cause the TUI actor to write a row on its own.
    ///
    /// The conversation id once scoped the headless autosave the door used to
    /// run. That writer is retired — every entry in `memories` now arrives only
    /// because someone asked (a `memory_store` tool call, the CLI, the console,
    /// or a TUI command bound to the same actor). With the autosave gone, the
    /// submitted id stays on the agent for read-side scope only; the door's
    /// own `MEMORY_VIEW` remains `All` because the TUI is the operator's
    /// surface, and that read-scope invariant is covered by the neighbouring
    /// `the_tui_turn_door_reads_all_of_memory` test. This test pins the
    /// write-side half: a row reaches memory only when the operator (or a
    /// tool they trust) asks — never from the inbound submit itself.
    #[tokio::test]
    async fn a_submitted_conversation_id_does_not_write_memory_on_its_own() {
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
            seen.is_empty(),
            "the TUI actor must not write a memory row from inbound input alone; \
             a row enters memory only via an explicit tool call, CLI, console, or \
             TUI command. Saw {seen:?} store call(s) under the submitted scope."
        );
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
    }

    // ── memory view ──────────────────────────────────────────────────────

    /// A provider that keeps the messages of every request, and answers from a
    /// script of responses (then with `ok`).
    struct ScriptedChat {
        script: std::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
        seen: std::sync::Mutex<Vec<Vec<(String, String)>>>,
    }

    impl ScriptedChat {
        fn new(script: Vec<ChatResponse>) -> Arc<Self> {
            Arc::new(Self {
                script: std::sync::Mutex::new(script.into()),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Provider for Arc<ScriptedChat> {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("fallback".to_string())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            self.seen.lock().expect("seen mutex").push(
                request
                    .messages
                    .iter()
                    .map(|m| (m.role.clone(), m.content.clone()))
                    .collect(),
            );
            Ok(self
                .script
                .lock()
                .expect("script mutex")
                .pop_front()
                .unwrap_or(ChatResponse {
                    usage: None,
                    text: Some("ok".to_string()),
                    tool_calls: vec![],
                }))
        }
    }

    fn texts(provider: &ScriptedChat) -> (String, String) {
        let seen = provider.seen.lock().expect("seen mutex").clone();
        let join = |system: bool| {
            seen.iter()
                .flatten()
                .filter(|(role, _)| (role == "system") == system)
                .map(|(_, content)| content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        (join(true), join(false))
    }

    /// The TUI is the operator's own surface, so its turn reads all of memory:
    /// every note, scoped to a conversation or not, reaches the model, and the
    /// prompt carries `USER.md` and `MEMORY.md`.
    #[tokio::test]
    async fn the_tui_turn_door_reads_all_of_memory() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        std::fs::write(
            workspace.path().join("USER.md"),
            "# User\nprofile-canary-41c9",
        )
        .unwrap();
        std::fs::write(
            workspace.path().join("MEMORY.md"),
            "# Memory\nnotes-canary-52d0",
        )
        .unwrap();
        let sqlite = crate::memory::SqliteMemory::new(workspace.path()).unwrap();
        for (key, word, scope) in [
            ("shared_note", "saffronquartz", None),
            ("session_note", "juniperquartz", Some("tui:s1")),
            ("other_note", "marigoldquartz", Some("tui:other")),
        ] {
            sqlite
                .store(
                    key,
                    &format!("A note about the lantern: {word}"),
                    crate::memory::MemoryCategory::Core,
                    scope,
                )
                .await
                .unwrap();
        }
        let mem: Arc<dyn Memory> = Arc::new(sqlite);
        let provider = ScriptedChat::new(vec![]);
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let agent = Agent::builder()
            .provider(Box::new(Arc::clone(&provider)))
            .tools(vec![])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .memory_loader(Box::new(
                crate::agent::memory_loader::DefaultMemoryLoader::new(5, 0.0),
            ))
            .workspace_dir(workspace.path().to_path_buf())
            .build()
            .expect("agent builder should succeed");

        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let handle = tokio::spawn(TuiAgentActor::new(Some(agent), req_rx, events_tx).run());
        req_tx
            .send(TurnRequest::Submit {
                text: "what about the lantern".into(),
                conversation_id: Some("tui:s1".into()),
            })
            .await
            .unwrap();
        while let Ok(Some(ev)) = timeout(Duration::from_secs(5), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                break;
            }
        }
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;

        let (system, rest) = texts(&provider);
        for word in ["saffronquartz", "juniperquartz", "marigoldquartz"] {
            assert!(rest.contains(word), "{word} is missing:\n{rest}");
        }
        assert!(system.contains("profile-canary-41c9"), "{system}");
        assert!(system.contains("notes-canary-52d0"), "{system}");
    }
    /// A resumed session seeds the real system prompt and its conversational
    /// turns; the screen notes and error lines it holds never reach the model.
    #[tokio::test]
    async fn a_resumed_session_seeds_the_system_prompt_and_only_its_turns() {
        use crate::sessions::{turns_for_history, Message};
        let row = |role: &str, content: &str| Message {
            id: 0,
            session_id: "s1".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: None,
            timestamp: 0,
        };
        let stored = vec![
            row("note", "Resumed session 12176d39 (4 messages)"),
            row("user", "where is the lantern"),
            row("assistant", "✗ provider timed out"),
            row("system", "Removed 1 message(s)."),
            row("assistant", "under the stairs"),
            row(
                "system",
                "[Compacted summary of earlier conversation]\n\nwe talked about keys",
            ),
            row("user", "and the key"),
        ];
        let provider = ScriptedChat::new(vec![]);
        let mut agent = build_test_agent_with_provider(Box::new(Arc::clone(&provider)));
        agent
            .restore_history(&turns_for_history(&stored))
            .expect("restore history");

        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, mut events_rx) = mpsc::channel(32);
        let handle = tokio::spawn(TuiAgentActor::new(Some(agent), req_rx, events_tx).run());
        req_tx
            .send(TurnRequest::Submit {
                text: "one more".into(),
                conversation_id: None,
            })
            .await
            .unwrap();
        while let Ok(Some(ev)) = timeout(Duration::from_secs(5), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                break;
            }
        }
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;

        let seen = provider.seen.lock().expect("seen mutex").clone();
        let request = seen.first().expect("one request reached the provider");
        assert_eq!(
            request.first().map(|(role, _)| role.as_str()),
            Some("system"),
            "the request must open with the system prompt: {request:?}"
        );
        assert!(
            request[0].1.len() > 100,
            "the first system message must be the real prompt, not a note: {:?}",
            request[0]
        );
        let all = request
            .iter()
            .map(|(_, c)| c.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for note in ["Resumed session", "Removed 1", "provider timed out"] {
            assert!(!all.contains(note), "{note} reached the model:\n{all}");
        }
        for kept in [
            "where is the lantern",
            "under the stairs",
            "and the key",
            "we talked about keys",
        ] {
            assert!(all.contains(kept), "{kept} is missing:\n{all}");
        }
    }

    /// A `Reload` whose config cannot build an agent reports `ReloadFailed` to
    /// the TUI, and the agent it already had keeps the conversation: the next
    /// prompt is still answered under the old config.
    #[tokio::test]
    async fn failed_rebuild_reports_reload_failed_and_the_old_agent_still_answers() {
        use crate::agent::door_test_support::DoorFixture;
        let fixture = DoorFixture::start().await;
        let agent = Agent::from_config(&fixture.config)
            .await
            .expect("the agent builds");

        let mut broken = fixture.config.clone();
        broken.default_provider = Some("nosuch-provider".to_string());

        let (req_tx, req_rx) = mpsc::channel(16);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let handle = tokio::spawn(TuiAgentActor::new(Some(agent), req_rx, events_tx).run());
        req_tx
            .send(TurnRequest::Reload(Box::new(broken)))
            .await
            .unwrap();

        let mut reported = None;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(5), events_rx.recv()).await {
            if let AgentEvent::ReloadFailed { error } = ev {
                reported = Some(error);
                break;
            }
        }
        let error = reported.expect("the failed rebuild is reported, not only logged");
        assert!(
            error.contains("nosuch-provider"),
            "the report names the provider that could not be built: {error}"
        );

        req_tx
            .send(TurnRequest::Submit {
                text: "still there?".into(),
                conversation_id: None,
            })
            .await
            .unwrap();
        let mut answered = false;
        while let Ok(Some(ev)) = timeout(Duration::from_secs(5), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                answered = true;
                break;
            }
        }
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;

        assert!(
            answered,
            "the previous agent still answers after a failed rebuild"
        );
        fixture.llm.wait_for_requests(1).await;
    }

    /// Two exchanges, then `Reload` (what `/model` sends), then a message: the
    /// new agent keeps both exchanges, and its system prompt is rebuilt from
    /// the reloaded config.
    #[tokio::test]
    async fn reload_keeps_the_history_and_rebuilds_the_system_prompt() {
        use crate::agent::door_test_support::DoorFixture;
        let fixture = DoorFixture::start().await;
        let agent = Agent::from_config(&fixture.config)
            .await
            .expect("the agent builds");
        let mut agent = agent;
        agent
            .restore_history(&turns(&[
                ("user", "first question"),
                ("assistant", "first answer"),
                ("user", "second question"),
                ("assistant", "second answer"),
            ]))
            .expect("restore history");

        let (req_tx, req_rx) = mpsc::channel(16);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let handle = tokio::spawn(TuiAgentActor::new(Some(agent), req_rx, events_tx).run());
        req_tx
            .send(TurnRequest::Reload(Box::new(fixture.config.clone())))
            .await
            .unwrap();
        while let Ok(Some(ev)) = timeout(Duration::from_secs(5), events_rx.recv()).await {
            if matches!(ev, AgentEvent::ReloadComplete { .. }) {
                break;
            }
        }
        req_tx
            .send(TurnRequest::Submit {
                text: "third question".into(),
                conversation_id: None,
            })
            .await
            .unwrap();
        while let Ok(Some(ev)) = timeout(Duration::from_secs(5), events_rx.recv()).await {
            if matches!(ev, AgentEvent::Done { .. }) {
                break;
            }
        }
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;

        fixture.llm.wait_for_requests(1).await;
        let shape: Vec<(String, String)> = fixture.llm.messages_for_test().into_iter().collect();
        let roles: Vec<&str> = shape.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "user", "assistant", "user"],
            "{shape:?}"
        );
        let conversation: Vec<&str> = shape[1..].iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(
            conversation,
            [
                "first question",
                "first answer",
                "second question",
                "second answer",
                "third question",
            ],
            "every exchange must survive the reload, in order"
        );
        assert!(
            shape[0].1.len() > 100,
            "the system prompt is rebuilt from the reloaded config: {:?}",
            shape[0]
        );
    }

    /// Runs `requests` through a fresh actor over `agent`, waits for every turn
    /// to finish, and returns what the provider was sent for each request.
    async fn run_requests(
        agent: Agent,
        provider: &Arc<ScriptedChat>,
        requests: Vec<TurnRequest>,
    ) -> Vec<Vec<(String, String)>> {
        let (req_tx, req_rx) = mpsc::channel(16);
        let (events_tx, mut events_rx) = mpsc::channel(64);
        let handle = tokio::spawn(TuiAgentActor::new(Some(agent), req_rx, events_tx).run());
        let turns = requests
            .iter()
            .filter(|r| matches!(r, TurnRequest::Submit { .. }))
            .count();
        for request in requests {
            req_tx.send(request).await.unwrap();
        }
        let mut done = 0;
        while done < turns {
            match timeout(Duration::from_secs(5), events_rx.recv()).await {
                Ok(Some(AgentEvent::Done { .. })) => done += 1,
                Ok(Some(_)) => {}
                _ => panic!("a turn did not finish"),
            }
        }
        drop(req_tx);
        let _ = timeout(Duration::from_secs(1), handle).await;
        provider.seen.lock().expect("seen mutex").clone()
    }

    fn submit(text: &str) -> TurnRequest {
        TurnRequest::Submit {
            text: text.into(),
            conversation_id: None,
        }
    }

    fn turns(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(r, c)| (r.to_string(), c.to_string()))
            .collect()
    }

    /// `SetHistory` replaces the history the next request carries: the old
    /// turns are gone, the new ones are in order, and the real system prompt
    /// leads the request.
    #[tokio::test]
    async fn set_history_replaces_the_history_before_the_next_prompt() {
        let provider = ScriptedChat::new(vec![]);
        let mut agent = build_test_agent_with_provider(Box::new(Arc::clone(&provider)));
        agent
            .restore_history(&turns(&[
                ("user", "old question"),
                ("assistant", "old answer"),
            ]))
            .expect("restore history");

        let seen = run_requests(
            agent,
            &provider,
            vec![
                TurnRequest::SetHistory(turns(&[
                    ("user", "new question"),
                    ("assistant", "new answer"),
                ])),
                submit("next"),
            ],
        )
        .await;

        let request = seen.first().expect("one request reached the provider");
        let shape: Vec<(&str, &str)> = request
            .iter()
            .map(|(r, c)| (r.as_str(), c.as_str()))
            .collect();
        assert_eq!(shape[0].0, "system");
        assert!(shape[0].1.len() > 100, "the prompt must be real: {shape:?}");
        assert_eq!(
            shape[1..],
            [
                ("user", "new question"),
                ("assistant", "new answer"),
                ("user", "next"),
            ],
            "{shape:?}"
        );
    }

    /// Two exchanges, then `/new` (an empty reseed), then a message: the request
    /// is the system prompt and the new message, and none of the old exchanges.
    #[tokio::test]
    async fn after_new_the_request_is_the_system_prompt_and_the_new_message() {
        let provider = ScriptedChat::new(vec![]);
        let agent = build_test_agent_with_provider(Box::new(Arc::clone(&provider)));

        let seen = run_requests(
            agent,
            &provider,
            vec![
                TurnRequest::SetHistory(turns(&[
                    ("user", "first question"),
                    ("assistant", "first answer"),
                    ("user", "second question"),
                    ("assistant", "second answer"),
                ])),
                // The same channel carries `/new`, so it is ordered behind
                // the history it replaces.
                TurnRequest::SetHistory(vec![]),
                submit("a new message"),
            ],
        )
        .await;

        let request = seen.first().expect("one request reached the provider");
        let shape: Vec<(&str, &str)> = request
            .iter()
            .map(|(r, c)| (r.as_str(), c.as_str()))
            .collect();
        assert_eq!(shape.len(), 2, "{shape:?}");
        assert_eq!(shape[0].0, "system");
        assert_eq!(shape[1], ("user", "a new message"));
    }

    /// Empty turns leave the fresh-session shape: the real system prompt and
    /// then the new message, with nothing before it.
    #[tokio::test]
    async fn set_history_with_no_turns_leaves_only_the_system_prompt() {
        let provider = ScriptedChat::new(vec![]);
        let mut agent = build_test_agent_with_provider(Box::new(Arc::clone(&provider)));
        agent
            .restore_history(&turns(&[
                ("user", "old question"),
                ("assistant", "old answer"),
            ]))
            .expect("restore history");

        let seen = run_requests(
            agent,
            &provider,
            vec![TurnRequest::SetHistory(vec![]), submit("fresh")],
        )
        .await;

        let request = seen.first().expect("one request reached the provider");
        let shape: Vec<&str> = request.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(shape, ["system", "user"], "{request:?}");
        assert!(request[0].1.len() > 100, "{request:?}");
        assert_eq!(request[1].1, "fresh");
    }
}
