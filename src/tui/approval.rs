//! Tool approval for the TUI: a tool on `always_ask` prompts in the approval
//! pane, at every autonomy level, as it does in the web console.
//!
//! The TUI agent used to carry no `ApprovalManager`, so the only prompts it
//! could raise came from tools that ask for themselves. Under `Full` the `pty`
//! and `ssh` tools skip their own prompt, which left `always_ask` unread on
//! this surface.
//!
//! The manager here gates `always_ask` and nothing else: the TUI never prompted
//! for a tool outside the list, and the tools keep the gates they carry. The
//! decision is `ApprovalManager::needs_approval`, the same one every other door
//! makes. The question reaches the user through the registry the pane already
//! drains, so there is one pane and one set of keys.

use std::sync::Arc;

use crate::agent::Agent;
use crate::approval::{
    ApprovalBackend, ApprovalManager, ApprovalRequest, ApprovalResponse, Verdict,
};
use crate::config::AutonomyConfig;
use crate::security::{Decision, SecurityPolicy, SessionScope};

/// Puts a gated tool call in front of the user through the approval pane.
pub(crate) struct TuiApprovalBackend {
    security: Arc<SecurityPolicy>,
}

impl TuiApprovalBackend {
    pub(crate) fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait::async_trait]
impl ApprovalBackend for TuiApprovalBackend {
    async fn decide(&self, mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse {
        self.decide_verdict(mgr, request).await.response
    }

    async fn decide_verdict(&self, _mgr: &ApprovalManager, request: &ApprovalRequest) -> Verdict {
        // No registry means no pane to ask in. Refuse rather than run a tool
        // the operator asked to be consulted about.
        let Some(pending) = self.security.pending() else {
            return Verdict::no_approver();
        };
        let (channel, reply_target) = crate::security::current_turn_scope();
        // `always_ask` means ask every time, so an answer holds for this call
        // only. That scope also keeps the pane from writing the tool's name
        // into the shell command allowlist, which is what an answer to a
        // command request does.
        let decision = pending
            .request_decision_in_scope(
                uuid::Uuid::new_v4(),
                request.tool_name.clone(),
                // Every argument in full. The summary the other surfaces use
                // cuts a value at 80 characters, which can hide the tail of the
                // command being approved.
                request.arguments.to_string(),
                channel,
                reply_target,
                SessionScope::HighRisk,
            )
            .await;
        match decision {
            Decision::Deny => Verdict::answered(ApprovalResponse::No),
            // The pane shows every argument, so a yes covers the whole call.
            Decision::Once | Decision::Session | Decision::Persist => {
                Verdict::answered_with_full_args(ApprovalResponse::Yes)
            }
        }
    }
}

/// Gate the TUI agent's `always_ask` tools through the approval pane. Called
/// wherever the TUI builds an agent, so a rebuilt agent keeps the gate.
pub(crate) fn attach_tool_approval(agent: &mut Agent, autonomy: &AutonomyConfig) {
    let Some(security) = agent.security() else {
        // `Agent::from_config` always attaches a policy, so this is a bare
        // agent. Say so: a gate that is silently absent reads as one that holds.
        tracing::error!(
            target: "tui",
            "agent has no security policy; `always_ask` is not enforced"
        );
        return;
    };
    // The live policy, not the config snapshot: a level or list change applies
    // to the turn in flight, before the agent is rebuilt.
    let manager = ApprovalManager::from_config(autonomy)
        .with_policy(Arc::clone(&security))
        .gating_only_always_ask()
        // The shell tool asks about each command itself.
        .leaving_to_tool_gate("shell");
    agent.set_approval(
        Some(Arc::new(manager)),
        Some(Arc::new(TuiApprovalBackend::new(security))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::dispatcher::XmlToolDispatcher;
    use crate::memory::Memory;
    use crate::observability::Observer;
    use crate::providers::{ChatRequest, ChatResponse, Provider};
    use crate::security::{AutonomyLevel, PendingApprovals};
    use crate::tools::{Tool, ToolResult};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Answers with a tool call on the first request and plain text after.
    struct ToolCallThenDone {
        tool: &'static str,
        asked: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for ToolCallThenDone {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("done".to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            let text = if self.asked.fetch_add(1, Ordering::SeqCst) == 0 {
                format!(
                    "<tool_call>\n{{\"name\": \"{}\", \"arguments\": {{\"command\": \"ls\"}}}}\n</tool_call>",
                    self.tool
                )
            } else {
                "done".to_string()
            };
            Ok(ChatResponse {
                usage: None,
                text: Some(text),
                tool_calls: vec![],
            })
        }
    }

    /// A stand-in for a tool: it counts its runs and runs nothing. With
    /// `asks_itself` it gates the way `pty` and `ssh` do, which is to ask the
    /// registry unless the call is already approved or autonomy is `Full`.
    struct StandInTool {
        name: &'static str,
        runs: Arc<AtomicUsize>,
        asks_itself: Option<Arc<SecurityPolicy>>,
    }

    #[async_trait::async_trait]
    impl Tool for StandInTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "stand-in"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            if let Some(security) = &self.asks_itself {
                if security.effective_autonomy() != AutonomyLevel::Full
                    && !crate::security::call_already_approved()
                {
                    let decision = security
                        .pending()
                        .expect("the registry is attached")
                        .request_decision_in(
                            uuid::Uuid::new_v4(),
                            "tool-gate",
                            "ls",
                            String::new(),
                            String::new(),
                        )
                        .await;
                    if decision == Decision::Deny {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("denied".into()),
                        });
                    }
                }
            }
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult {
                success: true,
                output: "ran".into(),
                error: None,
            })
        }
    }

    struct Run {
        /// Basenames of the requests the pane would have shown, in order.
        prompts: Vec<String>,
        /// How many times the tool body ran.
        runs: usize,
    }

    /// Drives one TUI turn in which the model calls `tool`. A responder plays
    /// the pane: it records every request and answers it with `answer`.
    /// `registry` false leaves the policy without a registry to ask in.
    async fn drive_turn(
        tool: &'static str,
        autonomy: AutonomyConfig,
        asks_itself: bool,
        answer: Decision,
        registry: bool,
    ) -> Run {
        drive_turn_with(tool, autonomy, asks_itself, answer, registry, false).await
    }

    /// A backend like the console's, the channel relay's and the CLI's: its
    /// prompt shows a summary that cuts long values, so a yes does not cover
    /// the full call.
    struct SummaryBackend;

    #[async_trait::async_trait]
    impl ApprovalBackend for SummaryBackend {
        async fn decide(
            &self,
            _mgr: &ApprovalManager,
            _request: &ApprovalRequest,
        ) -> ApprovalResponse {
            ApprovalResponse::Yes
        }
    }

    /// As [`drive_turn`]; with `summary_backend` the outer gate asks through
    /// [`SummaryBackend`] instead of the pane.
    async fn drive_turn_with(
        tool: &'static str,
        autonomy: AutonomyConfig,
        asks_itself: bool,
        answer: Decision,
        registry: bool,
        summary_backend: bool,
    ) -> Run {
        let dir = tempfile::tempdir().expect("tempdir");
        let security = Arc::new(SecurityPolicy::from_config(&autonomy, dir.path()));
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_secs(5))));
        if registry {
            security.set_pending(approvals.clone());
        }

        let prompts = Arc::new(Mutex::new(Vec::new()));
        let mut rx = approvals.subscribe();
        let responder = tokio::spawn({
            let prompts = prompts.clone();
            let approvals = approvals.clone();
            async move {
                while let Ok(request) = rx.recv().await {
                    prompts.lock().unwrap().push(request.basename.clone());
                    approvals.resolve(request.id, answer);
                }
            }
        });

        let runs = Arc::new(AtomicUsize::new(0));
        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let memory: Arc<dyn Memory> =
            Arc::from(crate::memory::create_memory(&memory_cfg, dir.path(), None).expect("memory"));
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(Box::new(ToolCallThenDone {
                tool,
                asked: AtomicUsize::new(0),
            }))
            .tools(vec![Box::new(StandInTool {
                name: tool,
                runs: runs.clone(),
                asks_itself: asks_itself.then(|| security.clone()),
            })])
            .memory(memory)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(dir.path().to_path_buf())
            .security(security.clone())
            .build()
            .expect("agent builds");
        agent.set_surface("tui");
        attach_tool_approval(&mut agent, &autonomy);
        if summary_backend {
            let manager = ApprovalManager::from_config(&autonomy)
                .with_policy(security.clone())
                .gating_only_always_ask();
            agent.set_approval(Some(Arc::new(manager)), Some(Arc::new(SummaryBackend)));
        }

        tokio::time::timeout(Duration::from_secs(10), agent.turn("go"))
            .await
            .expect("the turn finishes")
            .expect("the turn succeeds");
        responder.abort();

        let seen = prompts.lock().unwrap().clone();
        Run {
            prompts: seen,
            runs: runs.load(Ordering::SeqCst),
        }
    }

    fn autonomy(level: AutonomyLevel, always_ask: &[&str]) -> AutonomyConfig {
        AutonomyConfig {
            level,
            always_ask: always_ask.iter().map(|t| (*t).to_string()).collect(),
            auto_approve: vec![],
            ..AutonomyConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_always_ask_tool_prompts_in_the_pane_under_full_autonomy() {
        let run = drive_turn(
            "pty",
            autonomy(AutonomyLevel::Full, &["pty"]),
            true,
            Decision::Once,
            true,
        )
        .await;
        assert_eq!(run.prompts, vec!["pty".to_string()]);
        assert_eq!(run.runs, 1, "an approved call runs");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_outside_always_ask_runs_without_a_prompt_under_full_autonomy() {
        let run = drive_turn(
            "pty",
            autonomy(AutonomyLevel::Full, &[]),
            true,
            Decision::Once,
            true,
        )
        .await;
        assert!(run.prompts.is_empty(), "{:?}", run.prompts);
        assert_eq!(run.runs, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_denied_always_ask_call_does_not_run() {
        let run = drive_turn(
            "pty",
            autonomy(AutonomyLevel::Full, &["pty"]),
            true,
            Decision::Deny,
            true,
        )
        .await;
        assert_eq!(run.prompts, vec!["pty".to_string()]);
        assert_eq!(run.runs, 0, "a refused call must not run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_always_ask_call_is_refused_when_there_is_no_pane_to_ask_in() {
        let run = drive_turn(
            "pty",
            autonomy(AutonomyLevel::Full, &["pty"]),
            false,
            Decision::Once,
            false,
        )
        .await;
        assert!(run.prompts.is_empty());
        assert_eq!(
            run.runs, 0,
            "nobody could be asked, so the call must not run"
        );
    }

    /// A human's Deny in the pane ends the turn: the model is not asked again,
    /// so it cannot try the same command — or an alternative one — behind the
    /// human's back. The shell's own gate (Layer B) is the prompt the pane
    /// shows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_denied_pane_prompt_ends_the_turn() {
        use crate::agent::deny_test_support::{answer_first, turn_agent, CountingToolProvider};

        let dir = tempfile::tempdir().expect("tempdir");
        let security = Arc::new(SecurityPolicy::from_config(
            &autonomy(AutonomyLevel::Supervised, &[]),
            dir.path(),
        ));
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_secs(5))));
        security.set_pending(approvals.clone());

        let (provider, requests) = CountingToolProvider::new("shell", r#"{"command":"probe"}"#);
        let mut agent = turn_agent(
            Box::new(provider),
            vec![Box::new(crate::tools::ShellTool::new(
                Arc::clone(&security),
                Arc::new(crate::runtime::NativeRuntime::new()),
            ))],
            Arc::clone(&security),
            dir.path(),
        );
        agent.set_surface("tui");
        // The pane answers the request the shell registered.
        let responder = answer_first(approvals, Decision::Deny);

        let token = tokio_util::sync::CancellationToken::new();
        agent
            .turn_streaming("go", None, Some(token))
            .await
            .expect("the turn ends");

        responder.await.expect("the pane answered");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "a Deny must end the turn: the model was asked again"
        );
    }

    /// Under `Supervised` the `pty` and `ssh` tools also ask for themselves.
    /// The user must see one prompt for the call, not two.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervised_always_ask_call_prompts_exactly_once() {
        let run = drive_turn(
            "pty",
            autonomy(AutonomyLevel::Supervised, &["pty"]),
            true,
            Decision::Once,
            true,
        )
        .await;
        assert_eq!(run.prompts, vec!["pty".to_string()], "one prompt per call");
        assert_eq!(run.runs, 1);
    }

    /// The shell tool has its own command gates. A named `always_ask` entry for
    /// it, or an entry for another tool, must not put a tool-level prompt in
    /// front of them. This is the console's behavior for the same values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_named_shell_entry_adds_no_tool_level_prompt() {
        for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            for always_ask in [&["shell"][..], &["pty"][..], &[][..]] {
                let run = drive_turn(
                    "shell",
                    autonomy(level, always_ask),
                    false,
                    Decision::Once,
                    true,
                )
                .await;
                assert!(
                    run.prompts.is_empty(),
                    "{level:?} {always_ask:?}: {:?}",
                    run.prompts
                );
                assert_eq!(run.runs, 1, "{level:?} {always_ask:?}");
            }
        }
    }

    /// The `*` wildcard (the Manual preset) forces every tool to ask, `shell`
    /// included, exactly as in the console.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wildcard_makes_shell_ask_before_the_shell_tool_gates_itself() {
        for level in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
            let run = drive_turn(
                "shell",
                autonomy(level, &["*"]),
                false,
                Decision::Once,
                true,
            )
            .await;
            assert_eq!(run.prompts, vec!["shell".to_string()], "{level:?}");
            assert_eq!(run.runs, 1, "{level:?}");
        }
    }

    /// An outer yes that did not show the full call (the summary cuts a value
    /// at 80 characters) must not stand in for the tool's own prompt, which
    /// shows the whole command.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_yes_from_a_backend_that_cut_the_arguments_leaves_the_tools_own_prompt() {
        let run = drive_turn_with(
            "pty",
            autonomy(AutonomyLevel::Supervised, &["pty"]),
            true,
            Decision::Once,
            true,
            true,
        )
        .await;
        assert_eq!(
            run.prompts,
            vec!["tool-gate".to_string()],
            "the tool still asks for itself"
        );
        assert_eq!(run.runs, 1);
    }

    /// Only the list is gated. `Supervised` does not start to prompt for the
    /// tools the TUI has always left to their own gates.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_supervised_tool_outside_always_ask_does_not_prompt() {
        let run = drive_turn(
            "file_write",
            autonomy(AutonomyLevel::Supervised, &["pty"]),
            false,
            Decision::Once,
            true,
        )
        .await;
        assert!(run.prompts.is_empty(), "{:?}", run.prompts);
        assert_eq!(run.runs, 1);
    }

    /// The user approves what the pane shows, so it must show all of it. The
    /// summary other surfaces use cuts a value at 80 characters.
    #[tokio::test]
    async fn the_pane_request_carries_every_argument_in_full() {
        let dir = tempfile::tempdir().expect("tempdir");
        let security = Arc::new(SecurityPolicy::from_config(
            &AutonomyConfig::default(),
            dir.path(),
        ));
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_secs(5))));
        security.set_pending(approvals.clone());
        let backend = TuiApprovalBackend::new(security);
        let manager = ApprovalManager::from_config(&AutonomyConfig::default());
        let tail = "tail-of-a-long-command";
        let command = format!("{}{tail}", "x".repeat(200));
        let request = ApprovalRequest {
            tool_name: "pty".into(),
            arguments: serde_json::json!({ "action": "start", "command": command }),
        };
        let decision = tokio::spawn(async move { backend.decide(&manager, &request).await });

        let mut shown = Vec::new();
        for _ in 0..500 {
            shown = approvals.list();
            if !shown.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let request = shown.first().expect("the request is registered");
        assert!(
            request.full_command.contains(tail),
            "the end of the command is missing: {}",
            request.full_command
        );
        approvals.resolve(request.id, Decision::Deny);
        assert_eq!(decision.await.unwrap(), ApprovalResponse::No);
    }

    /// Every place the TUI builds an agent must attach the gate, or a rebuilt
    /// agent (a reload after `/autonomy`, a wizard run) would run `always_ask`
    /// tools without asking. The calls sit beside `set_surface`, so count both.
    #[test]
    fn every_tui_agent_construction_attaches_the_tool_gate() {
        // Assembled at runtime so the counts below do not include this test.
        let surface = format!("{}(\"tui\")", "set_surface");
        let attach = format!("{}(", "attach_tool_approval");
        for (name, src) in [
            ("app.rs", include_str!("app.rs")),
            ("async_bridge.rs", include_str!("async_bridge.rs")),
        ] {
            let surfaces = src.matches(surface.as_str()).count();
            let attaches = src.matches(attach.as_str()).count();
            assert!(surfaces > 0, "{name}: no agent construction found");
            assert_eq!(
                surfaces, attaches,
                "{name}: each agent built for the TUI must attach the tool gate"
            );
        }
    }
}
