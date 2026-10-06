//! Comprehensive agent-loop test suite.
//!
//! Tests exercise the full `Agent.turn()` cycle with mock providers and tools,
//! covering every edge case an agentic tool loop must handle:
//!
//!   1. Simple text response (no tools)
//!   2. Single tool call → final response
//!   3. Multi-step tool chain (tool A → tool B → response)
//!   4. Max-iteration bailout
//!   5. Unknown tool name recovery
//!   6. Tool execution failure recovery
//!   7. Parallel tool dispatch
//!   8. History trimming during long conversations
//!   9. Memory auto-save round-trip
//!  10. Native vs XML dispatcher integration
//!  11. Empty / whitespace-only LLM responses
//!  12. Mixed text + tool call responses
//!  13. Multi-tool batch in a single response
//!  14. System prompt generation & tool instructions
//!  15. Context enrichment from memory loader
//!  16. ConversationMessage serialization round-trip
//!  17. Tool call with stringified JSON arguments
//!  18. Conversation history fidelity (tool call → tool result → assistant)
//!  19. Builder validation (missing required fields)
//!  20. Idempotent system prompt insertion

use crate::agent::agent::Agent;
use crate::agent::dispatcher::{
    NativeToolDispatcher, ToolDispatcher, ToolExecutionResult, XmlToolDispatcher,
};
use crate::config::{AgentConfig, MemoryConfig};
use crate::memory::{self, Memory};
use crate::observability::{NoopObserver, Observer};
use crate::providers::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ToolCall,
    ToolResultMessage,
};
use crate::tools::{Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// `(category, session_id)` tuple recorded by `RecordingMemory` for each
/// `store` call. Extracted to a type alias to keep `RecordingMemory::stored`
/// readable.
type StoredMemoryRecord = (String, Option<String>);

// ═══════════════════════════════════════════════════════════════════════════
// Test Helpers — Mock Provider, Mock Tool, Mock Memory
// ═══════════════════════════════════════════════════════════════════════════

/// A mock LLM provider that returns pre-scripted responses in order.
/// When the queue is exhausted it returns a simple "done" text response.
struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
    /// Records every request for assertion.
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("fallback".into())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        self.requests
            .lock()
            .unwrap()
            .push(request.messages.to_vec());

        let mut guard = self.responses.lock().unwrap();
        if guard.is_empty() {
            return Ok(ChatResponse {
                usage: None,
                text: Some("done".into()),
                tool_calls: vec![],
            });
        }
        Ok(guard.remove(0))
    }
}

/// A [`ScriptedProvider`] the test keeps a handle to, so it can read the
/// requests after the agent has taken ownership of its provider.
struct SharedProvider(Arc<ScriptedProvider>);

#[async_trait]
impl Provider for SharedProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> Result<String> {
        self.0
            .chat_with_system(system_prompt, message, model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> Result<ChatResponse> {
        self.0.chat(request, model, temperature).await
    }
}

/// A mock provider that always returns an error.
struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        anyhow::bail!("provider error")
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        anyhow::bail!("provider error")
    }
}

/// A simple echo tool that returns its arguments as output.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes the input"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)")
            .to_string();
        Ok(ToolResult {
            success: true,
            output: msg,
            error: None,
        })
    }
}

/// A tool that always fails execution.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail"
    }

    fn description(&self) -> &str {
        "Always fails"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some("intentional failure".into()),
        })
    }
}

/// A tool that panics (tests error propagation).
struct PanickingTool;

#[async_trait]
impl Tool for PanickingTool {
    fn name(&self) -> &str {
        "panicker"
    }

    fn description(&self) -> &str {
        "Panics on execution"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        anyhow::bail!("catastrophic tool failure")
    }
}

/// A tool that tracks how many times it was called.
struct CountingTool {
    count: Arc<Mutex<usize>>,
}

impl CountingTool {
    fn new() -> (Self, Arc<Mutex<usize>>) {
        let count = Arc::new(Mutex::new(0));
        (
            Self {
                count: count.clone(),
            },
            count,
        )
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "counter"
    }

    fn description(&self) -> &str {
        "Counts calls"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult> {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        Ok(ToolResult {
            success: true,
            output: format!("call #{}", *c),
            error: None,
        })
    }
}

fn make_memory() -> Arc<dyn Memory> {
    let cfg = MemoryConfig {
        backend: "none".into(),
        ..MemoryConfig::default()
    };
    Arc::from(memory::create_memory(&cfg, &std::env::temp_dir(), None).unwrap())
}

fn make_sqlite_memory() -> (Arc<dyn Memory>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = MemoryConfig {
        backend: "sqlite".into(),
        ..MemoryConfig::default()
    };
    let mem = Arc::from(memory::create_memory(&cfg, tmp.path(), None).unwrap());
    (mem, tmp)
}

fn make_observer() -> Arc<dyn Observer> {
    Arc::from(NoopObserver {})
}

fn build_agent_with(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    dispatcher: Box<dyn ToolDispatcher>,
) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(dispatcher)
        .workspace_dir(std::env::temp_dir())
        .build()
        .unwrap()
}

fn build_agent_with_config(
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    config: AgentConfig,
) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .config(config)
        .build()
        .unwrap()
}

/// Helper: create a ChatResponse with tool calls (native format).
fn tool_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        usage: None,
        text: Some(String::new()),
        tool_calls: calls,
    }
}

/// Helper: create a plain text ChatResponse.
fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        usage: None,
        text: Some(text.into()),
        tool_calls: vec![],
    }
}

/// Helper: create an XML-style tool call response.
fn xml_tool_response(name: &str, args: &str) -> ChatResponse {
    ChatResponse {
        usage: None,
        text: Some(format!(
            "<tool_call>\n{{\"name\": \"{name}\", \"arguments\": {args}}}\n</tool_call>"
        )),
        tool_calls: vec![],
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Simple text response (no tools)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_returns_text_when_no_tools_called() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("Hello world")]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("hi").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty text response from provider"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Single tool call → final response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_executes_single_tool_then_returns() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello from tool"}"#.into(),
        }]),
        text_response("I ran the tool"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("run echo").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool execution"
    );
}

/// The agent door never synthesises a `[Used tools: …]` prefix on the text it
/// stores or returns. The model's final text reaches history verbatim, and the
/// structured tool-call / tool-result rows are stored as their own
/// `ConversationMessage` variants. This is the same invariant the channel door
/// relies on; `agent::run_with_scope` is the shared code path the CLI single
/// shot, the TUI agent, the cron scheduler and the gateway chat all go through.
#[tokio::test]
async fn turn_stores_assistant_text_without_a_used_tools_prefix() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello from tool"}"#.into(),
        }]),
        text_response("I ran the tool and got back data."),
    ]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("run echo").await.unwrap();
    assert!(
        !response.starts_with("[Used tools:"),
        "the runtime synthesised a `[Used tools:` prefix on the model text: {response:?}"
    );
    assert!(
        !response.contains("[Used tools:"),
        "the runtime synthesised a `[Used tools:` substring on the model text: {response:?}"
    );

    // Walk the agent's structured history: every `assistant` chat row must
    // start with the model's own text, never a runtime-prefixed summary.
    for message in agent.history() {
        let ConversationMessage::Chat(chat) = message else {
            continue;
        };
        if chat.role != "assistant" {
            continue;
        }
        assert!(
            !chat.content.starts_with("[Used tools:"),
            "an assistant row in history carries a runtime-prefixed label: {chat:?}"
        );
        assert!(
            !chat.content.contains("[Used tools:"),
            "an assistant row in history carries the runtime's `[Used tools:` substring: {chat:?}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Multi-step tool chain (tool A → tool B → response)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multi_step_tool_chain() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        tool_response(vec![ToolCall {
            id: "tc2".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        tool_response(vec![ToolCall {
            id: "tc3".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        text_response("Done after 3 calls"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(counting_tool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("count 3 times").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after multi-step chain"
    );
    assert_eq!(*count.lock().unwrap(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Max-iteration soft-cap (force a final summary instead of erroring)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_soft_caps_at_max_iterations_with_summary() {
    // Set up `max_iters` tool-call responses, then one final text response
    // that the soft-cap path will consume when force_final_summary runs.
    // Behavior contract (since the soft-cap refactor): at max_iterations,
    // the agent does NOT return Err — it makes one tools-disabled provider
    // call so the user gets a real summary, then returns Ok with that text.
    let max_iters = 3;
    let mut responses = Vec::new();
    for i in 0..max_iters {
        responses.push(tool_response(vec![ToolCall {
            id: format!("tc{i}"),
            name: "echo".into(),
            arguments: r#"{"message": "loop"}"#.into(),
        }]));
    }
    responses.push(text_response(
        "Hit the iteration cap. Tried `echo` three times. Type /continue to extend the budget.",
    ));

    let provider = Box::new(ScriptedProvider::new(responses));

    let config = AgentConfig {
        max_tool_iterations: max_iters,
        ..AgentConfig::default()
    };

    let mut agent = build_agent_with_config(provider, vec![Box::new(EchoTool)], config);

    let response = agent
        .turn("loop forever")
        .await
        .expect("soft-cap path returns Ok with a final summary, not Err");
    assert!(
        !response.is_empty(),
        "expected a non-empty final summary at the iteration cap"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Unknown tool name recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_unknown_tool_gracefully() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "nonexistent_tool".into(),
            arguments: "{}".into(),
        }]),
        text_response("I couldn't find that tool"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("use nonexistent").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after unknown tool recovery"
    );

    // Verify the tool result mentioned "Unknown tool"
    let has_tool_result = agent.history().iter().any(|msg| match msg {
        ConversationMessage::ToolResults(results) => {
            results.iter().any(|r| r.content.contains("Unknown tool"))
        }
        _ => false,
    });
    assert!(
        has_tool_result,
        "Expected tool result with 'Unknown tool' message"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Tool execution failure recovery
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_recovers_from_tool_failure() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "fail".into(),
            arguments: "{}".into(),
        }]),
        text_response("Tool failed but I recovered"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(FailingTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("try failing tool").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool failure recovery"
    );
}

#[tokio::test]
async fn turn_recovers_from_tool_error() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "panicker".into(),
            arguments: "{}".into(),
        }]),
        text_response("I recovered from the error"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(PanickingTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("try panicking").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after tool error recovery"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Provider error propagation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_propagates_provider_error() {
    let mut agent = build_agent_with(
        Box::new(FailingProvider),
        vec![],
        Box::new(NativeToolDispatcher),
    );

    let result = agent.turn("hello").await;
    assert!(result.is_err(), "Expected provider error to propagate");
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. History trimming during long conversations
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_trims_after_max_messages() {
    let max_history = 6;
    let mut responses = vec![];
    for _ in 0..max_history + 5 {
        responses.push(text_response("ok"));
    }

    let provider = Box::new(ScriptedProvider::new(responses));
    let config = AgentConfig {
        max_history_messages: max_history,
        ..AgentConfig::default()
    };

    let mut agent = build_agent_with_config(provider, vec![], config);

    for i in 0..max_history + 5 {
        let _ = agent.turn(&format!("msg {i}")).await.unwrap();
    }

    // System prompt (1) + trimmed messages
    // Should not exceed max_history + 1 (system prompt)
    assert!(
        agent.history().len() <= max_history + 1,
        "History length {} exceeds max {} + 1 (system)",
        agent.history().len(),
        max_history,
    );

    // System prompt should always be preserved
    let first = &agent.history()[0];
    assert!(matches!(first, ConversationMessage::Chat(c) if c.role == "system"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. The agent door never writes a Conversation row on its own
// ═══════════════════════════════════════════════════════════════════════════

/// A row enters the `memories` table only because someone asked: the
/// `memory_store` tool, the CLI, the console or the TUI. This is the
/// invariant on the agent door. The agent builder opts into the auto-save
/// path that used to write here; with it on, the writer was reached on every
/// turn. The test runs one such turn, then asserts no Conversation row was
/// written. The auto-save setting is left on deliberately so a regression
/// that re-adds the writer fires this assertion immediately.
#[tokio::test]
async fn agent_turn_does_not_write_a_conversation_row() {
    let (mem, _tmp) = make_sqlite_memory();
    let provider = Box::new(ScriptedProvider::new(vec![text_response(
        "I remember everything",
    )]));

    let mut agent = Agent::builder()
        .provider(provider)
        .tools(vec![])
        .memory(mem.clone())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .build()
        .unwrap();

    crate::memory::MEMORY_VIEW
        .scope(crate::memory::MemoryView::All, async {
            let response = agent.turn("Remember this fact").await.unwrap();
            assert_eq!(response, "I remember everything");
        })
        .await;

    let all = mem.list(None, None).await.unwrap();
    let conversation_rows: Vec<_> = all
        .iter()
        .filter(|e| e.category == crate::memory::MemoryCategory::Conversation)
        .collect();
    assert!(
        conversation_rows.is_empty(),
        "the agent turn must not write a Conversation row; got {conversation_rows:?}"
    );

    // The agent's own history still holds the turn — only memory writes were
    // removed, not the in-process chat.
    assert!(
        agent.history().len() >= 2,
        "the system prompt and the user/assistant pair still sit in agent history"
    );
}

/// Two turns in a row: each must leave the Conversation count unchanged.
/// Without this, a per-turn key could write (zero rows would be a defect here
/// only because no prior rows exist) and still claim the invariant.
#[tokio::test]
async fn agent_repeated_turns_do_not_grow_conversation_rows() {
    let (mem, _tmp) = make_sqlite_memory();
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("first reply"),
        text_response("second reply"),
    ]));

    let mut agent = Agent::builder()
        .provider(provider)
        .tools(vec![])
        .memory(mem.clone())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .build()
        .unwrap();

    crate::memory::MEMORY_VIEW
        .scope(crate::memory::MemoryView::All, async {
            let _ = agent.turn("my name is rantaiclaw_user").await.unwrap();
            let _ = agent.turn("I work in the Jakarta office").await.unwrap();
        })
        .await;

    let all = mem.list(None, None).await.unwrap();
    let conversation_rows: Vec<_> = all
        .iter()
        .filter(|e| e.category == crate::memory::MemoryCategory::Conversation)
        .collect();
    assert!(
        conversation_rows.is_empty(),
        "two turns must not write any Conversation row; got {conversation_rows:?}"
    );
}

/// The headless `agent -m` door (`src/main.rs:1804`) drives
/// [`crate::agent::run`], which builds a provider from
/// `config.default_provider` and turns the agent. A row enters the
/// `memories` table only because someone asked — through the
/// `memory_store` tool, the CLI, the console or the TUI. A re-added
/// `mem.store(...)` inside `run_with_scope` (or any helper it calls)
/// shows up as a `count_after > count_before` here.
///
/// Replaces `agent::loop_::tests::headless_agent_message_writer_is_removed`,
/// a source-pinning guard that grepped `loop_.rs` for `autosave_screened(`,
/// a function that no longer exists; a re-added `mem.store(...)` would have
/// passed the old guard.
#[tokio::test]
async fn headless_door_does_not_write_a_memory_row() {
    let fixture = super::door_test_support::DoorFixture::start().await;
    let mem = crate::memory::SqliteMemory::new(&fixture.workspace).unwrap();
    let count_before = mem.count().await.unwrap();

    crate::memory::MEMORY_VIEW
        .scope(crate::memory::MemoryView::All, async {
            // `agent::run` is a large future; box it once to keep this test
            // future off the poll-loop stack (clippy::large_futures), mirroring
            // the cron scheduler and the existing door tests. The headless
            // path always passes `Some(message)`, so the REPL branch never
            // reads from the reader; `tokio::io::BufReader::new(tokio::io::stdin())`
            // is the safe default since this is a `Send` future.
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let reply = Box::pin(crate::agent::run(
                fixture.config.clone(),
                Some("hi from headless".to_string()),
                None,
                None,
                0.0,
                "cli",
                // The CLI single-shot path: stdout is the operator's
                // terminal and the wrapper `println!`s the reply.
                false,
                &mut stdin,
            ))
            .await
            .expect("the headless turn runs against the local server");
            assert_eq!(reply, "Done.");
        })
        .await;

    let count_after = mem.count().await.unwrap();
    assert_eq!(
        count_before, count_after,
        "the headless `agent -m` door must not change the memories table \
             (before={count_before}, after={count_after})"
    );
}

/// The console/TUI door drives a turn through `Agent::turn`, which the
/// console route (`src/gateway/api_v1.rs:717`) reaches when an operator
/// sends a message from the web UI. The headless door in the sibling test
/// covers the `agent -m` path that builds a full `agent::run` wrapper;
/// this test covers the in-process turn the API and the TUI send. The
/// invariant is the same: a row enters the table only when someone
/// asked. The two tests together cover the two surfaces the operator
/// can reach directly from a terminal.
///
/// Replaces `agent::loop_::tests::repl_writer_is_removed`, which had the
/// same source-pinning flaw as its headless sibling.
#[tokio::test]
async fn agent_turn_door_does_not_write_a_memory_row() {
    let (mem, _tmp) = make_sqlite_memory();
    let count_before = mem.count().await.unwrap();
    let provider = Box::new(ScriptedProvider::new(vec![text_response("done")]));

    let mut agent = Agent::builder()
        .provider(provider)
        .tools(vec![])
        .memory(mem.clone())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .build()
        .unwrap();

    crate::memory::MEMORY_VIEW
        .scope(crate::memory::MemoryView::All, async {
            let reply = agent.turn("hi from console").await.unwrap();
            assert_eq!(reply, "done");
        })
        .await;

    let count_after = mem.count().await.unwrap();
    assert_eq!(
        count_before, count_after,
        "the console/TUI turn must not change the memories table \
             (before={count_before}, after={count_after})"
    );
}

/// The stdin REPL door is the REPL else-branch of `run_with_scope`: when
/// no message is passed on the command line, the loop reads from the
/// injected reader, runs each line through
/// `run_tool_call_loop`, prints the reply, and breaks on `/quit`. Driving
/// it under test needs an injected `AsyncBufRead` so a single scripted
/// buffer can run one turn then break — the production caller passes
/// `tokio::io::stdin()` wrapped in a `BufReader`, which would block
/// forever on an empty stdin in a test. The invariant is the same as the
/// headless and agent-turn doors: a row enters the table only when
/// someone asked.
#[tokio::test]
async fn repl_door_does_not_write_a_memory_row() {
    let fixture = super::door_test_support::DoorFixture::start().await;
    let mem = crate::memory::SqliteMemory::new(&fixture.workspace).unwrap();
    let count_before = mem.count().await.unwrap();

    // Scripted input: one turn then `/quit` so the loop exits cleanly.
    let mut reader =
        tokio::io::BufReader::new(std::io::Cursor::new(b"hi from repl\n/quit\n".to_vec()));

    crate::memory::MEMORY_VIEW
        .scope(crate::memory::MemoryView::All, async {
            // `message = None` reaches the REPL else-branch. The reader is
            // the only thing that lets the loop make progress; the future
            // is boxed because `agent::run` is large.
            let reply = Box::pin(crate::agent::run(
                fixture.config.clone(),
                None,
                None,
                None,
                0.0,
                "cli",
                // REPL path prints the reply to the operator's terminal on
                // every turn. The stdout noise is fine here; the assertion
                // is on memory.
                false,
                &mut reader,
            ))
            .await
            .expect("the REPL turn runs against the local server");
            assert_eq!(
                reply, "Done.",
                "the last reply the REPL saw is the one returned: {reply}"
            );
        })
        .await;

    let count_after = mem.count().await.unwrap();
    assert_eq!(
        count_before, count_after,
        "the REPL turn must not change the memories table \
             (before={count_before}, after={count_after})"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 10. Native vs XML dispatcher integration
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn xml_dispatcher_parses_and_loops() {
    let provider = Box::new(ScriptedProvider::new(vec![
        xml_tool_response("echo", r#"{"message": "xml-test"}"#),
        text_response("XML tool completed"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(XmlToolDispatcher),
    );

    let response = agent.turn("test xml").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response from XML dispatcher"
    );
}

#[tokio::test]
async fn native_dispatcher_sends_tool_specs() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let _ = agent.turn("hi").await.unwrap();

    // NativeToolDispatcher.should_send_tool_specs() returns true
    let dispatcher = NativeToolDispatcher;
    assert!(dispatcher.should_send_tool_specs());
}

#[tokio::test]
async fn xml_dispatcher_does_not_send_tool_specs() {
    let dispatcher = XmlToolDispatcher;
    assert!(!dispatcher.should_send_tool_specs());
}

// ═══════════════════════════════════════════════════════════════════════════
// 11. Empty / whitespace-only LLM responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_empty_text_response() {
    let provider = Box::new(ScriptedProvider::new(vec![ChatResponse {
        usage: None,
        text: Some(String::new()),
        tool_calls: vec![],
    }]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

    let response = agent.turn("hi").await.unwrap();
    assert!(response.is_empty());
}

#[tokio::test]
async fn turn_handles_none_text_response() {
    let provider = Box::new(ScriptedProvider::new(vec![ChatResponse {
        usage: None,
        text: None,
        tool_calls: vec![],
    }]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

    // Should not panic — falls back to empty string
    let response = agent.turn("hi").await.unwrap();
    assert!(response.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Mixed text + tool call responses
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_preserves_text_alongside_tool_calls() {
    let provider = Box::new(ScriptedProvider::new(vec![
        ChatResponse {
            usage: None,
            text: Some("Let me check...".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "hi"}"#.into(),
            }],
        },
        text_response("Here are the results"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("check something").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty final response after mixed text+tool"
    );

    // The intermediate text must be preserved in history. The unified loop
    // keeps it on the structured `AssistantToolCalls.text` (one assistant
    // message carrying both text and tool calls — the correct provider shape),
    // rather than a separate `Chat` message; accept either.
    let has_intermediate = agent.history().iter().any(|msg| match msg {
        ConversationMessage::Chat(c) => c.role == "assistant" && c.content.contains("Let me check"),
        ConversationMessage::AssistantToolCalls { text, .. } => {
            text.as_deref().is_some_and(|t| t.contains("Let me check"))
        }
        ConversationMessage::ToolResults(_) => false,
    });
    assert!(has_intermediate, "Intermediate text should be in history");
}

// ═══════════════════════════════════════════════════════════════════════════
// 13. Multi-tool batch in a single response
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn turn_handles_multiple_tools_in_one_response() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![
            ToolCall {
                id: "tc1".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc2".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc3".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
        ]),
        text_response("All 3 done"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(counting_tool)],
        Box::new(NativeToolDispatcher),
    );

    let response = agent.turn("batch").await.unwrap();
    assert!(
        !response.is_empty(),
        "Expected non-empty response after multi-tool batch"
    );
    assert_eq!(
        *count.lock().unwrap(),
        3,
        "All 3 tools should have been called"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 14. System prompt generation & tool instructions
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn system_prompt_injected_on_first_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    assert!(agent.history().is_empty(), "History should start empty");

    let _ = agent.turn("hi").await.unwrap();

    // First message should be the system prompt
    let first = &agent.history()[0];
    assert!(
        matches!(first, ConversationMessage::Chat(c) if c.role == "system"),
        "First history entry should be system prompt"
    );
}

#[tokio::test]
async fn system_prompt_not_duplicated_on_second_turn() {
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("first"),
        text_response("second"),
    ]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let _ = agent.turn("hi").await.unwrap();
    let _ = agent.turn("hello again").await.unwrap();

    let system_count = agent
        .history()
        .iter()
        .filter(|msg| matches!(msg, ConversationMessage::Chat(c) if c.role == "system"))
        .count();
    assert_eq!(system_count, 1, "System prompt should appear exactly once");
}

// ═══════════════════════════════════════════════════════════════════════════
// 15. Conversation history fidelity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn history_contains_all_expected_entries_after_tool_loop() {
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "tool-out"}"#.into(),
        }]),
        text_response("final answer"),
    ]));

    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(NativeToolDispatcher),
    );

    let _ = agent.turn("test").await.unwrap();

    // Expected history entries:
    //   0: system prompt
    //   1: user message "test"
    //   2: AssistantToolCalls
    //   3: ToolResults
    //   4: assistant "final answer"
    let history = agent.history();
    assert!(
        history.len() >= 5,
        "Expected at least 5 history entries, got {}",
        history.len()
    );

    assert!(matches!(&history[0], ConversationMessage::Chat(c) if c.role == "system"));
    assert!(matches!(&history[1], ConversationMessage::Chat(c) if c.role == "user"));
    assert!(matches!(
        &history[2],
        ConversationMessage::AssistantToolCalls { .. }
    ));
    assert!(matches!(&history[3], ConversationMessage::ToolResults(_)));
    assert!(
        matches!(&history[4], ConversationMessage::Chat(c) if c.role == "assistant" && c.content == "final answer")
    );
}

/// Characterization gate for the loop-collapse refactor (PR2-rest): the XML
/// dispatcher must produce STRUCTURED `AssistantToolCalls` / `ToolResults`
/// history across multiple turns. That structure is load-bearing — the TUI
/// renders tool-call blocks from it (Hermes parity) and the next turn rebuilds
/// provider messages from it. Any unification of the two agent loops must keep
/// this contract; a delegation that flattens history to plain `Chat` entries
/// will fail here.
#[tokio::test]
async fn xml_dispatcher_multi_turn_preserves_structured_tool_history() {
    let provider = Box::new(ScriptedProvider::new(vec![
        xml_tool_response("echo", r#"{"message": "first"}"#),
        text_response("answer one"),
        text_response("answer two"),
    ]));
    let mut agent = build_agent_with(
        provider,
        vec![Box::new(EchoTool)],
        Box::new(XmlToolDispatcher),
    );

    let r1 = agent.turn("turn one").await.unwrap();
    assert_eq!(r1, "answer one");

    let history = agent.history();
    // The load-bearing contract: a structured AssistantToolCalls entry exists
    // (the TUI renders its tool-call block from this). The XML dispatcher feeds
    // tool *results* back as a chat message rather than the native
    // `ToolResults` variant, so we don't assert that variant here — only that
    // the structured tool-call entry survives and the turn produced output.
    assert!(
        history
            .iter()
            .any(|m| matches!(m, ConversationMessage::AssistantToolCalls { .. })),
        "expected an AssistantToolCalls entry after an XML tool call"
    );
    let len_after_1 = agent.history().len();
    assert!(
        len_after_1 >= 4,
        "expected system+user+tool-call+result+answer history, got {len_after_1}"
    );

    // Second turn still works and history keeps growing (context preserved).
    let r2 = agent.turn("turn two").await.unwrap();
    assert_eq!(r2, "answer two");
    assert!(
        agent.history().len() > len_after_1,
        "history should grow across turns"
    );
}

/// Memory mock that records the `session_id` each `store` was called with, so a
/// test can assert turn memory is written under the agent's conversation scope.
struct RecordingMemory {
    stored: Arc<Mutex<Vec<StoredMemoryRecord>>>,
}

#[async_trait]
impl Memory for RecordingMemory {
    fn name(&self) -> &str {
        "recording"
    }
    async fn store(
        &self,
        key: &str,
        _content: &str,
        _category: crate::memory::MemoryCategory,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.stored
            .lock()
            .unwrap()
            .push((key.to_string(), session_id.map(|s| s.to_string())));
        Ok(())
    }
    async fn recall(
        &self,
        _q: &str,
        _l: usize,
        _s: Option<&str>,
    ) -> Result<Vec<crate::memory::MemoryEntry>> {
        Ok(vec![])
    }
    async fn get(&self, _k: &str) -> Result<Option<crate::memory::MemoryEntry>> {
        Ok(None)
    }
    async fn list(
        &self,
        _c: Option<&crate::memory::MemoryCategory>,
        _s: Option<&str>,
    ) -> Result<Vec<crate::memory::MemoryEntry>> {
        Ok(vec![])
    }
    async fn forget(&self, _k: &str) -> Result<bool> {
        Ok(false)
    }
    async fn count(&self) -> Result<usize> {
        Ok(0)
    }
    async fn health_check(&self) -> bool {
        true
    }
}

/// End-to-end: an agent built with a `conversation_id` no longer writes turn
/// memory under any scope. With no conversation_id it would not have stored
/// globally either — auto-save is gone. The `conversation_id` itself stays on
/// the agent so any future tool the model calls can still use it.
#[tokio::test]
async fn turn_does_not_store_memory_under_conversation_scope_or_anyone() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let memory = Arc::new(RecordingMemory {
        stored: recorded.clone(),
    });
    let provider = Box::new(ScriptedProvider::new(vec![text_response("ok")]));
    let mut agent = Agent::builder()
        .provider(provider)
        .tools(vec![])
        .memory(memory)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .conversation_id(Some("telegram:123".to_string()))
        .build()
        .unwrap();

    let _ = agent.turn("hello").await.unwrap();

    let recs = recorded.lock().unwrap();
    assert!(
        recs.is_empty(),
        "the agent turn must not write any row, scoped or not; got {recs:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 16. Builder validation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn builder_fails_without_provider() {
    let result = Agent::builder()
        .tools(vec![])
        .memory(make_memory())
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::path::PathBuf::from("/tmp"))
        .build();

    assert!(result.is_err(), "Building without provider should fail");
}

// ═══════════════════════════════════════════════════════════════════════════
// 17. Multi-turn conversation maintains context
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_turn_maintains_growing_history() {
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("response 1"),
        text_response("response 2"),
        text_response("response 3"),
    ]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

    let r1 = agent.turn("msg 1").await.unwrap();
    let len_after_1 = agent.history().len();

    let r2 = agent.turn("msg 2").await.unwrap();
    let len_after_2 = agent.history().len();

    let r3 = agent.turn("msg 3").await.unwrap();
    let len_after_3 = agent.history().len();

    assert_eq!(r1, "response 1");
    assert_eq!(r2, "response 2");
    assert_eq!(r3, "response 3");

    // History should grow with each turn (user + assistant per turn)
    assert!(
        len_after_2 > len_after_1,
        "History should grow after turn 2"
    );
    assert!(
        len_after_3 > len_after_2,
        "History should grow after turn 3"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 18. Tool call with stringified JSON arguments (common LLM pattern)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn native_dispatcher_handles_stringified_arguments() {
    let dispatcher = NativeToolDispatcher;
    let response = ChatResponse {
        usage: None,
        text: Some(String::new()),
        tool_calls: vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "hello"}"#.into(),
        }],
    };

    let (_, calls) = dispatcher.parse_response(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "echo");
    assert_eq!(
        calls[0].arguments.get("message").unwrap().as_str().unwrap(),
        "hello"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 19. XML dispatcher edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_handles_nested_json() {
    let response = ChatResponse {
        usage: None,
        text: Some(
            r#"<tool_call>
{"name": "file_write", "arguments": {"path": "test.json", "content": "{\"key\": \"value\"}"}}
</tool_call>"#
                .into(),
        ),
        tool_calls: vec![],
    };

    let dispatcher = XmlToolDispatcher;
    let (_, calls) = dispatcher.parse_response(&response);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "file_write");
    assert_eq!(
        calls[0].arguments.get("path").unwrap().as_str().unwrap(),
        "test.json"
    );
}

#[test]
fn xml_dispatcher_handles_empty_tool_call_tag() {
    let response = ChatResponse {
        usage: None,
        text: Some("<tool_call>\n</tool_call>\nSome text".into()),
        tool_calls: vec![],
    };

    let dispatcher = XmlToolDispatcher;
    let (text, calls) = dispatcher.parse_response(&response);
    assert!(calls.is_empty());
    assert!(text.contains("Some text"));
}

#[test]
fn xml_dispatcher_handles_unclosed_tool_call() {
    let response = ChatResponse {
        usage: None,
        text: Some("Before\n<tool_call>\n{\"name\": \"shell\"}".into()),
        tool_calls: vec![],
    };

    let dispatcher = XmlToolDispatcher;
    let (text, calls) = dispatcher.parse_response(&response);
    // Should not panic — just treat as text
    assert!(calls.is_empty());
    assert!(text.contains("Before"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 20. ConversationMessage serialization round-trip
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn conversation_message_serialization_roundtrip() {
    let messages = vec![
        ConversationMessage::Chat(ChatMessage::system("system")),
        ConversationMessage::Chat(ChatMessage::user("hello")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    for msg in &messages {
        let json = serde_json::to_string(msg).unwrap();
        let parsed: ConversationMessage = serde_json::from_str(&json).unwrap();

        // Verify the variant type matches
        match (msg, &parsed) {
            (ConversationMessage::Chat(a), ConversationMessage::Chat(b)) => {
                assert_eq!(a.role, b.role);
                assert_eq!(a.content, b.content);
            }
            (
                ConversationMessage::AssistantToolCalls {
                    text: a_text,
                    tool_calls: a_calls,
                },
                ConversationMessage::AssistantToolCalls {
                    text: b_text,
                    tool_calls: b_calls,
                },
            ) => {
                assert_eq!(a_text, b_text);
                assert_eq!(a_calls.len(), b_calls.len());
            }
            (ConversationMessage::ToolResults(a), ConversationMessage::ToolResults(b)) => {
                assert_eq!(a.len(), b.len());
            }
            _ => panic!("Variant mismatch after serialization"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 21. Tool dispatcher format_results
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_format_results_includes_status_and_output() {
    let dispatcher = XmlToolDispatcher;
    let results = vec![
        ToolExecutionResult {
            name: "shell".into(),
            output: "file1.txt\nfile2.txt".into(),
            success: true,
            tool_call_id: None,
        },
        ToolExecutionResult {
            name: "file_read".into(),
            output: "Error: file not found".into(),
            success: false,
            tool_call_id: None,
        },
    ];

    let msg = dispatcher.format_results(&results);
    let content = match msg {
        ConversationMessage::Chat(c) => c.content,
        _ => panic!("Expected Chat variant"),
    };

    assert!(content.contains("shell"));
    assert!(content.contains("file1.txt"));
    assert!(content.contains("ok"));
    assert!(content.contains("file_read"));
    assert!(content.contains("error"));
}

#[test]
fn native_format_results_maps_tool_call_ids() {
    let dispatcher = NativeToolDispatcher;
    let results = vec![
        ToolExecutionResult {
            name: "a".into(),
            output: "out1".into(),
            success: true,
            tool_call_id: Some("tc-001".into()),
        },
        ToolExecutionResult {
            name: "b".into(),
            output: "out2".into(),
            success: true,
            tool_call_id: Some("tc-002".into()),
        },
    ];

    let msg = dispatcher.format_results(&results);
    match msg {
        ConversationMessage::ToolResults(r) => {
            assert_eq!(r.len(), 2);
            assert_eq!(r[0].tool_call_id, "tc-001");
            assert_eq!(r[0].content, "out1");
            assert_eq!(r[1].tool_call_id, "tc-002");
            assert_eq!(r[1].content, "out2");
        }
        _ => panic!("Expected ToolResults"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 22. to_provider_messages conversion
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_converts_history_to_provider_messages() {
    let dispatcher = XmlToolDispatcher;
    let history = vec![
        ConversationMessage::Chat(ChatMessage::system("sys")),
        ConversationMessage::Chat(ChatMessage::user("hi")),
        ConversationMessage::AssistantToolCalls {
            text: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        },
        ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "ok".into(),
        }]),
        ConversationMessage::Chat(ChatMessage::assistant("done")),
    ];

    let messages = dispatcher.to_provider_messages(&history);

    // Should have: system, user, assistant (from tool calls), user (tool results), assistant
    assert!(messages.len() >= 4);
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[1].role, "user");
}

#[test]
fn native_dispatcher_converts_tool_results_to_tool_messages() {
    let dispatcher = NativeToolDispatcher;
    let history = vec![ConversationMessage::ToolResults(vec![
        ToolResultMessage {
            tool_call_id: "tc1".into(),
            content: "output1".into(),
        },
        ToolResultMessage {
            tool_call_id: "tc2".into(),
            content: "output2".into(),
        },
    ])];

    let messages = dispatcher.to_provider_messages(&history);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "tool");
    assert_eq!(messages[1].role, "tool");
}

// ═══════════════════════════════════════════════════════════════════════════
// 23. XML tool instructions generation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn xml_dispatcher_generates_tool_instructions() {
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let dispatcher = XmlToolDispatcher;
    let instructions = dispatcher.prompt_instructions(&tools);

    assert!(instructions.contains("## Tool Use Protocol"));
    assert!(instructions.contains("<tool_call>"));
    assert!(instructions.contains("echo"));
    assert!(instructions.contains("Echoes the input"));
}

#[test]
fn native_dispatcher_returns_empty_instructions() {
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(EchoTool)];
    let dispatcher = NativeToolDispatcher;
    let instructions = dispatcher.prompt_instructions(&tools);
    assert!(instructions.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════════
// 24. Clear history
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn clear_history_resets_conversation() {
    let provider = Box::new(ScriptedProvider::new(vec![
        text_response("first"),
        text_response("second"),
    ]));

    let mut agent = build_agent_with(provider, vec![], Box::new(NativeToolDispatcher));

    let _ = agent.turn("hi").await.unwrap();
    assert!(!agent.history().is_empty());

    agent.clear_history();
    assert!(agent.history().is_empty());

    // Next turn should re-inject system prompt
    let _ = agent.turn("hello again").await.unwrap();
    assert!(matches!(
        &agent.history()[0],
        ConversationMessage::Chat(c) if c.role == "system"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
// 21. Compaction stores nothing
// ═══════════════════════════════════════════════════════════════════════════

fn agent_with_security(provider: Box<dyn Provider>, mem: Arc<dyn Memory>) -> Agent {
    Agent::builder()
        .provider(provider)
        .tools(vec![])
        .memory(mem)
        .observer(make_observer())
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .security(Arc::new(crate::security::SecurityPolicy::default()))
        .build()
        .unwrap()
}

/// Seed enough history for `compute_split_index` to find a boundary.
async fn seed_history(agent: &mut Agent) {
    for i in 0..12 {
        let _ = agent.turn(&format!("turn {i}")).await;
    }
}

/// Compaction folds older turns into a summary and does nothing else: it asks
/// the model for no memory write, so a note is stored only when the person asked
/// for one. Exactly one request is made (the summary) and nothing is stored.
#[tokio::test]
async fn compaction_asks_for_a_summary_and_stores_nothing() {
    let (mem, _tmp) = make_sqlite_memory();
    let mut script: Vec<ChatResponse> = (0..12).map(|_| text_response("ok")).collect();
    script.push(text_response("## Summary\nsummarised"));

    let provider = Arc::new(ScriptedProvider::new(script));
    let mut agent = agent_with_security(Box::new(SharedProvider(provider.clone())), mem.clone());
    seed_history(&mut agent).await;
    let requests_before = provider.request_count();

    crate::memory::MEMORY_VIEW
        .scope(
            crate::memory::MemoryView::All,
            agent.compact_streaming(4, None),
        )
        .await
        .unwrap();

    assert_eq!(
        provider.request_count() - requests_before,
        1,
        "compaction made a request besides the summary"
    );
    assert!(
        mem.list(None, None).await.unwrap().is_empty(),
        "compaction stored a note nobody asked for"
    );
}
