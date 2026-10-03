//! Shared fixture for the tests that drive a door end to end: a local
//! OpenAI-compatible server the door's provider talks to, and a workspace with
//! owner files and notes in every tier of the store.
//!
//! The door tests assert on what the model is sent. They do not assert on a
//! helper: the door builds its own provider from a config that names the local
//! server, so the request the server records is the request a real provider
//! would have received.

use crate::config::Config;
use crate::memory::{Memory, MemoryCategory, SqliteMemory};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

/// One word per stored note, so a test can tell which note reached the model.
pub(crate) const SHARED_WORD: &str = "saffronquartz";
pub(crate) const CHAT_WORD: &str = "juniperquartz";
pub(crate) const CRON_WORD: &str = "cinnamonquartz";
pub(crate) const OTHER_WORD: &str = "marigoldquartz";

/// Content only the owner's workspace files carry.
pub(crate) const USER_FILE_CANARY: &str = "owner-profile-canary-41c9";
pub(crate) const MEMORY_FILE_CANARY: &str = "owner-notes-canary-52d0";
/// A workspace file that is not memory. A prompt that carries it shows the
/// files were read, so the absence of the two above is a decision and not an
/// empty fixture.
pub(crate) const SOUL_FILE_CANARY: &str = "bot-soul-canary-63e1";

/// A local OpenAI-compatible chat server that records every request body.
pub(crate) struct MockLlm {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct MockState {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    reply: &'static str,
}

async fn answer(State(state): State<MockState>, body: Bytes) -> Response {
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let streaming = request
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    state
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(request);

    if streaming {
        let chunk = serde_json::json!({
            "choices": [{ "index": 0, "delta": { "content": state.reply }, "finish_reason": null }]
        });
        let finish = serde_json::json!({
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
        });
        let body = format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response();
    }
    let completion = serde_json::json!({
        "id": "mock-completion",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": state.reply },
            "finish_reason": "stop"
        }]
    });
    axum::Json(completion).into_response()
}

/// The text of a message's `content`, whether it is a string or a list of parts.
fn content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl MockLlm {
    pub(crate) async fn start(reply: &'static str) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().fallback(answer).with_state(MockState {
            requests: Arc::clone(&requests),
            reply,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock server binds a local port");
        let addr = listener.local_addr().expect("the bound address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            requests,
            addr,
            task,
        }
    }

    /// The `default_provider` value that points a door at this server.
    pub(crate) fn provider(&self) -> String {
        format!("custom:http://{}", self.addr)
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Waits until `count` requests arrived, or panics after a few seconds.
    pub(crate) async fn wait_for_requests(&self, count: usize) {
        for _ in 0..400 {
            if self.request_count() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "the mock server saw {} of {count} requests",
            self.request_count()
        );
    }

    fn messages(&self) -> Vec<(String, String)> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flat_map(|request| {
                request
                    .get("messages")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .map(|message| {
                let role = message
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let content = message.get("content").map(content_text).unwrap_or_default();
                (role, content)
            })
            .collect()
    }

    /// The system prompt of the last request the model was sent.
    pub(crate) fn last_system_text(&self) -> String {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .and_then(|request| request.get("messages")?.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("system")
            })
            .filter_map(|message| message.get("content").map(content_text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every system prompt the model was sent.
    pub(crate) fn system_text(&self) -> String {
        self.messages()
            .into_iter()
            .filter(|(role, _)| role == "system")
            .map(|(_, content)| content)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every message the model was sent except the system prompt: the user's
    /// turn with the memory context in front of it, and anything after.
    pub(crate) fn conversation_text(&self) -> String {
        self.messages()
            .into_iter()
            .filter(|(role, _)| role != "system")
            .map(|(_, content)| content)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Everything the model was sent.
    pub(crate) fn sent_text(&self) -> String {
        format!("{}\n{}", self.system_text(), self.conversation_text())
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The key a note is stored under for the chat a door test names.
pub(crate) const CHAT_KEY: &str = "telegram:chat-a";

/// A workspace for a door to run in. Owner files and a note in every tier of
/// the store are in place, and the environment points at temp directories for
/// as long as the fixture lives.
pub(crate) struct DoorFixture {
    pub(crate) config: Config,
    pub(crate) llm: MockLlm,
    pub(crate) workspace: std::path::PathBuf,
    // Fields drop in declaration order: the directories, then the environment
    // guards that put the variables back, and the lock last.
    _dirs: Vec<TempDir>,
    _config_dir_env: crate::test_env::EnvGuard,
    _home_env: crate::test_env::HomeGuard,
    _audit: crate::test_env::EnvGuard,
    _lock: crate::test_env::EnvAuditRedirect,
}

impl DoorFixture {
    pub(crate) async fn start() -> Self {
        let (lock, audit) = crate::test_env::redirect_audit_temp().await;
        let home = TempDir::new().expect("temp home");
        let config_dir = TempDir::new().expect("temp config dir");
        let workspace_dir = TempDir::new().expect("temp workspace");
        let home_env = crate::test_env::HomeGuard::set(home.path());
        let config_dir_env =
            crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", config_dir.path());

        let workspace = workspace_dir.path().to_path_buf();
        std::fs::write(
            workspace.join("USER.md"),
            format!("# User\n{USER_FILE_CANARY}"),
        )
        .unwrap();
        std::fs::write(
            workspace.join("MEMORY.md"),
            format!("# Memory\n{MEMORY_FILE_CANARY}"),
        )
        .unwrap();
        std::fs::write(
            workspace.join("SOUL.md"),
            format!("# Soul\n{SOUL_FILE_CANARY}"),
        )
        .unwrap();

        let memory = SqliteMemory::new(&workspace).expect("the notes database opens");
        for (key, word, scope) in [
            ("shared_note", SHARED_WORD, None),
            ("chat_note", CHAT_WORD, Some(CHAT_KEY)),
            ("cron_note", CRON_WORD, Some("cron:door-job")),
            ("other_note", OTHER_WORD, Some("telegram:chat-other")),
        ] {
            memory
                .store(
                    key,
                    &format!("A note about the lantern: {word}"),
                    MemoryCategory::Core,
                    scope,
                )
                .await
                .expect("a note is stored");
        }
        drop(memory);

        let llm = MockLlm::start("Done.").await;
        let config = Config {
            workspace_dir: workspace.clone(),
            config_path: config_dir.path().join("config.toml"),
            default_provider: Some(llm.provider()),
            default_model: Some("mock-model".to_string()),
            api_key: Some("mock-key".to_string()),
            memory: crate::config::MemoryConfig {
                min_relevance_score: 0.0,
                ..crate::config::MemoryConfig::default()
            },
            ..Config::default()
        };
        Self {
            config,
            llm,
            workspace,
            _dirs: vec![home, config_dir, workspace_dir],
            _config_dir_env: config_dir_env,
            _home_env: home_env,
            _audit: audit,
            _lock: lock,
        }
    }

    /// Asserts the model was sent every note: all of memory was readable.
    pub(crate) fn assert_every_note_was_sent(&self) {
        let sent = self.llm.conversation_text();
        for word in [SHARED_WORD, CHAT_WORD, CRON_WORD, OTHER_WORD] {
            assert!(sent.contains(word), "{word} is missing:\n{sent}");
        }
        let system = self.llm.system_text();
        for canary in [USER_FILE_CANARY, MEMORY_FILE_CANARY, SOUL_FILE_CANARY] {
            assert!(system.contains(canary), "{canary} is missing:\n{system}");
        }
    }

    /// Asserts the system prompt of the last request carries the owner's files.
    /// A door that rebuilds its prompt for a session that already has turns is
    /// judged on this request, not on the first one.
    pub(crate) fn assert_last_system_prompt_carries_the_owner_files(&self) {
        let system = self.llm.last_system_text();
        for canary in [USER_FILE_CANARY, MEMORY_FILE_CANARY, SOUL_FILE_CANARY] {
            assert!(
                system.contains(canary),
                "{canary} is missing from the last system prompt:\n{system}"
            );
        }
    }

    /// Asserts the model was sent none of the notes and none of the files that
    /// are memory, and was sent the workspace file that is not.
    pub(crate) fn assert_no_note_was_sent(&self) {
        self.assert_only_notes_were_sent(&[]);
    }

    /// Asserts the model was sent exactly the notes in `words`, and neither
    /// `USER.md` nor `MEMORY.md`, which are memory read into the prompt.
    pub(crate) fn assert_only_notes_were_sent(&self, words: &[&str]) {
        assert!(self.llm.request_count() > 0, "the door sent no request");
        let sent = self.llm.sent_text();
        for word in [SHARED_WORD, CHAT_WORD, CRON_WORD, OTHER_WORD] {
            if words.contains(&word) {
                assert!(
                    self.llm.conversation_text().contains(word),
                    "control: {word} should have been read:\n{sent}"
                );
            } else {
                assert!(!sent.contains(word), "{word} reached the model:\n{sent}");
            }
        }
        for canary in [USER_FILE_CANARY, MEMORY_FILE_CANARY] {
            assert!(
                !sent.contains(canary),
                "{canary} reached a prompt that does not see all of memory:\n{sent}"
            );
        }
        let system = self.llm.system_text();
        assert!(
            system.contains(SOUL_FILE_CANARY),
            "control: the prompt reads the workspace files:\n{system}"
        );
    }
}

/// The registry a door builds from `config`: `all_tools_with_runtime` over the
/// same arguments the CLI door passes. A test reads what each tool says about
/// itself from here and compares it with what the door's prompt says.
pub(crate) fn registry_for(config: &Config) -> Vec<Box<dyn crate::tools::Tool>> {
    let runtime: Arc<dyn crate::runtime::RuntimeAdapter> = Arc::from(
        crate::runtime::create_runtime(&config.runtime).expect("the native runtime builds"),
    );
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
    let memory: Arc<dyn Memory> = Arc::from(
        crate::memory::create_memory_with_storage(
            &config.memory,
            &config.workspace_dir,
            config.api_key.as_deref(),
        )
        .expect("the notes database opens"),
    );
    crate::tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        memory,
        None,
        None,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        config,
    )
}

/// The names of the `## Tools` list in `prompt`, in order. The listing is names
/// only after plan 531 (each request describes each tool once, in the tool's
/// own words, so the descriptions reach the model via native specs or the XML
/// tool-instruction block, not the `## Tools` block).
pub(crate) fn tools_section_entries(prompt: &str) -> Vec<(String, String)> {
    let Some((_, rest)) = prompt.split_once("## Tools\n\n") else {
        return Vec::new();
    };
    let section = rest.split("\n## ").next().unwrap_or(rest).trim_end();
    section
        .lines()
        .filter_map(|line| {
            let name = line.strip_prefix("- **")?.strip_suffix("**")?;
            Some((name.to_string(), String::new()))
        })
        .collect()
}

/// What a registry says about its tools: `(name, description)` per tool, read
/// from the tools themselves.
pub(crate) fn held_by(registry: &[Box<dyn crate::tools::Tool>]) -> Vec<(&str, &str)> {
    registry
        .iter()
        .map(|tool| (tool.name(), tool.description()))
        .collect()
}

/// Asserts that `prompt` lists exactly the tools in `held`: the set of names is
/// equal in both directions. The `## Tools` block is names only after plan
/// 531; description equality is asserted against the tool specs (native
/// providers) or the XML tool-instruction block (non-native providers) by the
/// door tests, not against this section.
pub(crate) fn assert_prompt_lists(door: &str, prompt: &str, held: &[(&str, &str)]) {
    let listed = tools_section_entries(prompt);
    let mut listed_names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
    listed_names.sort_unstable();
    let mut held_names: Vec<&str> = held.iter().map(|(name, _)| *name).collect();
    held_names.sort_unstable();
    assert_eq!(
        listed_names, held_names,
        "{door}: the prompt names a different set of tools than the registry holds"
    );
}
