//! End-to-end: TuiAgentActor + real Agent + scripted provider.
#![cfg(feature = "tui")]

use std::sync::Arc;

use async_trait::async_trait;
use rantaiclaw::agent::agent::Agent;
use rantaiclaw::agent::dispatcher::XmlToolDispatcher;
use rantaiclaw::agent::events::AgentEvent;
use rantaiclaw::memory::Memory;
use rantaiclaw::observability::NoopObserver;
use rantaiclaw::observability::Observer;
use rantaiclaw::providers::{ChatRequest, ChatResponse, Provider};
use rantaiclaw::tui::async_bridge::{TuiAgentActor, TurnRequest};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

struct StaticProvider(&'static str);

#[async_trait]
impl Provider for StaticProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(self.0.into())
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            text: Some(self.0.into()),
            tool_calls: vec![],
        })
    }
}

fn build_agent(response: &'static str) -> Agent {
    let memory_cfg = rantaiclaw::config::MemoryConfig {
        backend: "none".into(),
        ..rantaiclaw::config::MemoryConfig::default()
    };
    let mem: Arc<dyn Memory> = Arc::from(
        rantaiclaw::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
            .expect("memory creation should succeed with valid config"),
    );
    let observer: Arc<dyn Observer> = Arc::from(NoopObserver {});

    Agent::builder()
        .provider(Box::new(StaticProvider(response)))
        .tools(vec![])
        .memory(mem)
        .observer(observer)
        .tool_dispatcher(Box::new(XmlToolDispatcher))
        .workspace_dir(std::path::PathBuf::from("/tmp"))
        .build()
        .expect("agent builder should succeed with valid config")
}

#[tokio::test]
async fn end_to_end_turn_emits_chunks_and_done() {
    let agent = build_agent("hello world from integration test");

    let (req_tx, req_rx) = mpsc::channel(4);
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let handle = tokio::spawn(actor.run());

    req_tx.send(TurnRequest::Submit("hi".into())).await.unwrap();

    let mut chunks: Vec<String> = Vec::new();
    let mut saw_done = false;
    while let Ok(Some(ev)) = timeout(Duration::from_secs(3), events_rx.recv()).await {
        match ev {
            AgentEvent::Chunk(s) => chunks.push(s),
            AgentEvent::Done {
                final_text,
                cancelled,
            } => {
                assert_eq!(final_text, "hello world from integration test");
                assert!(!cancelled);
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(!chunks.is_empty(), "expected at least one Chunk event");
    let combined: String = chunks.into_iter().collect();
    assert!(
        combined.contains("hello"),
        "chunks should contain response text"
    );
    assert!(saw_done, "expected Done event");

    drop(req_tx);
    let _ = timeout(Duration::from_secs(1), handle).await;
}
