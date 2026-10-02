//! A channel runtime for tests in other modules that need the owner prompt a
//! daemon would send over a real workspace: the gateway, the TUI and the CLI
//! each delete a note, and the next owner turn must not carry it.

use super::dispatch::process_channel_message;
use super::test_support::{dispatch_ctx, RecordingChannel, ReplyAndPromptProvider, OWNER_SENDER};
use super::*;
use std::sync::Arc;

/// One runtime over `workspace`, with the named owner in a direct chat. The
/// owner prompt is built per turn from the workspace files, as the daemon
/// builds it, and the provider keeps the system prompt of every turn.
///
/// The environment guards hold for the life of the value: the lock on the
/// process environment, a throwaway home and config directory, and an audit
/// directory. Fields drop in declaration order, so the runtime goes first and
/// the guards that put the variables back go last.
pub(crate) struct OwnerDm {
    ctx: Arc<ChannelRuntimeContext>,
    provider: Arc<ReplyAndPromptProvider>,
    next_message: std::sync::atomic::AtomicUsize,
    _config_dir: tempfile::TempDir,
    _home: tempfile::TempDir,
    _profile_env: crate::test_env::EnvGuard,
    _config_dir_env: crate::test_env::EnvGuard,
    _home_env: crate::test_env::HomeGuard,
    _audit: crate::test_env::EnvGuard,
    _lock: crate::test_env::EnvAuditRedirect,
}

impl OwnerDm {
    pub(crate) async fn start(workspace: &std::path::Path) -> Self {
        let (lock, audit) = crate::test_env::redirect_audit_temp().await;
        let home = tempfile::TempDir::new().expect("temp home");
        let config_dir = tempfile::TempDir::new().expect("temp config dir");
        let home_env = crate::test_env::HomeGuard::set(home.path());
        let config_dir_env =
            crate::test_env::EnvGuard::set("RANTAICLAW_CONFIG_DIR", config_dir.path());
        let profile_env = crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "owner-dm");

        let provider = Arc::new(ReplyAndPromptProvider {
            reply: "ok".to_string(),
            system_prompts: std::sync::Mutex::new(Vec::new()),
        });
        let channel: Arc<dyn Channel> = Arc::new(RecordingChannel::default());
        let mut ctx = dispatch_ctx(
            vec![channel],
            provider.clone(),
            routing::RuntimeConfigSlot::default(),
        );
        {
            let inner = Arc::get_mut(&mut ctx).expect("the context is not shared yet");
            inner.workspace_dir = Arc::new(workspace.to_path_buf());
            inner.approval_owners = Arc::new(vec![OWNER_SENDER.to_string()]);
            inner.owner_prompt = owner_prompt_builder(
                workspace.to_path_buf(),
                "owner-dm-model".to_string(),
                Vec::new(),
                crate::config::IdentityConfig::default(),
                None,
                false,
                crate::config::SkillsPromptInjectionMode::Full,
                Arc::new(Vec::new()),
            );
        }
        Self {
            ctx,
            provider,
            next_message: std::sync::atomic::AtomicUsize::new(0),
            _config_dir: config_dir,
            _home: home,
            _profile_env: profile_env,
            _config_dir_env: config_dir_env,
            _home_env: home_env,
            _audit: audit,
            _lock: lock,
        }
    }

    /// Runs one message from the owner through dispatch and returns the system
    /// prompt the provider received for it.
    pub(crate) async fn turn(&self) -> String {
        let id = self
            .next_message
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        process_channel_message(
            Arc::clone(&self.ctx),
            traits::ChannelMessage {
                sender_aliases: Vec::new(),
                id: format!("owner-dm-{id}"),
                sender: OWNER_SENDER.to_string(),
                reply_target: "owner-dm-chat".to_string(),
                content: "hello".to_string(),
                channel: "test-channel".to_string(),
                timestamp: 1,
                thread_ts: None,
                reply_anchor: None,
                is_direct: true,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        self.provider
            .system_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .cloned()
            .expect("the provider saw the turn")
    }
}
