//! Shared fixture for the tests that ask what a human's Deny does to a turn.
//!
//! The fixture counts the provider requests a turn made. One request and then
//! silence is a turn that ended; a second request is a turn the model was
//! allowed to carry on — which is exactly what a Deny has to prevent.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::agent::Agent;
use crate::memory::Memory;
use crate::observability::Observer;
use crate::providers::{ChatRequest, ChatResponse, Provider, ToolCall};
use crate::security::SecurityPolicy;
use crate::tools::Tool;

/// A provider that asks for one tool call on its first request and answers in
/// text on every later one, counting the requests it served.
pub(crate) struct CountingToolProvider {
    tool: &'static str,
    arguments: String,
    requests: Arc<AtomicUsize>,
}

impl CountingToolProvider {
    /// The provider, and the counter the test reads.
    pub(crate) fn new(tool: &'static str, arguments: &str) -> (Self, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        (
            Self {
                tool,
                arguments: arguments.to_string(),
                requests: Arc::clone(&requests),
            },
            requests,
        )
    }
}

#[async_trait::async_trait]
impl Provider for CountingToolProvider {
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
        let served = self.requests.fetch_add(1, Ordering::SeqCst);
        let tool_calls = if served == 0 {
            vec![ToolCall {
                id: "call-1".to_string(),
                name: self.tool.to_string(),
                arguments: self.arguments.clone(),
            }]
        } else {
            Vec::new()
        };
        Ok(ChatResponse {
            usage: None,
            text: Some("done".to_string()),
            tool_calls,
        })
    }
}

/// An agent for one turn: `provider`, `tools`, and the policy the caller
/// attached an approval registry to. The workspace is the temp dir the caller
/// keeps alive for the length of the test.
pub(crate) fn turn_agent(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    security: Arc<SecurityPolicy>,
    workspace: &std::path::Path,
) -> Agent {
    let memory_cfg = crate::config::MemoryConfig {
        backend: "none".into(),
        ..crate::config::MemoryConfig::default()
    };
    let memory: Arc<dyn Memory> = Arc::from(
        crate::memory::create_memory(&memory_cfg, workspace, None)
            .expect("the memory backend builds"),
    );
    let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(memory)
        .observer(observer)
        .tool_dispatcher(Box::new(crate::agent::dispatcher::XmlToolDispatcher))
        .workspace_dir(workspace.to_path_buf())
        .security(security)
        .build()
        .expect("the agent builds")
}

/// Answer the first request the registry holds with `decision`, from another
/// task — the tool is waiting on the answer, so the answer has to come from
/// somewhere else. Returns the handle so the test can await it and see a
/// failure instead of a timeout.
pub(crate) fn answer_first(
    pending: Arc<crate::security::PendingApprovals>,
    decision: crate::security::Decision,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        for _ in 0..300 {
            if let Some(request) = pending.list().into_iter().next() {
                pending.resolve(request.id, decision);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the tool never registered an approval request");
    })
}
